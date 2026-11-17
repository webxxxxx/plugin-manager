#!/usr/bin/env node
// closed-tickets-merged: a closed ticket must have at least one commit on the
// base branch. It catches the #119 case (ticket closed, branch never merged).
//
// Usage:
//   node tools/closed-tickets-merged.mjs --map 112
//   node tools/closed-tickets-merged.mjs --map 112 --base origin/main --json
//   node tools/closed-tickets-merged.mjs --map 112 --allow 110,111
//
// Requires an authenticated gh CLI (GH_TOKEN in CI).

import { execFileSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

function parseArgs(argv) {
  const args = { map: "112", base: "origin/main", allow: new Set(), json: false };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--map") args.map = argv[(index += 1)];
    else if (arg === "--base") args.base = argv[(index += 1)];
    else if (arg === "--allow") {
      for (const key of argv[(index += 1)].split(",")) {
        if (key.trim()) args.allow.add(key.trim());
      }
    } else if (arg === "--json") args.json = true;
    else {
      console.error(`closed-tickets-merged: unknown argument ${arg}`);
      process.exit(2);
    }
  }
  return args;
}

function run(command, args, cwd = REPO) {
  return execFileSync(command, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

function repoSlug() {
  try {
    const remote = run("git", ["remote", "get-url", "origin"]);
    const match = remote.match(/github\.com[:/]([^/]+\/[^/]+?)(?:\.git)?$/);
    if (match) return match[1];
  } catch {}
  return run("gh", ["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"]);
}

function subIssues(repo, map, attempts = 3) {
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    try {
      return subIssuesOnce(repo, map);
    } catch (error) {
      if (attempt === attempts) throw error;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 500 * attempt);
    }
  }
  return [];
}

function subIssuesOnce(repo, map) {
  const output = run("gh", [
    "api",
    `repos/${repo}/issues/${map}/sub_issues`,
    "--paginate",
    "--jq",
    ".[] | [.number, .state, .title] | @tsv",
  ]);
  return output
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const [number, state, ...title] = line.split("\t");
      return { number, state, title: title.join("\t") };
    });
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  try {
    run("git", ["rev-parse", "--verify", args.base]);
  } catch {
    console.error(`closed-tickets-merged: base ${args.base} does not resolve; fetch first`);
    process.exit(2);
  }
  const repo = repoSlug();
  const issues = subIssues(repo, args.map);
  // Match issue numbers in the SUBJECT, not the body: a docs/ADR commit that
  // merely mentions "#119" must not make a closed-but-unmerged ticket pass.
  const history = run("git", ["log", args.base, "--format=%H%x09%s"])
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const [sha, ...subject] = line.split("\t");
      return { sha, subject: subject.join("\t") };
    });
  const checked = [];
  const missing = [];
  for (const issue of issues) {
    if (issue.state.toLowerCase() !== "closed") continue;
    if (args.allow.has(issue.number)) continue;
    const marker = new RegExp(`#${issue.number}(?!\\d)`);
    const commits = history.filter((commit) => marker.test(commit.subject));
    const entry = { ...issue, commits: commits.length };
    checked.push(entry);
    if (commits.length === 0) missing.push(entry);
  }
  if (args.json) {
    console.log(JSON.stringify({ base: args.base, map: args.map, checked, missing }, null, 2));
    if (missing.length > 0) process.exit(1);
  } else {
    console.log(`closed-tickets-merged: map #${args.map} on ${args.base} (${checked.length} closed children)`);
    for (const entry of checked) {
      const mark = entry.commits > 0 ? "ok" : "MISSING";
      console.log(`  ${mark.padEnd(7)} #${entry.number} ${entry.title} (${entry.commits} commit refs)`);
    }
    if (missing.length > 0) {
      console.error("");
      console.error("Closed but no commit on the base branch:");
      for (const entry of missing) {
        console.error(`  #${entry.number} ${entry.title}`);
      }
      console.error("Merge the branch (or close with a documented no-code reason) before shipping.");
      process.exit(1);
    }
  }
}

main();
