#!/usr/bin/env node
// agent-preflight: one read-only view of the worktree/branch state that keeps
// biting parallel sessions. Run it before editing and before merging.
//
// Usage:
//   node tools/agent-preflight.mjs
//   node tools/agent-preflight.mjs --fetch
//   node tools/agent-preflight.mjs --strict   exit 1 on a main-worktree checkout
//   node tools/agent-preflight.mjs --json

import { execFileSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

function parseArgs(argv) {
  const args = { fetch: false, strict: false, json: false };
  for (const arg of argv) {
    if (arg === "--fetch") args.fetch = true;
    else if (arg === "--strict") args.strict = true;
    else if (arg === "--json") args.json = true;
    else {
      console.error(`agent-preflight: unknown argument ${arg}`);
      process.exit(2);
    }
  }
  return args;
}

function git(args, { allowFailure = false } = {}) {
  try {
    return execFileSync("git", args, { cwd: REPO, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
  } catch (error) {
    if (allowFailure) return "";
    throw error;
  }
}

function worktrees() {
  const raw = git(["worktree", "list", "--porcelain"]);
  const entries = [];
  let current = {};
  for (const line of raw.split("\n")) {
    if (line.startsWith("worktree ")) current.path = line.slice("worktree ".length);
    else if (line.startsWith("branch ")) current.branch = line.slice("branch refs/heads/".length);
    else if (line === "detached") current.branch = "(detached)";
    else if (line === "") {
      if (current.path) entries.push(current);
      current = {};
    }
  }
  if (current.path) entries.push(current);
  return entries;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.fetch) {
    try {
      git(["fetch", "--quiet", "origin"]);
    } catch {
      console.error("agent-preflight: fetch failed; continuing with the local refs");
    }
  }
  const branch = git(["rev-parse", "--abbrev-ref", "HEAD"]);
  const head = git(["rev-parse", "--short", "HEAD"]);
  const status = git(["status", "--porcelain"]);
  const dirty = status ? status.split("\n").filter(Boolean) : [];
  const aheadBehind = git(["rev-list", "--left-right", "--count", `HEAD...origin/main`], { allowFailure: true });
  const [ahead, behind] = aheadBehind ? aheadBehind.split(/\s+/).map(Number) : [0, 0];
  const trees = worktrees();
  const warnings = [];
  if (branch === "main" && dirty.length > 0) {
    warnings.push("main is checked out and dirty; implementation work belongs in a worktree");
  }
  if (branch === "main") {
    warnings.push("current branch is main; create target/wt-<ticket> from origin/main before editing");
  }
  if (behind > 0) {
    warnings.push(`HEAD is ${behind} commit(s) behind origin/main; rebase/merge before merging`);
  }
  const siblings = trees.filter((tree) => tree.path && resolve(tree.path) !== REPO);
  if (branch === "main" && siblings.length > 0) {
    warnings.push("other worktrees exist; shared main is read-only for implementation");
  }
  const report = {
    branch,
    head,
    dirty: dirty.length,
    ahead,
    behind,
    worktrees: trees,
    warnings,
  };
  if (args.json) {
    console.log(JSON.stringify(report, null, 2));
  } else {
    console.log(`branch ${branch} @ ${head}`);
    console.log(`origin/main: ahead ${ahead}, behind ${behind}`);
    console.log(`dirty files: ${dirty.length}`);
    for (const tree of trees) {
      console.log(`  worktree ${tree.path} [${tree.branch ?? "detached"}]`);
    }
    for (const warning of warnings) console.log(`warning: ${warning}`);
  }
  if (args.strict && branch === "main") process.exit(1);
}

main();
