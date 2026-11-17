#!/usr/bin/env node
// check: run a test/check tier and print a compact summary instead of the
// thousands of lines cargo emits. Full logs stay in memory; only failures are
// printed, capped.
//
//   node tools/check.mjs quick            cargo check --all-targets
//   node tools/check.mjs no-default       cargo check --no-default-features --bin svcd
//   node tools/check.mjs cross [target]   cargo check --target <target> --all-targets
//   node tools/check.mjs test <filter>    cargo test <filter> --no-fail-fast
//   node tools/check.mjs full             cargo test --no-fail-fast
//
// `cross` exists because a Windows host never compiles `cfg(unix)` code:
// a green `quick`/`full` says nothing about the Linux build (#139).

import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

function cargo(args) {
  const result = spawnSync("cargo", args, {
    cwd: REPO,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    env: process.env,
  });
  if (result.error) {
    console.error("check: cannot run cargo: " + result.error.message);
    process.exit(2);
  }
  return result;
}

function lines(text) {
  return (text || "").split(/\r?\n/).filter(Boolean);
}

function failureTail(result, cap = 60) {
  const all = lines(result.stdout).concat(lines(result.stderr));
  return all.slice(-cap).join("\n");
}

function summarizeTests(result) {
  const all = lines(result.stdout).concat(lines(result.stderr));
  const results = all.filter((line) => line.startsWith("test result:"));
  const kept = results.filter((line) => {
    const match = line.match(/^test result: (ok|FAILED)\. (\d+) passed/);
    return !match || Number(match[2]) > 0 || match[1] === "FAILED";
  });
  const failures = [...new Set(
    all.filter(
      (line) =>
        !line.startsWith("test result:") &&
        (line.includes("FAILED") || line.includes("panicked at") || line.startsWith("error:")),
    ),
  )].slice(0, 40);
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
  const result = cargo(["check", "--all-targets", "--quiet"]);
  if (result.status === 0) console.log("check: ok (all targets)");
  else {
    console.log("check: FAILED");
    console.log(failureTail(result));
  }
  process.exit(result.status ?? 1);
}

if (tier === "no-default") {
  const result = cargo(["check", "--no-default-features", "--bin", "svcd", "--quiet"]);
  if (result.status === 0) console.log("check: ok (no default features)");
  else {
    console.log("check: FAILED (no default features)");
    console.log(failureTail(result));
  }
  process.exit(result.status ?? 1);
}

if (tier === "cross") {
  const target = rest[0] || CROSS_TARGET;
  const result = cargo(["check", "--target", target, "--all-targets", "--quiet"]);
  reportCross(target, result);
  process.exit(result.status ?? 1);
}

if (tier === "test" || tier === "full") {
  const filter = tier === "test" ? rest : [];
  const result = cargo(["test", ...filter, "--no-fail-fast"]);
  summarizeTests(result);
  process.exit(result.status ?? 1);
}

console.error(
  "check: unknown tier " + tier + "; use quick|no-default|cross [target]|full|test <filter>",
);
process.exit(2);
