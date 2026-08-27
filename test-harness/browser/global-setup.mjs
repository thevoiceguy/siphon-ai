import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { startStack, HERE } from "./stack.mjs";

/**
 * Boot the lab once for the whole run and hand the tests its ports.
 *
 * Playwright has no supported way to pass objects from global setup to
 * tests, so the stack description goes through a file — the same trick
 * the teardown uses to find what to stop.
 */
export default async function globalSetup() {
  const stack = await startStack();
  writeFileSync(
    join(HERE, ".stack.json"),
    JSON.stringify(
      {
        work: stack.work,
        ports: stack.ports,
        pageUrl: stack.pageUrl,
        toneReport: stack.toneReport,
        logs: stack.logs,
      },
      null,
      2,
    ),
  );
  globalThis.__labStack = stack;
  console.log(`lab up: page ${stack.pageUrl}  logs ${stack.work}`);
  return async () => {
    await stack.stop();
  };
}
