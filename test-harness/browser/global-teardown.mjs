/**
 * `globalSetup` returns its own teardown, which Playwright runs; this
 * file exists so a run interrupted between the two still says where
 * the logs are rather than leaving the operator to guess.
 */
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { HERE } from "./stack.mjs";

export default async function globalTeardown() {
  const path = join(HERE, ".stack.json");
  if (!existsSync(path)) return;
  const stack = JSON.parse(readFileSync(path, "utf8"));
  console.log(`lab logs kept in ${stack.work}`);
}
