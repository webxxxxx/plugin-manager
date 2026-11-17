#!/usr/bin/env node
// check: run a test/check tier and print a compact summary instead of the
// thousands of lines cargo emits. Full logs stay in memory; only failures are
// printed, capped.
//
//   node tools/check.mjs quick            cargo check --all-targets
//   node tools/check.mjs no-default       cargo check --no-default-features --bin svcd
//   node tools/check.mjs cross [target]   cargo check --target <target> --all-targets
//   node tools/check.mjs test <filter>    cargo test <filter> --no-fail-fast -- --test-threads=1
//   node tools/check.mjs full             cargo test --no-fail-fast -- --test-threads=1
//   ... --parallel                        drop `--test-threads=1` (one-off debugging)
//
// `cross` exists because a Windows host never compiles `cfg(unix)` code:
// a green `quick`/`full` says nothing about the Linux build (#139).
//
// The test tiers default to the same knob CI uses (`--test-threads=1`), so a
// red run means the same thing locally and in CI. Parallel runs share process
// state and have always needed a human to judge "noise" (#153).
//
// Every tier has a wall-clock cap: a hung test prints TIMEOUT and the last
// lines instead of looking like a slow compile. Override with
// `SVCD_CHECK_TIMEOUT_MS` while debugging.

import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

/// 编译档和测试档的墙钟上限；`SVCD_CHECK_TIMEOUT_MS` 覆盖（调试卡死用）。
const COMPILE_TIMEOUT_MS = Number(process.env.SVCD_CHECK_TIMEOUT_MS) || 10 * 60 * 1000;
const TEST_TIMEOUT_MS = Number(process.env.SVCD_CHECK_TIMEOUT_MS) || 20 * 60 * 1000;

function cargo(args, timeoutMs) {
  const started = Date.now();
  const result = spawnSync("cargo", args, {
    cwd: REPO,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    env: process.env,
    timeout: timeoutMs,
  });
  result.elapsedMs = Date.now() - started;
  // A killed child reports `status: null` + `error.code === "ETIMEDOUT"`;
  // any other spawn error is the tool itself being unusable.
  if (result.error && result.error.code !== "ETIMEDOUT") {
    console.error("check: cannot run cargo: " + result.error.message);
    process.exit(2);
  }
  return result;
}

function timedOut(result) {
  return Boolean(result.error && result.error.code === "ETIMEDOUT");
}

function reportTimeout(result) {
  const seconds = Math.round((result.elapsedMs ?? 0) / 1000);
  console.log(`check: TIMEOUT after ${seconds}s (cargo was killed); last output:`);
  console.log(failureTail(result, 40));
}

function lines(text) {
  return (text || "").split(/\r?\n/).filter(Boolean);
}

function failureTail(result, cap = 60) {
  const all = lines(result.stdout).concat(lines(result.stderr));
  return all.slice(-cap).join("\n");
}

/// How many message lines to keep after one `panicked at` header.
const PANIC_CONTEXT_LINES = 12;
/// Cap for the whole failure block, so a suite with many panics stays readable.
const FAILURE_LINE_CAP = 80;

/// The failing lines, plus the message cargo prints *after* each `panicked at`
/// header. cargo puts the panic message on the following lines, so keeping only
/// header-matching lines drops the key/request/actual detail that a named
/// regression exists to report (#155).
function failureLines(all) {
  const kept = [];
  const keep = (line) => {
    if (!kept.includes(line)) kept.push(line);
  };
  for (let index = 0; index < all.length; index += 1) {
    const line = all[index];
    if (line.includes("panicked at")) {
      keep(line);
      for (let next = index + 1; next < all.length; next += 1) {
        const text = all[next];
        const stop =
          next - index > PANIC_CONTEXT_LINES ||
          text.trim() === "" ||
          text.startsWith("note:") ||
          text.startsWith("stack backtrace:") ||
          text.startsWith("---- ") ||
          text.startsWith("test result:") ||
          text.includes("panicked at");
        if (stop) break;
        keep(text);
      }
      continue;
    }
    if (line.startsWith("test result:")) continue;
    if (line.includes("FAILED") || line.startsWith("error:")) keep(line);
  }
  return kept.slice(0, FAILURE_LINE_CAP);
}

function summarizeTests(result) {
  const all = lines(result.stdout).concat(lines(result.stderr));
  const results = all.filter((line) => line.startsWith("test result:"));
  const kept = results.filter((line) => {
    const match = line.match(/^test result: (ok|FAILED)\. (\d+) passed/);
    return !match || Number(match[2]) > 0 || match[1] === "FAILED";
  });
  const failures = failureLines(all);
  const empty = results.length - kept.length;
  console.log(
    "test: " +
      (result.status === 0 ? "ok" : "FAILED") +
      " | " +
      kept.length +
      " target(s) with results" +
      (empty > 0 ? ", " + empty + " empty" : ""),
  );
  for (const line of kept) console.log("  " + line);
  if (result.status !== 0) {
    console.log("failures:");
    for (const line of failures) console.log("  " + line);
  }
}

const argv = process.argv.slice(2);
const tier = argv[0] || "quick";
const rest = argv.slice(1);

/// The target a Windows host should also check: it is the one the repo's
/// musl CI job builds, and it compiles the `cfg(unix)` half of the tree.
const CROSS_TARGET = "x86_64-unknown-linux-musl";

/// Print only the first compiler error block and a count, so a cross-target
/// failure reads like the test tiers instead of thousands of cargo lines.
function reportCross(target, result) {
  const all = lines(result.stdout).concat(lines(result.stderr));
  const ok = result.status === 0;
  console.log("cross: " + (ok ? "ok" : "FAILED") + " (" + target + ", all targets)");
  if (ok) return;
  const errorLines = all.filter(
    (line) => /^error(\[|:)/.test(line) && !line.includes("could not compile"),
  );
  const warnings = all.filter((line) => line.startsWith("warning:")).length;
  const first = all.findIndex((line) => /^error(\[|:)/.test(line));
  if (first >= 0) {
    const block = [];
    for (let index = first; index < all.length && block.length < 12; index += 1) {
      // Stop at the next error so only the first one is shown.
      if (index > first && /^error(\[|:)/.test(all[index])) break;
      if (all[index] === "" && block.length > 2) break;
      block.push(all[index]);
    }
    for (const line of block) console.log("  " + line);
  } else {
    console.log(failureTail(result));
  }
  console.log(
    "  errors: " + errorLines.length + " | warnings: " + warnings,
  );
}

if (tier === "--help" || tier === "-h") {
  console.log("usage: node tools/check.mjs quick|no-default|cross [target]|full|test <filter>");
  process.exit(0);
}

if (tier === "quick") {
  console.log("check: running cargo check --all-targets");
  const result = cargo(["check", "--all-targets", "--quiet"], COMPILE_TIMEOUT_MS);
  if (timedOut(result)) {
    reportTimeout(result);
    process.exit(124);
  }
  if (result.status === 0) console.log("check: ok (all targets)");
  else {
    console.log("check: FAILED");
    console.log(failureTail(result));
  }
  process.exit(result.status ?? 1);
}

if (tier === "no-default") {
  console.log("check: running cargo check --no-default-features --bin svcd");
  const result = cargo(
    ["check", "--no-default-features", "--bin", "svcd", "--quiet"],
    COMPILE_TIMEOUT_MS,
  );
  if (timedOut(result)) {
    reportTimeout(result);
    process.exit(124);
  }
  if (result.status === 0) console.log("check: ok (no default features)");
  else {
    console.log("check: FAILED (no default features)");
    console.log(failureTail(result));
  }
  process.exit(result.status ?? 1);
}

if (tier === "cross") {
  const target = rest[0] || CROSS_TARGET;
  console.log(`check: running cargo check --target ${target} --all-targets`);
  const result = cargo(
    ["check", "--target", target, "--all-targets", "--quiet"],
    COMPILE_TIMEOUT_MS,
  );
  if (timedOut(result)) {
    reportTimeout(result);
    process.exit(124);
  }
  reportCross(target, result);
  process.exit(result.status ?? 1);
}

if (tier === "test" || tier === "full") {
  const parallel = rest.includes("--parallel");
  const filter = tier === "test" ? rest.filter((arg) => arg !== "--parallel") : [];
  const args = ["test", ...filter, "--no-fail-fast"];
  if (!parallel) args.push("--", "--test-threads=1");
  console.log("check: running cargo " + args.join(" "));
  const result = cargo(args, TEST_TIMEOUT_MS);
  if (timedOut(result)) {
    reportTimeout(result);
    process.exit(124);
  }
  summarizeTests(result);
  process.exit(result.status ?? 1);
}

console.error(
  "check: unknown tier " + tier + "; use quick|no-default|cross [target]|full [--parallel]|test <filter> [--parallel]",
);
process.exit(2);
