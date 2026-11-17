#!/usr/bin/env node
// ci-failures: summarize the failing tests of one GitHub Actions run without
// pulling the whole log into the agent's context.
//
//   node tools/ci-failures.mjs <run-id> [--lines <n>]
//
// Read-only: it never starts a workflow. It prints the run header, then only
// the lines around panics, failed test headers, daemon log lines and HTTP
// response probes. Use it as the first step after a CI-only failure.

import { execFileSync } from "node:child_process";

function usage() {
  console.error("usage: node tools/ci-failures.mjs <run-id> [--lines <n>]");
  process.exit(2);
}

const argv = process.argv.slice(2);
let runId;
let maxLines = 120;
for (let index = 0; index < argv.length; index += 1) {
  if (argv[index] === "--lines") {
    maxLines = Number(argv[(index += 1)]) || 120;
  } else if (!runId) {
    runId = argv[index];
  } else {
    usage();
  }
}
if (!runId) usage();

function gh(args) {
  return execFileSync("gh", args, {
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    stdio: ["ignore", "pipe", "pipe"],
  });
}

let meta;
try {
  meta = JSON.parse(
    gh(["run", "view", runId, "--json", "url,status,conclusion,headSha,workflowName"]),
  );
} catch (error) {
  console.error("ci-failures: cannot read run " + runId + ": " + (error.message || error));
  process.exit(2);
}
console.log(
  "ci-failures: " +
    (meta.workflowName || "workflow") +
    " " +
    runId +
    " " +
    (meta.status || "") +
    (meta.conclusion ? " / " + meta.conclusion : "") +
    " " +
    (meta.headSha || "").slice(0, 12),
);
console.log("  " + (meta.url || ""));
if (meta.conclusion === "success") {
  console.log("ci-failures: run succeeded; nothing to summarize");
  process.exit(0);
}

let log = "";
try {
  log = gh(["run", "view", runId, "--log-failed"]);
} catch (error) {
  // gh exits non-zero when there are failed steps but still writes the log.
  log = error.stdout ? error.stdout.toString() : "";
  if (!log) {
    console.error("ci-failures: no failed-step log (" + (error.message || error) + ")");
    process.exit(2);
  }
}

const wanted = [
  /panicked at /,
  /\bFAILED\b/,
  /^failures?:/,
  /^\s{2,}\S+::\S+$/,
  /assertion .* failed/,
  /test result: FAILED/,
  /error: test failed/,
  /daemon log:/,
  /^svcd: /,
  /^response: /,
  /package staging/,
  /staging file/,
];
const lines = [];
for (const raw of log.split(/\r?\n/)) {
  if (!raw.trim()) continue;
  const job = raw.split("\t")[0] || "";
  const text =
    raw.replace(/^.*?\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z\s*/, "").trim() ||
    raw.trim();
  if (wanted.some((pattern) => pattern.test(text))) {
    lines.push((job ? job + ": " : "") + text);
  }
}
if (lines.length === 0) {
  console.log("ci-failures: no failure markers found; run may have failed in setup");
  process.exit(0);
}
console.log("");
for (const line of lines.slice(0, maxLines)) console.log(line);
if (lines.length > maxLines) {
  console.log("... " + (lines.length - maxLines) + " more lines (raise --lines)");
}
