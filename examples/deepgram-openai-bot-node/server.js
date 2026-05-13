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

const { WebSocketServer } = require('ws');
const { createClient } = require('@deepgram/sdk');
const OpenAI = require('openai');

// ─── Config ───────────────────────────────────────────────────────────────
const [BIND_HOST, BIND_PORT] = (process.env.BOT_BIND || '0.0.0.0:8080').split(':');

const DG_KEY = process.env.DEEPGRAM_API_KEY;
const OPENAI_KEY = process.env.OPENAI_API_KEY;
if (!DG_KEY) {
  console.error('ERROR: DEEPGRAM_API_KEY not set.');
  process.exit(1);
}
if (!OPENAI_KEY) {
  console.error('ERROR: OPENAI_API_KEY not set.');
  process.exit(1);
}

const SYSTEM_PROMPT =
  process.env.BOT_SYSTEM_PROMPT ||
  'You are a helpful voice agent. Keep responses brief and conversational — ' +
    'typically 1–2 sentences. Speak naturally as if on a phone call. ' +
    'Avoid markdown, lists, or formatting.';

const GREETING = process.env.BOT_GREETING || 'Hi there! How can I help you today?';

// SiphonAI's protocol pins these: pcm16le, mono, 20 ms frames,
// 8 kHz or 16 kHz. Anything else means a daemon misconfig or a
// future protocol bump — refuse the call rather than silently
// guessing.
const SUPPORTED_RATES = new Set([8000, 16000]);

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
function makePlayout(ws, sampleRate, log) {
  const frameSize = bytesPerFrame(sampleRate);
  /** @type {Buffer[]} queued exactly-20ms frames. */
  let frames = [];
  /** @type {Buffer} partial frame across chunks. */
  let carry = Buffer.alloc(0);
  let timer = null;

  function pump() {
    if (ws.readyState !== ws.OPEN) {
      stop();
      return;
    }
    const f = frames.shift();
    if (!f) {
      stop();
      return;
    }
    ws.send(f);
  }

  function start() {
    if (timer) return;
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
      start();
    },

    /** Drop the queue and reset partial frame. Called on barge-in
     *  and on turn end. The daemon's outbound buffer keeps its
     *  own state; pair with a `clear` to flush that too. */
    cancel() {
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

  // ─── Deepgram STT (built lazily, once we have the sample rate) ───
  const deepgram = createClient(DG_KEY);
  const openai = new OpenAI({ apiKey: OPENAI_KEY });
  let dgStt = null;
  let dgSttReady = false;

  // Conversation state.
  const conversation = [{ role: 'system', content: SYSTEM_PROMPT }];
  let speaking = false;
  let pendingUtterance = null;
  let utteranceBuf = '';

  async function openDeepgramStt() {
    dgStt = deepgram.listen.live({
      model: 'nova-3',
      encoding: 'linear16',
      sample_rate: sampleRate,
      channels: 1,
      punctuate: true,
      interim_results: true,
      endpointing: 300,
      utterance_end_ms: 1000,
      vad_events: true,
      smart_format: false,
    });
    dgStt.on('open', () => {
      log(`STT open at ${sampleRate} Hz`);
      dgSttReady = true;
    });
    dgStt.on('error', (e) => log('STT error:', e?.message || e));
    dgStt.on('close', () => log('STT closed'));
    dgStt.on('Results', onSttResults);
    dgStt.on('UtteranceEnd', onUtteranceEnd);
  }

  async function onSttResults(data) {
    const transcript = data?.channel?.alternatives?.[0]?.transcript;
    if (!transcript) return;
    if (data.is_final) {
      utteranceBuf += (utteranceBuf ? ' ' : '') + transcript;
      log(`(final fragment): "${transcript}"`);
    } else {
      log(`interim: "${transcript}"`);
    }
  }

  async function onUtteranceEnd() {
    if (!utteranceBuf.trim()) return;
    const u = utteranceBuf.trim();
    utteranceBuf = '';
    log(`UTTERANCE: "${u}"`);
    if (speaking) {
      // Defer until we're done talking; barge-in handles the
      // interruption case separately.
      log('  (queued — agent still speaking)');
      pendingUtterance = u;
      return;
    }
    await handleUserTurn(u);
    while (pendingUtterance && !speaking) {
      const next = pendingUtterance;
      pendingUtterance = null;
      log(`processing queued: "${next}"`);
      await handleUserTurn(next);
    }
  }

  // ─── TTS streaming → playout ───
  async function speakStreaming(text) {
    return new Promise((resolve, reject) => {
      let collected = 0;
      const tts = deepgram.speak.live({
        model: 'aura-2-thalia-en',
        encoding: 'linear16',
        sample_rate: sampleRate,
      });
      tts.on('open', () => {
        // `Speak`+`Flush` pattern: synthesize one phrase, close.
        tts.send(JSON.stringify({ type: 'Speak', text }));
        tts.send(JSON.stringify({ type: 'Flush' }));
      });
      tts.on('error', (e) => {
        log('TTS error:', e?.message || e);
        reject(e);
      });
      // SDK v4 emits binary audio chunks via the `Audio` event;
      // metadata (Flushed, Speaker) is delivered via Metadata.
      tts.on('Audio', (chunk) => {
        // chunk can be Buffer, Uint8Array, or ArrayBuffer depending on
        // transport. Normalise to Buffer.
        const buf =
          Buffer.isBuffer(chunk)
            ? chunk
            : chunk instanceof ArrayBuffer
              ? Buffer.from(new Uint8Array(chunk))
              : Buffer.from(chunk);
        collected += buf.length;
        playout.push(buf);
      });
      tts.on('Flushed', () => {
        log(`TTS Flushed: ${collected} bytes`);
        playout.flush();
        try { tts.requestClose(); } catch { /* ignore */ }
        resolve();
      });
      tts.on('close', () => {
        // If we get close without Flushed (e.g. mid-barge-in), still
        // resolve so the caller doesn't hang.
        resolve();
      });
    });
  }

  // ─── Conversation turn ───
  async function handleUserTurn(userText) {
    speaking = true;
    conversation.push({ role: 'user', content: userText });
    let fullResponse = '';
    let phraseBuf = '';
    const phrases = [];

    try {
      const stream = await openai.chat.completions.create({
        model: 'gpt-4o-mini',
        messages: conversation,
        stream: true,
      });
      for await (const chunk of stream) {
        const delta = chunk.choices?.[0]?.delta?.content;
        if (!delta) continue;
        phraseBuf += delta;
        fullResponse += delta;
        // Phrase-level chunking: TTS each clause as it lands so
        // the first audio plays well before the LLM finishes.
        if (
          /[.!?]$/.test(phraseBuf.trim()) ||
          (/,$/.test(phraseBuf.trim()) && phraseBuf.length > 40)
        ) {
          phrases.push(phraseBuf.trim());
          phraseBuf = '';
        }
      }
      if (phraseBuf.trim()) phrases.push(phraseBuf.trim());

      for (const p of phrases) {
        if (!speaking) break; // barge-in killed us
        await speakStreaming(p);
      }

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
      playout = makePlayout(ws, sampleRate, log);
      openDeepgramStt().then(async () => {
        // Greet after a brief settle so the daemon's first inbound
        // frames don't crash into our outbound.
        await new Promise((r) => setTimeout(r, 200));
        speaking = true;
        await speakStreaming(GREETING);
        while (playout.isActive()) {
          await new Promise((r) => setTimeout(r, 50));
        }
        speaking = false;
      });
    } else if (t === 'speech_started') {
      // Barge-in: caller began talking while we were. Drop the
      // pending playout locally and tell the daemon to flush
      // its outbound buffer too.
      if (playout?.isActive()) {
        log('barge-in: dropping playout + sending clear');
        playout.cancel();
        try {
          ws.send(JSON.stringify({ type: 'clear', call_id: callId }));
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

  ws.on('close', () => {
    log('WS closed');
    try { dgStt?.requestClose(); } catch {}
    playout?.cancel();
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
  handleCall(ws, req).catch((err) => {
    console.error('handleCall threw:', err);
    try { ws.close(); } catch {}
  });
});
