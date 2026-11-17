#!/usr/bin/env node
// agent-preflight: one compact view of branch/worktree/remote state. The
// default is one line plus actionable warnings; --all lists every worktree.
//
//   node tools/agent-preflight.mjs [--fetch] [--all] [--strict]
//   node tools/agent-preflight.mjs --json

import { execFileSync } from "node:child_process";
import { dirname, isAbsolute, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

function parseArgs(argv) {
  const args = { fetch: false, strict: false, all: false, json: false };
  for (const arg of argv) {
    if (arg === "--fetch") args.fetch = true;
    else if (arg === "--strict") args.strict = true;
    else if (arg === "--all") args.all = true;
    else if (arg === "--json") args.json = true;
    else {
      console.error("agent-preflight: unknown argument " + arg);
      process.exit(2);
    }
  }
  return args;
}

function git(args, allowFailure = false) {
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

const args = parseArgs(process.argv.slice(2));
if (args.fetch) {
  try {
    git(["fetch", "--quiet", "origin"]);
  } catch {
    console.error("agent-preflight: fetch failed; using local refs");
  }
}
const branch = git(["rev-parse", "--abbrev-ref", "HEAD"]);
const head = git(["rev-parse", "--short", "HEAD"]);
const dirty = git(["status", "--porcelain"]).split("\n").filter(Boolean).length;
const counts = git(["rev-list", "--left-right", "--count", "HEAD...origin/main"], true);
const [ahead, behind] = counts ? counts.split(/\s+/).map(Number) : [0, 0];
const trees = worktrees();
// Worktrees under the main checkout's target/ are deleted by `cargo clean`.
// Resolve the main checkout from the shared git dir, so this warns from any
// linked worktree too.
const commonDir = git(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
const mainRoot = dirname(commonDir);
const targetRoot = resolve(mainRoot, "target");
const targetWorktrees = trees.filter((tree) => {
  const inside = relative(targetRoot, resolve(tree.path));
  return inside !== "" && !inside.startsWith("..") && !isAbsolute(inside);
});
const warnings = [];
if (branch === "main") {
  warnings.push("on main: create an external worktree with node tools/worktree-add.mjs <ticket> before editing");
}
if (behind > 0) warnings.push("HEAD is " + behind + " commit(s) behind origin/main");
if (branch === "main" && dirty > 0) warnings.push("shared main is dirty: do not reset/clean it");
if (branch === "main" && trees.length > 1) warnings.push(trees.length - 1 + " other worktree(s) exist");
if (targetWorktrees.length > 0) {
  warnings.push(
    targetWorktrees.length +
      " worktree(s) under target/: cargo clean deletes them; move them outside the repo (git worktree move / node tools/worktree-add.mjs <ticket>)",
  );
}
if (args.strict && (branch === "main" || targetWorktrees.length > 0)) process.exitCode = 1;

if (args.json) {
  console.log(
    JSON.stringify(
      {
        branch,
        head,
        dirty,
        ahead,
        behind,
        worktrees: trees,
        targetWorktrees: targetWorktrees.map((tree) => tree.path),
        warnings,
      },
      null,
      2,
    ),
  );
} else {
  console.log("branch " + branch + " @ " + head + " | origin/main +" + ahead + " -" + behind + " | dirty " + dirty + " | worktrees " + trees.length);
  if (args.all) {
    for (const tree of trees) {
      const danger = targetWorktrees.includes(tree) ? "  [under target!]" : "";
      console.log("  " + tree.path + " [" + (tree.branch || "detached") + "]" + danger);
    }
  }
  for (const warning of warnings) console.log("warning: " + warning);
}
