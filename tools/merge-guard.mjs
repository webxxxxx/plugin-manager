#!/usr/bin/env node
// merge-guard: one-line staleness/overlap summary before merging a branch.
// Default prints at most five overlap files; --all prints every file.
//
//   node tools/merge-guard.mjs feat/119-blob-upload [--base origin/main] [--all] [--strict] [--ci]
//
// --ci reads the latest `test` workflow on main (read-only; it never starts a
// run). With --strict, a completed red main exits 1 so the merge stops before
// inheriting a known-broken baseline.

import { execFileSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

function git(args) {
  return execFileSync("git", args, { cwd: REPO, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

const argv = process.argv.slice(2);
let branch;
let base = "origin/main";
let strict = false;
let all = false;
let ci = false;
for (let index = 0; index < argv.length; index += 1) {
  if (argv[index] === "--base") base = argv[(index += 1)];
  else if (argv[index] === "--strict") strict = true;
  else if (argv[index] === "--all") all = true;
  else if (argv[index] === "--ci") ci = true;
  else if (!branch) branch = argv[index];
  else {
    console.error("merge-guard: unknown argument " + argv[index]);
    process.exit(2);
  }
}
if (!branch) {
  console.error("usage: node tools/merge-guard.mjs <branch> [--base origin/main] [--all] [--strict]");
  process.exit(2);
}
const mergeBase = git(["merge-base", base, branch]);
const baseCommits = Number(git(["rev-list", "--count", mergeBase + ".." + base]));
const branchCommits = Number(git(["rev-list", "--count", mergeBase + ".." + branch]));
const mainFiles = new Set(git(["diff", "--name-only", mergeBase + ".." + base]).split("\n").filter(Boolean));
const branchFiles = git(["diff", "--name-only", mergeBase + ".." + branch]).split("\n").filter(Boolean);
const overlap = branchFiles.filter((file) => mainFiles.has(file));
const show = all ? overlap : overlap.slice(0, 5);
console.log("merge-guard: " + branch + " -> " + base + " | base moved " + baseCommits + " | branch " + branchCommits + " | overlap " + overlap.length);
for (const file of show) console.log("  overlap " + file);
if (!all && overlap.length > show.length) console.log("  ... +" + (overlap.length - show.length) + " more (--all)");
if (baseCommits > 0) console.log("stale: merge/rebase main, rerun the suite, check handoff markers");

function latestMainCi() {
  let run;
  try {
    const raw = execFileSync(
      "gh",
      [
        "run",
        "list",
        "--branch",
        "main",
        "--workflow",
        "test",
        "--limit",
        "1",
        "--json",
        "databaseId,status,conclusion,headSha,url,displayTitle",
      ],
      { cwd: REPO, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    ).trim();
    run = JSON.parse(raw)[0];
  } catch (error) {
    console.log("ci: cannot read main status (" + (error.message || error) + "); continuing");
    return 0;
  }
  if (!run) {
    console.log("ci: no main test run found");
    return 0;
  }
  console.log(
    "ci: main test " +
      run.status +
      (run.conclusion ? " / " + run.conclusion : "") +
      " " +
      String(run.headSha).slice(0, 12) +
      " " +
      run.url,
  );
  if (run.status === "completed" && run.conclusion !== "success") {
    console.log("ci: main is red; fix it first (node tools/ci-failures.mjs " + run.databaseId + ")");
    return 1;
  }
  return 0;
}

const ciRed = ci ? latestMainCi() : 0;
if (strict && (baseCommits > 0 || overlap.length > 0 || ciRed > 0)) process.exit(1);
