#!/usr/bin/env node
// merge-guard: show how stale a ticket branch is before merging it into main,
// and which files both sides touched (the conflict/loss surface).
//
// Usage:
//   node tools/merge-guard.mjs feat/119-blob-upload
//   node tools/merge-guard.mjs feat/119-blob-upload --base origin/main --strict

import { execFileSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

function git(args) {
  return execFileSync("git", args, { cwd: REPO, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

function main() {
  const argv = process.argv.slice(2);
  let branch;
  let base = "origin/main";
  let strict = false;
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] === "--base") base = argv[(index += 1)];
    else if (argv[index] === "--strict") strict = true;
    else if (!branch) branch = argv[index];
    else {
      console.error(`merge-guard: unknown argument ${argv[index]}`);
      process.exit(2);
    }
  }
  if (!branch) {
    console.error("usage: node tools/merge-guard.mjs <branch> [--base origin/main] [--strict]");
    process.exit(2);
  }
  const mergeBase = git(["merge-base", base, branch]);
  const baseCommits = git(["rev-list", "--count", `${mergeBase}..${base}`]);
  const branchCommits = git(["rev-list", "--count", `${mergeBase}..${branch}`]);
  const mainFiles = new Set(
    git(["diff", "--name-only", `${mergeBase}..${base}`]).split("\n").filter(Boolean),
  );
  const branchFiles = git(["diff", "--name-only", `${mergeBase}..${branch}`])
    .split("\n")
    .filter(Boolean);
  const overlap = branchFiles.filter((file) => mainFiles.has(file));
  console.log(`merge-guard: ${branch} -> ${base}`);
  console.log(`  merge-base ${mergeBase.slice(0, 12)}`);
  console.log(`  base moved ${baseCommits} commit(s) since the base`);
  console.log(`  branch has ${branchCommits} commit(s)`);
  console.log(`  both sides touched ${overlap.length} file(s)`);
  for (const file of overlap.slice(0, 40)) console.log(`    overlap ${file}`);
  if (overlap.length > 40) console.log(`    ... ${overlap.length - 40} more`);
  if (Number(baseCommits) > 0) {
    console.log("  the branch is stale: merge/rebase main, rerun the full suite, and check handoff markers");
  }
  if (strict && (Number(baseCommits) > 0 || overlap.length > 0)) process.exit(1);
}

main();
