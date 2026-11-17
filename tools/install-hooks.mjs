#!/usr/bin/env node
// install-hooks: point this clone at .githooks and make the hook executable.
// Per-clone config (core.hooksPath), so CI and other clones are unaffected.
//
// Usage: node tools/install-hooks.mjs [--uninstall]

import { chmodSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");
const uninstall = process.argv.includes("--uninstall");

function git(args) {
  return execFileSync("git", args, { cwd: REPO, encoding: "utf8" });
}

if (uninstall) {
  try {
    git(["config", "--unset", "core.hooksPath"]);
  } catch {}
  console.log("install-hooks: core.hooksPath unset");
} else {
  git(["config", "core.hooksPath", ".githooks"]);
  if (process.platform !== "win32") {
    chmodSync(join(REPO, ".githooks", "pre-commit"), 0o755);
  }
  console.log("install-hooks: core.hooksPath = .githooks");
}
