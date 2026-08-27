// Boots the lab the browser tests dial into, and tears it down again.
//
// Deliberately a Node twin of `examples/browser-sip/headless-check.sh`
// rather than a call into it: Playwright needs the stack up *before*
// any project runs and the ports back as data, and the bash harness is
// a self-contained pass/fail check. The two share the lab config and
// the page, which is where drift would actually hurt.

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import {
  createReadStream,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import net from "node:net";

export const HERE = dirname(fileURLToPath(import.meta.url));
export const REPO_ROOT = resolve(HERE, "../..");
const LAB = join(REPO_ROOT, "examples/browser-sip");

/** An OS-chosen free port, so parallel runs and busy boxes both work. */
async function freePort() {
  return new Promise((res, rej) => {
    const srv = net.createServer();
    srv.on("error", rej);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => res(port));
    });
  });
}

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
};

/**
 * Serve the (port-templated) page, plus `/vendor/sip.js/…` straight
 * out of `node_modules`.
 *
 * Vendored rather than CDN-loaded on purpose: a nightly job that fails
 * because jsdelivr had a bad minute teaches nobody anything, and the
 * interop matrix wants the *same* stack version across engines.
 */
function pageServer(pageDir, port) {
  const roots = [
    { prefix: "/vendor/sip.js/", dir: join(HERE, "node_modules/sip.js/") },
    { prefix: "/", dir: pageDir },
  ];
  const server = createServer((req, res) => {
    const url = new URL(req.url, "http://127.0.0.1");
    let path = decodeURIComponent(url.pathname);
    if (path.endsWith("/")) path += "index.html";
    for (const { prefix, dir } of roots) {
      if (!path.startsWith(prefix)) continue;
      const file = normalize(join(dir, path.slice(prefix.length)));
      if (!file.startsWith(normalize(dir))) break; // no ../ escapes
      if (existsSync(file) && statSync(file).isFile()) {
        res.writeHead(200, {
          "content-type": MIME[extname(file)] ?? "application/octet-stream",
        });
        createReadStream(file).pipe(res);
        return;
      }
    }
    res.writeHead(404).end("not found");
  });
  return new Promise((res) =>
    server.listen(port, "127.0.0.1", () => res(server)),
  );
}

async function waitForMetrics(port, deadlineMs) {
  const until = Date.now() + deadlineMs;
  while (Date.now() < until) {
    try {
      const r = await fetch(`http://127.0.0.1:${port}/metrics`);
      if (r.ok) return true;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  return false;
}

/**
 * Start daemon + tone server + page server. Returns everything the
 * tests need, and a `stop()` that leaves the logs behind.
 */
export async function startStack() {
  const daemon = process.env.DAEMON_BIN ?? join(REPO_ROOT, "target/debug/siphon-ai");
  if (!existsSync(daemon)) {
    throw new Error(
      `no daemon at ${daemon} — build it first:\n` +
        "  cargo build -p siphon-ai --features webrtc",
    );
  }
  if (!existsSync(join(LAB, "certs/wss-key.pem"))) {
    throw new Error(`no WSS cert — run ${join(LAB, "gen-cert.sh")}`);
  }

  const work = join(
    process.env.RUNNER_TEMP ?? "/tmp",
    `browser-interop.${process.pid}`,
  );
  mkdirSync(join(work, "page"), { recursive: true });

  const ports = {
    sip: Number(process.env.SIP_PORT ?? (await freePort())),
    wss: Number(process.env.WSS_PORT ?? (await freePort())),
    page: Number(process.env.PAGE_PORT ?? (await freePort())),
    obs: Number(process.env.OBS_PORT ?? (await freePort())),
    admin: Number(process.env.ADMIN_PORT ?? (await freePort())),
    ws: Number(process.env.TONE_PORT ?? (await freePort())),
  };

  // Same substitutions the bash harness makes, for the same reason:
  // the committed lab.toml stays the readable reference.
  const config = readFileSync(join(LAB, "lab.toml"), "utf8")
    .replaceAll("127.0.0.1:5070", `127.0.0.1:${ports.sip}`)
    .replaceAll("127.0.0.1:8443", `127.0.0.1:${ports.wss}`)
    .replaceAll("127.0.0.1:8088", `127.0.0.1:${ports.page}`)
    .replaceAll("localhost:8088", `localhost:${ports.page}`)
    .replaceAll("127.0.0.1:9091", `127.0.0.1:${ports.obs}`)
    .replaceAll("127.0.0.1:9092", `127.0.0.1:${ports.admin}`)
    .replaceAll("127.0.0.1:8765", `127.0.0.1:${ports.ws}`)
    .replaceAll("examples/browser-sip/certs", join(LAB, "certs"))
    .replaceAll("examples/browser-sip/cdr.jsonl", join(work, "cdr.jsonl"));
  const configPath = join(work, "lab.toml");
  writeFileSync(configPath, config);

  // The page dials the WSS port from a literal, so it needs the same
  // treatment (a lesson the bash harness learned the hard way).
  writeFileSync(
    join(work, "page/index.html"),
    readFileSync(join(LAB, "index.html"), "utf8").replaceAll(
      "127.0.0.1:8443",
      `127.0.0.1:${ports.wss}`,
    ),
  );

  const logs = {};
  const children = [];
  const spawnLogged = (name, cmd, args) => {
    const out = join(work, `${name}.log`);
    logs[name] = out;
    const fd = openSync(out, "a");
    const child = spawn(cmd, args, { stdio: ["ignore", fd, fd] });
    children.push(child);
    return child;
  };

  const toneReport = join(work, "tone-report.json");
  spawnLogged("tone-ws", process.execPath, [
    join(HERE, "tone-ws-server.mjs"),
    "--port",
    String(ports.ws),
    "--report",
    toneReport,
  ]);
  spawnLogged("daemon", daemon, ["--config", configPath]);
  const http = await pageServer(join(work, "page"), ports.page);

  if (!(await waitForMetrics(ports.obs, 20_000))) {
    const log = existsSync(join(work, "daemon.log"))
      ? readFileSync(join(work, "daemon.log"), "utf8").slice(-2000)
      : "(no log)";
    throw new Error(`daemon never became ready. Tail:\n${log}`);
  }

  return {
    work,
    ports,
    toneReport,
    logs,
    pageUrl: `http://127.0.0.1:${ports.page}/`,
    async stop() {
      for (const c of children) c.kill("SIGTERM");
      await new Promise((r) => http.close(r));
      // Give the tone server a moment to flush its report.
      await new Promise((r) => setTimeout(r, 300));
      for (const c of children) c.kill("SIGKILL");
    },
  };
}
