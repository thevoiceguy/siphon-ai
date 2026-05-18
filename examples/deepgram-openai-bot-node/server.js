// server.js — SiphonAI bridge protocol v1 bot.
//
// Closed-loop voice agent:
//   caller speaks → Deepgram STT → OpenAI Chat (streaming) →
//   Deepgram TTS → caller hears.
//
// Differences from the FreeSWITCH `mod_audio_fork` model this was
// ported from:
//
//   - No ESL / `uuid_broadcast` / WAV files. SiphonAI streams audio
//     bidirectionally on the same WebSocket: binary frames in are
//     caller audio, binary frames out get pushed into the call.
//   - The first text message on the socket is `start` with the
//     audio format (`sample_rate`, `frame_ms`, `encoding`). We
//     read it, validate the contract (pcm16le / 20ms / 8 kHz or
//     16 kHz), and configure Deepgram to match.
//   - Outbound frames MUST be exactly 20 ms of PCM16 LE. We chunk
//     Deepgram TTS bytes into 320-byte (8 kHz) or 640-byte (16 kHz)
//     frames and pace them at real time so SiphonAI's 200 ms outbound
//     buffer doesn't queue.
//   - Barge-in: SiphonAI emits `speech_started` when the caller
//     starts talking. We send `clear` back, which drops anything
//     queued in the daemon's outbound buffer.
//
// Env:
//   DEEPGRAM_API_KEY        required
//   OPENAI_API_KEY          required
//   BOT_BIND                default `0.0.0.0:8080`
//   BOT_SYSTEM_PROMPT       optional override
//   BOT_GREETING            optional override

// Hide Node 22's built-in `globalThis.WebSocket` from the
// Deepgram SDK. The SDK detects native-WebSocket availability
// via `typeof WebSocket !== "undefined"` and, when true, opens
// its live client with `new WebSocket(url, ["token", apiKey])` —
// passing the API key as a Sec-WebSocket-Protocol value.
// Subprotocol tokens are RFC-2616 tokens (no whitespace, limited
// punctuation), so a real Deepgram API key fails the validation
// in both undici (`Invalid Sec-WebSocket-Protocol value`) and
// the `ws` package (`invalid or duplicated subprotocol`).
//
// Deleting the global flips the SDK's detection to the fallback
// branch, which `require('ws')` and authenticates via the
// `Authorization` header instead — the working path.
//
// MUST run before `require('@deepgram/sdk')` so the SDK's
// module-load-time `NATIVE_WEBSOCKET_AVAILABLE` constant sees
// the deletion.
delete globalThis.WebSocket;

const { WebSocketServer } = require('ws');
const { createClient } = require('@deepgram/sdk');
const OpenAI = require('openai');

// ─── Config ───────────────────────────────────────────────────────────────
const [BIND_HOST, BIND_PORT] = (process.env.BOT_BIND || '0.0.0.0:8080').split(':');

const DG_KEY = process.env.DEEPGRAM_API_KEY;

// LLM: any OpenAI-compatible chat-completions endpoint works (OpenAI,
// Groq, Together, OpenRouter, Fireworks, Anthropic's OAI-compat API,
// local Ollama, …). `BOT_LLM_BASE_URL` switches providers without
// touching code; `BOT_LLM_MODEL` picks the model name; the API key
// can come from either `BOT_LLM_API_KEY` (preferred for non-OpenAI
// providers) or the legacy `OPENAI_API_KEY` env var.
const LLM_BASE_URL  = process.env.BOT_LLM_BASE_URL || undefined;
const LLM_MODEL     = process.env.BOT_LLM_MODEL    || 'gpt-4o-mini';
const LLM_API_KEY   = process.env.BOT_LLM_API_KEY  || process.env.OPENAI_API_KEY;
const LLM_API_KEY_VAR = process.env.BOT_LLM_API_KEY ? 'BOT_LLM_API_KEY' : 'OPENAI_API_KEY';
const LLM_MAX_TOKENS  = process.env.BOT_LLM_MAX_TOKENS
  ? Number.parseInt(process.env.BOT_LLM_MAX_TOKENS, 10)
  : undefined;
const LLM_TEMPERATURE = process.env.BOT_LLM_TEMPERATURE
  ? Number.parseFloat(process.env.BOT_LLM_TEMPERATURE)
  : undefined;

/**
 * Validate an API key env var. Bails on missing OR
 * non-printable-ASCII values so a placeholder like `…` from the
 * docs (Unicode U+2026, common copy-paste hazard) fails here with
 * a clear message instead of three layers deeper inside the WS
 * library as `Invalid character in header content`. Real API
 * keys are ASCII tokens, so the check is sound.
 */
function requireKey(name, value) {
  if (!value) {
    console.error(`ERROR: ${name} not set.`);
    process.exit(1);
  }
  // RFC 9110 §5.5 visible-ASCII range (0x20–0x7E) covers every
  // real API key shape we've seen.
  if (!/^[\x20-\x7E]+$/.test(value)) {
    console.error(
      `ERROR: ${name} contains non-printable / non-ASCII characters. ` +
        `If you copy-pasted "…" or another placeholder from the docs, ` +
        `replace it with your real key.`,
    );
    process.exit(1);
  }
}
requireKey('DEEPGRAM_API_KEY', DG_KEY);
requireKey(LLM_API_KEY_VAR, LLM_API_KEY);

const SYSTEM_PROMPT =
  process.env.BOT_SYSTEM_PROMPT ||
  'You are a helpful voice agent. Keep responses brief and conversational — ' +
    'typically 1–2 sentences. Speak naturally as if on a phone call. ' +
    'Avoid markdown, lists, or formatting.';

const GREETING = process.env.BOT_GREETING || 'Hi there! How can I help you today?';

console.log(
  `[llm] model=${LLM_MODEL} base_url=${LLM_BASE_URL || '(OpenAI default)'} ` +
  `max_tokens=${LLM_MAX_TOKENS ?? '(provider default)'} ` +
  `temperature=${LLM_TEMPERATURE ?? '(provider default)'}`,
);

// SiphonAI's protocol pins these: pcm16le, mono, 20 ms frames,
// 8 kHz or 16 kHz. Anything else means a daemon misconfig or a
// future protocol bump — refuse the call rather than silently
// guessing.
const SUPPORTED_RATES = new Set([8000, 16000]);

// ─── Metrics ──────────────────────────────────────────────────────────────

/**
 * Per-call metric collector. Emits one log line per event with a
 * `+Nms` offset from call start, plus call-wide counters that
 * accumulate (barge-ins, clears, dropped frames). Designed to be
 * grep-friendly: every line starts with `metric `.
 *
 * Derived latencies (computed in `turn.finish()`):
 *   - user_stop_to_audio_ms  = first_outbound_frame_at − utterance_end_at
 *   - llm_first_token_ms     = llm_first_token_at      − llm_start_at
 *   - llm_completion_ms      = llm_completed_at        − llm_start_at
 *   - tts_first_byte_ms      = tts_first_byte_at       − tts_start_at
 *   - total_response_ms      = last_outbound_frame_at  − utterance_end_at
 */
function makeCallMetrics(log) {
  const callStart = Date.now();
  const counts = { bargeIn: 0, clear: 0, droppedFrames: 0 };
  let sttOpenAt = null;
  let firstUserAudioAt = null;

  function elapsed(when = Date.now()) {
    return when - callStart;
  }
  function emit(event, fields = {}) {
    const parts = [`metric ${event} +${elapsed()}ms`];
    for (const [k, v] of Object.entries(fields)) {
      if (v === null || v === undefined) continue;
      parts.push(`${k}=${v}`);
    }
    log(parts.join(' '));
  }

  return {
    elapsed,
    emit,
    markSttOpen() {
      if (sttOpenAt) return;
      sttOpenAt = Date.now();
      emit('stt_open');
    },
    markFirstUserAudio() {
      if (firstUserAudioAt) return;
      firstUserAudioAt = Date.now();
      emit('first_user_audio');
    },
    incBargeIn() {
      counts.bargeIn++;
      emit('barge_in', { count: counts.bargeIn });
    },
    incClear() {
      counts.clear++;
    },
    addDroppedFrames(n) {
      if (!n || n <= 0) return;
      counts.droppedFrames += n;
      emit('frames_dropped', { added: n, total: counts.droppedFrames });
    },
    snapshot() {
      return {
        callStart,
        sttOpenAt,
        firstUserAudioAt,
        bargeInCount: counts.bargeIn,
        clearCount: counts.clear,
        droppedFrameCount: counts.droppedFrames,
      };
    },
  };
}

/**
 * Per-turn metric tracker. A "turn" is either the greeting (no
 * preceding utterance — `startedAt` is the call start) or a
 * reply (started at UtteranceEnd). LLM + TTS + first/last
 * outbound frame timings are tracked through to `finish()`,
 * which emits a `turn_summary` line with derived latencies.
 */
function makeTurnMetrics(metrics, label) {
  const startedAt = Date.now();
  const t = {
    label,
    startedAt,
    llmStartAt: null,
    llmFirstTokenAt: null,
    llmCompletedAt: null,
    ttsStartAt: null,
    ttsFirstByteAt: null,
    firstOutboundFrameAt: null,
    lastOutboundFrameAt: null,
    framesSent: 0,
    maxQueueDepth: 0,
  };

  function diff(a, b) {
    return a !== null && b !== null ? a - b : null;
  }

  return {
    label,
    state: t,
    markLlmStart() {
      t.llmStartAt = Date.now();
      metrics.emit('llm_start', { turn: label });
    },
    markLlmFirstToken() {
      if (t.llmFirstTokenAt) return;
      t.llmFirstTokenAt = Date.now();
      metrics.emit('llm_first_token', {
        turn: label,
        latency_ms: diff(t.llmFirstTokenAt, t.llmStartAt),
      });
    },
    markLlmCompleted() {
      t.llmCompletedAt = Date.now();
      metrics.emit('llm_completed', {
        turn: label,
        latency_ms: diff(t.llmCompletedAt, t.llmStartAt),
      });
    },
    markTtsStart() {
      // Always emit the event (per-phrase visibility) but only
      // pin the turn-level ttsStartAt to the FIRST phrase's
      // request time. Otherwise the turn_summary's
      // tts_first_byte_ms diff is computed against the last
      // phrase's start, which goes negative once the first
      // phrase's audio has already been pumped.
      if (!t.ttsStartAt) t.ttsStartAt = Date.now();
      metrics.emit('tts_start', { turn: label });
    },
    markTtsFirstByte() {
      if (t.ttsFirstByteAt) return;
      t.ttsFirstByteAt = Date.now();
      metrics.emit('tts_first_byte', {
        turn: label,
        latency_ms: diff(t.ttsFirstByteAt, t.ttsStartAt),
      });
    },
    onFrameSent() {
      const now = Date.now();
      if (!t.firstOutboundFrameAt) {
        t.firstOutboundFrameAt = now;
        metrics.emit('first_outbound_frame', {
          turn: label,
          user_to_audio_ms: diff(t.firstOutboundFrameAt, t.startedAt),
        });
      }
      t.lastOutboundFrameAt = now;
      t.framesSent++;
    },
    observeQueueDepth(n) {
      if (n > t.maxQueueDepth) t.maxQueueDepth = n;
    },
    finish() {
      metrics.emit('turn_summary', {
        turn: label,
        user_to_audio_ms: diff(t.firstOutboundFrameAt, t.startedAt),
        llm_first_token_ms: diff(t.llmFirstTokenAt, t.llmStartAt),
        llm_completion_ms: diff(t.llmCompletedAt, t.llmStartAt),
        tts_first_byte_ms: diff(t.ttsFirstByteAt, t.ttsStartAt),
        total_response_ms: diff(t.lastOutboundFrameAt, t.startedAt),
        frames_sent: t.framesSent,
        max_queue_depth: t.maxQueueDepth,
      });
    },
  };
}

// ─── Audio plumbing ───────────────────────────────────────────────────────

/**
 * Frame size (bytes) for the given sample rate. 50 fps fixed.
 *  - 8000 Hz × 20 ms × 2 bytes = 320
 *  - 16000 Hz × 20 ms × 2 bytes = 640
 */
function bytesPerFrame(sampleRate) {
  return (sampleRate / 50) * 2;
}

/**
 * Streams pre-encoded PCM16 LE bytes back onto a WebSocket at
 * real-time pace (one frame per 20 ms), aligned to a sample
 * rate. The interval keeps a small dispatch queue so a bursty
 * upstream producer (Deepgram TTS dumping 200 ms at a time)
 * doesn't overflow the SiphonAI daemon's 200 ms outbound buffer.
 *
 * `cancel()` drops every pending frame — used by barge-in and
 * end-of-turn cleanup.
 */
function makePlayout(ws, sampleRate, log, hooks = {}) {
  const frameSize = bytesPerFrame(sampleRate);
  /** @type {Buffer[]} queued exactly-20ms frames. */
  let frames = [];
  /** @type {Buffer} partial frame across chunks. */
  let carry = Buffer.alloc(0);
  let timer = null;
  // Diagnostic counters — `playout summary` logs them on stop()
  // so a silent caller can be distinguished from a non-firing
  // pump. Reset on every start() so each TTS phrase reports its
  // own batch.
  let runSent = 0;
  let runErrors = 0;

  function pump() {
    const rs = ws.readyState;
    if (rs !== ws.OPEN) {
      log(`playout: ws not OPEN (readyState=${rs}), dropping ${frames.length} pending frames`);
      if (hooks.onCanceled && frames.length > 0) hooks.onCanceled(frames.length);
      frames = [];
      stop();
      return;
    }
    const f = frames.shift();
    if (!f) {
      stop();
      return;
    }
    try {
      ws.send(f);
      runSent++;
      if (hooks.onFrameSent) hooks.onFrameSent();
    } catch (e) {
      runErrors++;
      log('playout: ws.send threw:', e?.message || e);
    }
  }

  function start() {
    if (timer) return;
    runSent = 0;
    runErrors = 0;
    log('playout: pump started');
    // 20 ms cadence. setInterval is rough at the ms boundary; for
    // production-quality timing you'd run a monotonic clock loop
    // (see `forge-media`'s playout loop), but for a demo the OS
    // scheduler is plenty.
    timer = setInterval(pump, 20);
  }

  function stop() {
    if (timer) {
      clearInterval(timer);
      timer = null;
      log(`playout: pump stopped (sent=${runSent} errors=${runErrors} carry=${carry.length})`);
    }
  }

  return {
    /** Push raw PCM16 LE bytes (any length). They're carved into
     *  exact 20 ms frames; any remainder is held until the next push. */
    push(buf) {
      if (!buf || buf.length === 0) return;
      let work = carry.length ? Buffer.concat([carry, buf]) : buf;
      const usable = work.length - (work.length % frameSize);
      for (let i = 0; i < usable; i += frameSize) {
        frames.push(work.subarray(i, i + frameSize));
      }
      carry = work.subarray(usable);
      if (hooks.observeQueueDepth) hooks.observeQueueDepth(frames.length);
      start();
    },

    /** Drop the queue and reset partial frame. Called on barge-in
     *  and on turn end. The daemon's outbound buffer keeps its
     *  own state; pair with a `clear` to flush that too. */
    cancel() {
      const dropped = frames.length + (carry.length > 0 ? 1 : 0);
      if (hooks.onCanceled && dropped > 0) hooks.onCanceled(dropped);
      frames = [];
      carry = Buffer.alloc(0);
      stop();
    },

    /** True iff frames are still pending (or in flight). */
    isActive() {
      return frames.length > 0 || timer !== null;
    },

    /** Flush a final short frame (zero-padded) so trailing audio
     *  isn't lost on turn end. Optional — most TTS will end on a
     *  20 ms boundary anyway. */
    flush() {
      if (carry.length > 0) {
        const padded = Buffer.alloc(frameSize);
        carry.copy(padded);
        frames.push(padded);
        carry = Buffer.alloc(0);
        start();
      }
    },
  };
}

// ─── Per-call session ─────────────────────────────────────────────────────

async function handleCall(ws, req) {
  const remote = req.socket.remoteAddress;
  let callId = '(no-call-id-yet)';
  const log = (...args) => console.log(`[${callId}]`, ...args);

  // The `start` text message is the contract: the daemon picks
  // sample_rate from the negotiated codec and we mirror it on
  // every leg downstream. Refuse the call if we can't speak the
  // format the daemon settled on.
  let sampleRate = null;
  let playout = null;

  // Per-call metrics. Per-turn metrics live on `currentTurn` and
  // get rotated on each new speakStreaming sequence (greeting,
  // each reply). Playout hooks dispatch frame-sent events to the
  // active turn so latency math (utterance_end → first audio)
  // is computed without threading callbacks down through TTS.
  const metrics = makeCallMetrics(log);
  let currentTurn = null;
  // Flips when the WS closes. Used to short-circuit in-flight
  // LLM / TTS pipelines so we don't generate a flood of
  // "ws not OPEN, dropping frames" lines after the caller hangs up.
  let callEnded = false;

  // ─── Deepgram STT (built lazily, once we have the sample rate) ───
  const deepgram = createClient(DG_KEY);
  const openai = new OpenAI({
    apiKey: LLM_API_KEY,
    ...(LLM_BASE_URL ? { baseURL: LLM_BASE_URL } : {}),
  });
  let dgStt = null;
  let dgSttReady = false;

  // Conversation state.
  const conversation = [{ role: 'system', content: SYSTEM_PROMPT }];
  let speaking = false;
  let pendingUtterance = null;
  let utteranceBuf = '';
  let sawFirstInterimThisUtterance = false;
  let sawFirstFinalThisUtterance = false;

  async function openDeepgramStt() {
    dgStt = deepgram.listen.live({
      model: 'nova-3',
      encoding: 'linear16',
      sample_rate: sampleRate,
      channels: 1,
      punctuate: true,
      interim_results: true,
      // `endpointing` (ms of trailing silence before a "final"
      // transcript chunk fires) is safe to drop well below the
      // Deepgram default (10–300 are all accepted). 150 ms shaves
      // perceived latency without splitting "Hello [pause] there"
      // into two utterances.
      endpointing: 150,
      // `utterance_end_ms` has a HARD MINIMUM of 1000 enforced by
      // Deepgram — values below that get the connection rejected
      // with a 400 at handshake time. Leave at 1000 unless
      // Deepgram raises the floor.
      utterance_end_ms: 1000,
      vad_events: true,
      smart_format: false,
    });
    dgStt.on('open', () => {
      log(`STT open at ${sampleRate} Hz`);
      metrics.markSttOpen();
      dgSttReady = true;
    });
    dgStt.on('error', (e) => {
      log('STT error:', e?.message || e);
      // If STT never opened, the call is unusable — caller would
      // hear the greeting and then dead air forever. Close the
      // WS so the daemon ends the call instead of stranding the
      // caller. (If STT was open and dies mid-call, dgSttReady
      // is already true and we leave the call up so any
      // in-flight LLM/TTS finishes.)
      if (!dgSttReady) {
        log('STT failed before opening — closing WS so daemon tears the call down');
        try { ws.close(1011, 'STT failed'); } catch {}
      }
    });
    dgStt.on('close', () => log('STT closed'));
    dgStt.on('Results', onSttResults);
    dgStt.on('UtteranceEnd', onUtteranceEnd);
  }

  async function onSttResults(data) {
    const transcript = data?.channel?.alternatives?.[0]?.transcript;
    if (!transcript) return;
    if (data.is_final) {
      if (!sawFirstFinalThisUtterance) {
        sawFirstFinalThisUtterance = true;
        metrics.emit('first_final_transcript');
      }
      utteranceBuf += (utteranceBuf ? ' ' : '') + transcript;
      log(`(final fragment): "${transcript}"`);
    } else {
      if (!sawFirstInterimThisUtterance) {
        sawFirstInterimThisUtterance = true;
        metrics.emit('first_interim_transcript');
      }
      log(`interim: "${transcript}"`);
    }
  }

  async function onUtteranceEnd() {
    if (!utteranceBuf.trim()) return;
    const u = utteranceBuf.trim();
    utteranceBuf = '';
    sawFirstInterimThisUtterance = false;
    sawFirstFinalThisUtterance = false;
    metrics.emit('utterance_end');
    log(`UTTERANCE: "${u}"`);
    if (speaking) {
      // Defer until we're done talking; barge-in handles the
      // interruption case separately.
      log('  (queued — agent still speaking)');
      pendingUtterance = u;
      return;
    }
    // Start a new turn at the moment the utterance ends — that's
    // the t=0 for the "user-stop-to-agent-audio" latency.
    currentTurn = makeTurnMetrics(metrics, 'reply');
    await handleUserTurn(u, currentTurn);
    currentTurn.finish();
    while (pendingUtterance && !speaking) {
      const next = pendingUtterance;
      pendingUtterance = null;
      log(`processing queued: "${next}"`);
      currentTurn = makeTurnMetrics(metrics, 'reply');
      await handleUserTurn(next, currentTurn);
      currentTurn.finish();
    }
    currentTurn = null;
  }

  // ─── TTS streaming → playout ───
  async function speakStreaming(text, turn) {
    return new Promise((resolve) => {
      // No-op if the call already ended — generates audio that
      // can't go anywhere and floods the log with frame drops.
      if (callEnded) { resolve(); return; }
      let collected = 0;
      if (turn) turn.markTtsStart();
      const tts = deepgram.speak.live({
        model: 'aura-2-thalia-en',
        encoding: 'linear16',
        sample_rate: sampleRate,
      });
      // NB: the SDK uses *different* casing for STT vs TTS event
      // names — `LiveTranscriptionEvents` is lowercase
      // (`'open'`/`'close'`/`'error'`) but `LiveTTSEvents` is
      // PascalCase (`'Open'`/`'Close'`/`'Error'`). Match the TTS
      // enum exactly; lowercase listeners silently never fire.
      tts.on('Open', () => {
        // `Speak`+`Flush` pattern: synthesize one phrase, close.
        tts.send(JSON.stringify({ type: 'Speak', text }));
        tts.send(JSON.stringify({ type: 'Flush' }));
      });
      tts.on('Error', (e) => {
        log('TTS error:', e?.message || e);
        // Don't reject — the outer turn loop treats a missed
        // phrase as silence and continues. Rejecting here would
        // abort the entire turn for a transient TTS hiccup.
        resolve();
      });
      tts.on('Audio', (chunk) => {
        if (callEnded) return;
        // chunk can be Buffer, Uint8Array, or ArrayBuffer depending on
        // transport. Normalise to Buffer.
        const buf =
          Buffer.isBuffer(chunk)
            ? chunk
            : chunk instanceof ArrayBuffer
              ? Buffer.from(new Uint8Array(chunk))
              : Buffer.from(chunk);
        if (turn) turn.markTtsFirstByte();
        collected += buf.length;
        playout.push(buf);
      });
      tts.on('Flushed', () => {
        log(`TTS Flushed: ${collected} bytes`);
        playout.flush();
        try { tts.requestClose(); } catch { /* ignore */ }
        resolve();
      });
      tts.on('Close', () => {
        // If we get close without Flushed (e.g. mid-barge-in), still
        // resolve so the caller doesn't hang.
        resolve();
      });
    });
  }

  // ─── Conversation turn ───
  //
  // LLM → TTS is pipelined: the moment a phrase completes from the
  // LLM stream (sentence punctuation or a long enough comma chunk),
  // we kick off its speakStreaming() against a serial chain. The LLM
  // continues streaming in parallel; subsequent phrases queue behind
  // the first. The caller hears the first sentence as soon as TTS
  // has rendered it, rather than waiting for the whole LLM response.
  async function handleUserTurn(userText, turn) {
    speaking = true;
    conversation.push({ role: 'user', content: userText });
    let fullResponse = '';
    let phraseBuf = '';
    // Serial TTS chain: every phrase appended waits for the
    // previous to finish, so playout order matches LLM order.
    let ttsChain = Promise.resolve();

    function speakPhrase(p) {
      if (!p) return;
      ttsChain = ttsChain.then(async () => {
        if (!speaking) return; // barge-in or WS close mid-stream
        await speakStreaming(p, turn);
      });
    }

    try {
      if (turn) turn.markLlmStart();
      const stream = await openai.chat.completions.create({
        model: LLM_MODEL,
        messages: conversation,
        stream: true,
        ...(LLM_MAX_TOKENS  != null ? { max_tokens:  LLM_MAX_TOKENS  } : {}),
        ...(LLM_TEMPERATURE != null ? { temperature: LLM_TEMPERATURE } : {}),
      });
      for await (const chunk of stream) {
        const delta = chunk.choices?.[0]?.delta?.content;
        if (!delta) continue;
        if (turn) turn.markLlmFirstToken();
        phraseBuf += delta;
        fullResponse += delta;
        // Phrase-level chunking: hand off to TTS the moment we
        // have a complete clause. The first phrase typically
        // arrives a few hundred ms after first_token; that's the
        // perceived "agent starts speaking" moment, not
        // llm_completed.
        if (
          /[.!?]$/.test(phraseBuf.trim()) ||
          (/,$/.test(phraseBuf.trim()) && phraseBuf.length > 40)
        ) {
          speakPhrase(phraseBuf.trim());
          phraseBuf = '';
        }
      }
      if (turn) turn.markLlmCompleted();
      if (phraseBuf.trim()) speakPhrase(phraseBuf.trim());

      // Wait for all phrases to finish synthesising.
      await ttsChain;

      // Wait for the playout pipe to drain before declaring the
      // turn done — the LLM finishing isn't the same as the
      // caller having heard the response.
      while (playout.isActive() && speaking) {
        await new Promise((r) => setTimeout(r, 50));
      }
      conversation.push({ role: 'assistant', content: fullResponse });
      // Trim context to keep token usage bounded.
      const MAX_TURNS = 10;
      while (conversation.length > MAX_TURNS * 2 + 1) {
        conversation.splice(1, 2);
      }
    } catch (e) {
      log('turn error:', e?.message || e);
    } finally {
      speaking = false;
    }
  }

  // ─── Wire events from SiphonAI ───
  ws.on('message', (data, isBinary) => {
    if (isBinary) {
      metrics.markFirstUserAudio();
      if (dgSttReady) {
        try {
          dgStt.send(data);
        } catch (e) {
          log('STT send error (drop):', e?.message || e);
        }
      }
      return;
    }
    // Text frame — control message.
    let msg;
    try {
      msg = JSON.parse(data.toString());
    } catch {
      return;
    }
    const t = msg.type;
    if (t === 'start') {
      callId = msg.call_id || '(unknown)';
      const rate = msg.audio?.sample_rate;
      const encoding = msg.audio?.encoding;
      const frameMs = msg.audio?.frame_ms;
      log(
        `START from=${msg.from} to=${msg.to} ` +
          `audio=${encoding}@${rate}Hz/${frameMs}ms`,
      );
      if (encoding !== 'pcm16le' || frameMs !== 20 || !SUPPORTED_RATES.has(rate)) {
        log(`refusing call: unsupported audio format`);
        try { ws.close(1000); } catch {}
        return;
      }
      sampleRate = rate;
      playout = makePlayout(ws, sampleRate, log, {
        onFrameSent: () => { if (currentTurn) currentTurn.onFrameSent(); },
        onCanceled: (n) => metrics.addDroppedFrames(n),
        observeQueueDepth: (n) => { if (currentTurn) currentTurn.observeQueueDepth(n); },
      });
      openDeepgramStt().then(async () => {
        // Greet after a brief settle so the daemon's first inbound
        // frames don't crash into our outbound.
        await new Promise((r) => setTimeout(r, 200));
        speaking = true;
        // Greeting is a "turn" too — t=0 is the call start, so
        // user_to_audio_ms here measures call-answer-to-greeting.
        currentTurn = makeTurnMetrics(metrics, 'greeting');
        await speakStreaming(GREETING, currentTurn);
        while (playout.isActive()) {
          await new Promise((r) => setTimeout(r, 50));
        }
        currentTurn.finish();
        currentTurn = null;
        speaking = false;
      });
    } else if (t === 'speech_started') {
      // Barge-in: caller began talking while we were. Drop the
      // pending playout locally and tell the daemon to flush
      // its outbound buffer too.
      if (playout?.isActive()) {
        log('barge-in: dropping playout + sending clear');
        metrics.incBargeIn();
        playout.cancel();
        try {
          ws.send(JSON.stringify({ type: 'clear', call_id: callId }));
          metrics.incClear();
        } catch {}
        speaking = false;
      }
    } else if (t === 'speech_stopped' || t === 'mark') {
      // Informational. The bot loop already keys off STT
      // `UtteranceEnd` rather than `speech_stopped`, but this
      // makes the bot resilient if STT is restarted.
    } else if (t === 'dtmf') {
      log(`DTMF: ${msg.digit} (${msg.method})`);
    } else if (t === 'hold') {
      log(`peer on hold (direction=${msg.direction}); pausing playout`);
      playout?.cancel();
      speaking = false;
    } else if (t === 'resume') {
      log('peer resumed; ready for next utterance');
    } else if (t === 'stop') {
      log(`STOP reason=${msg.reason}`);
      // Cleanup happens on `close` below.
    } else if (t === 'error') {
      log(`daemon error code=${msg.code} message=${msg.message}`);
    }
  });

  ws.on('close', (code, reason) => {
    const r = reason ? Buffer.from(reason).toString('utf8') : '';
    log(`WS closed (code=${code} reason=${JSON.stringify(r)})`);
    callEnded = true;
    speaking = false;
    try { dgStt?.requestClose(); } catch {}
    playout?.cancel();
    // Final call-wide tally. Useful for: how many times did the
    // caller cut us off? did we leak frames anywhere?
    const s = metrics.snapshot();
    metrics.emit('call_summary', {
      duration_ms: metrics.elapsed(),
      barge_in_count: s.bargeInCount,
      clear_count: s.clearCount,
      dropped_frame_count: s.droppedFrameCount,
    });
  });
  ws.on('error', (e) => log('WS error:', e?.message || e));
}

// ─── Server ──────────────────────────────────────────────────────────────
const wss = new WebSocketServer({
  host: BIND_HOST,
  port: parseInt(BIND_PORT, 10),
  // The handshake echo. SiphonAI advertises `siphon-ai.v1`;
  // honoring it is recommended.
  handleProtocols: (protos) => (protos.has('siphon-ai.v1') ? 'siphon-ai.v1' : false),
});

wss.on('listening', () => {
  console.log(`siphon-ai bot listening on ws://${BIND_HOST}:${BIND_PORT}/`);
});

wss.on('connection', (ws, req) => {
  const remote = req.socket?.remoteAddress;
  const subproto = ws.protocol || '(none)';
  console.log(`[ws] connection from ${remote} subprotocol=${subproto}`);
  handleCall(ws, req).catch((err) => {
    console.error('handleCall threw:', err);
    try { ws.close(); } catch {}
  });
});
