#!/usr/bin/env node
// worktree-add: create a ticket worktree OUTSIDE the repository.
//
//   node tools/worktree-add.mjs <ticket>
//   node tools/worktree-add.mjs <ticket> --base origin/main --branch feat/<ticket>
//   node tools/worktree-add.mjs <ticket> --print
//
// The default root is <repo-parent>/<repo-name>.worktrees/<ticket>, next to the
// repository. A worktree under <repo>/target/ is a trap: `cargo clean` deletes
// the whole target/ directory, worktrees included. SVCD_WT_ROOT overrides the
// root, but it must still resolve outside the repository.

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

function fail(message) {
  console.error("worktree-add: " + message);
  process.exit(2);
}

function git(args) {
  return execFileSync("git", args, {
    cwd: HERE,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

function parseArgs(argv) {
  const args = { ticket: "", base: "origin/main", branch: "", print: false };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--base") {
      args.base = argv[(index += 1)];
      if (!args.base) fail("--base needs a value");
    } else if (arg === "--branch") {
      args.branch = argv[(index += 1)];
      if (!args.branch) fail("--branch needs a value");
    } else if (arg === "--print") {
      args.print = true;
    } else if (arg.startsWith("-")) {
      fail("unknown argument " + arg);
    } else if (!args.ticket) {
      args.ticket = arg;
    } else {
      fail("unexpected argument " + arg);
    }
  }
  if (!args.ticket) {
    fail("usage: node tools/worktree-add.mjs <ticket> [--base <ref>] [--branch <name>] [--print]");
  }
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(args.ticket)) {
    fail("ticket must be a plain id (letters, digits, . _ -): " + args.ticket);
  }
  if (!args.branch) args.branch = "feat/" + args.ticket;
  return args;
}

const args = parseArgs(process.argv.slice(2));
// Resolve the main worktree, not this checkout: a helper run from a linked
// worktree must still put the next worktree in the same external root.
const commonDir = git(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
const mainRoot = dirname(commonDir);
const root = process.env.SVCD_WT_ROOT
  ? resolve(mainRoot, process.env.SVCD_WT_ROOT)
  : join(dirname(mainRoot), basename(mainRoot) + ".worktrees");
const relativeRoot = relative(mainRoot, root);
if (relativeRoot === "" || (!relativeRoot.startsWith("..") && !isAbsolute(relativeRoot))) {
  fail(
    "worktree root must be outside the repo (got " +
      root +
      "); a worktree under the repo can be deleted by cargo clean",
  );
}
const worktree = join(root, args.ticket);
if (args.print) {
  console.log(worktree);
  process.exit(0);
}
if (existsSync(worktree)) fail("worktree path already exists: " + worktree);
mkdirSync(root, { recursive: true });
execFileSync("git", ["worktree", "add", worktree, "-b", args.branch, args.base], {
  cwd: mainRoot,
  stdio: "inherit",
});
console.log("worktree: " + worktree);
console.log("branch:   " + args.branch);
