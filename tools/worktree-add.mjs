#!/usr/bin/env node
// worktree-add: create a ticket worktree OUTSIDE the repository.
//
//   node tools/worktree-add.mjs <ticket>
//   node tools/worktree-add.mjs <ticket> --base origin/main --branch feat/<ticket>
//   node tools/worktree-add.mjs <ticket> --print
//   node tools/worktree-add.mjs <ticket> --force   # skip the ticket check (hotfix)
//
// The default root is <repo>/.worktrees/<ticket>: inside the repository (the
// sandbox grants read/write there) but git-ignored, so it never shows up in
// `git status`. A worktree under <repo>/target/ is a trap: `cargo clean`
// deletes the whole target/ directory, worktrees included. SVCD_WT_ROOT
// overrides the root; it is rejected only when it lands under target/.

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
  const args = { ticket: "", base: "origin/main", branch: "", print: false, force: false };
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
    } else if (arg === "--force") {
      args.force = true;
    } else if (arg.startsWith("-")) {
      fail("unknown argument " + arg);
    } else if (!args.ticket) {
      args.ticket = arg;
    } else {
      fail("unexpected argument " + arg);
    }
  }
  if (!args.ticket) {
    fail("usage: node tools/worktree-add.mjs <ticket> [--base <ref>] [--branch <name>] [--print] [--force]");
  }
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(args.ticket)) {
    fail("ticket must be a plain id (letters, digits, . _ -): " + args.ticket);
  }
  if (!args.branch) args.branch = "feat/" + args.ticket;
  return args;
}

/// A ticket mismatch is the expensive failure (#174 hit another session's
/// ticket): refuse a missing/closed ticket and print what #N actually is.
function checkTicket(ticket, force) {
  let raw;
  try {
    raw = execFileSync(
      "gh",
      ["issue", "view", ticket, "--json", "state,title,labels"],
      { cwd: mainRoot, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    );
  } catch (error) {
    if (error.code === "ENOENT") {
      console.log("worktree-add: warning: gh not found; ticket #" + ticket + " not checked");
      return;
    }
    if (force) {
      console.log("worktree-add: ticket #" + ticket + " not found (--force)");
      return;
    }
    fail(
      "ticket #" + ticket + " not found; create it first (deck_issue_create or gh issue create) or pass --force",
    );
  }
  const info = JSON.parse(raw);
  const labels = (info.labels ?? []).map((label) => label.name).join(", ");
  if (info.state === "CLOSED" && !force) {
    fail("ticket #" + ticket + " is closed (" + info.title + "); pass --force for a hotfix");
  }
  console.log("ticket: #" + ticket + " " + info.title + (labels ? " [" + labels + "]" : ""));
}

const args = parseArgs(process.argv.slice(2));
// Resolve the main worktree, not this checkout: a helper run from a linked
// worktree must still put the next worktree in the same external root.
const commonDir = git(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
const mainRoot = dirname(commonDir);
const root = process.env.SVCD_WT_ROOT
  ? resolve(mainRoot, process.env.SVCD_WT_ROOT)
  : join(mainRoot, ".worktrees");
// Only target/ is a trap now: `cargo clean` removes it wholesale. Anything
// else under the repo is git-ignored by /.worktrees/.
const targetRoot = join(mainRoot, "target");
const underTarget = relative(targetRoot, root);
if (underTarget === "" || (!underTarget.startsWith("..") && !isAbsolute(underTarget))) {
  fail("worktree root must not be under " + targetRoot + " (cargo clean deletes it)");
}
const worktree = join(root, args.ticket);
if (args.print) {
  console.log(worktree);
  process.exit(0);
}
if (existsSync(worktree)) fail("worktree path already exists: " + worktree);
checkTicket(args.ticket, args.force);
mkdirSync(root, { recursive: true });
execFileSync("git", ["worktree", "add", worktree, "-b", args.branch, args.base], {
  cwd: mainRoot,
  stdio: "inherit",
});
console.log("worktree: " + worktree);
console.log("branch:   " + args.branch);
