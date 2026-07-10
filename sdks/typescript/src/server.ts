/** The accept loop: WebSocket server speaking `siphon-ai.v1`. */

import { createServer, type IncomingMessage, type Server } from "node:http";

import { WebSocketServer, type WebSocket } from "ws";

import { Call } from "./call.js";
import { parseEvent, type Start } from "./events.js";

export const SUBPROTOCOL = "siphon-ai.v1";

/** The daemon sends `start` immediately after the upgrade. */
const START_DEADLINE_MS = 10_000;

export type CallHandler = (call: Call) => Promise<void> | void;

export interface SiphonServerOptions {
  host?: string;
  port?: number;
  /** When set, upgrade requests must carry `Authorization: Bearer <token>`
   * (rejected with 401 otherwise) — matching `[bridge].auth_bearer`. */
  authToken?: string;
}

/**
 * Accepts SiphonAI bridge connections and hands each to a handler.
 *
 * ```ts
 * const server = new SiphonServer(async (call) => {
 *   for await (const item of call) {
 *     if (item.type === "audio") call.sendAudioFrame(item.pcm); // echo
 *   }
 * });
 * await server.listen();
 * ```
 *
 * Serves `GET /healthz` → 200 for container probes. Handler exceptions
 * are logged and close that call only.
 */
export class SiphonServer {
  private readonly host: string;
  private readonly port: number;
  private readonly authToken?: string;
  private http: Server | null = null;

  constructor(
    private readonly handler: CallHandler,
    options: SiphonServerOptions = {},
  ) {
    this.host = options.host ?? "0.0.0.0";
    this.port = options.port ?? 8080;
    this.authToken = options.authToken;
  }

  /** Bind and start accepting; resolves once listening. */
  async listen(): Promise<void> {
    const http = createServer((req, res) => {
      if (req.url === "/healthz") {
        res.writeHead(200, { "content-type": "text/plain" });
        res.end("ok\n");
        return;
      }
      res.writeHead(404);
      res.end();
    });
    const wss = new WebSocketServer({
      server: http,
      handleProtocols: (protocols) =>
        protocols.has(SUBPROTOCOL) ? SUBPROTOCOL : false,
      maxPayload: 256 * 1024, // PROTOCOL.md §2 text-frame cap
      verifyClient: ({ req }: { req: IncomingMessage }) =>
        this.authToken === undefined ||
        req.headers.authorization === `Bearer ${this.authToken}`,
    });
    wss.on("connection", (ws: WebSocket) => this.connection(ws));
    this.http = http;
    await new Promise<void>((resolve) =>
      http.listen(this.port, this.host, resolve),
    );
  }

  /** The port actually bound (useful with `port: 0`). */
  address(): { port: number } {
    const addr = this.http?.address();
    if (addr === null || addr === undefined || typeof addr === "string") {
      throw new Error("server is not listening");
    }
    return { port: addr.port };
  }

  async close(): Promise<void> {
    const http = this.http;
    this.http = null;
    if (http !== null) {
      await new Promise<void>((resolve) => http.close(() => resolve()));
    }
  }

  private connection(ws: WebSocket): void {
    const deadline = setTimeout(() => {
      console.warn("siphon-ai-server: connection closed before `start`");
      ws.close(1002, "expected start");
    }, START_DEADLINE_MS);

    ws.once("message", (data: Buffer, isBinary: boolean) => {
      clearTimeout(deadline);
      if (isBinary) {
        ws.close(1002, "expected start");
        return;
      }
      let start: Start;
      try {
        const event = parseEvent(data);
        if (event.type !== "start") {
          ws.close(1002, "expected start");
          return;
        }
        start = event;
      } catch {
        ws.close(1002, "expected start");
        return;
      }
      if (start.version !== "1") {
        console.error(
          `siphon-ai-server: unsupported protocol version ${start.version}`,
        );
        ws.close(1003, "unsupported version");
        return;
      }

      const call = new Call(ws, start);
      Promise.resolve(this.handler(call))
        .catch((err) => {
          console.error(
            `siphon-ai-server: handler failed for call ${start.call_id}:`,
            err,
          );
        })
        .finally(() => {
          call.audioOut.close();
          if (ws.readyState === ws.OPEN) ws.close();
        });
    });
  }
}
