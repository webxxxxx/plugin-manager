#!/usr/bin/env node
// fmt: format exactly the files you name.
//
// `rustfmt src/manage/mod.rs` walks the `mod` declarations and reformats the
// whole subtree; one call once rewrote 27 unrelated files. `--config
// skip_children=true` stops that, and this wrapper also refuses anything
// outside src/ and tests/ so a typo cannot sweep the repo.
//
//   node tools/fmt.mjs src/manage/package.rs
//   node tools/fmt.mjs --check src/manage/package.rs
//
// There is deliberately no "format everything" mode: passing no files is a
// usage error, not a repo-wide format.

import { spawnSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import { dirname, isAbsolute, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");
const RUSTFMT_TIMEOUT_MS = 60 * 1000;

function fail(message) {
  console.error("fmt: " + message);
  process.exit(2);
}

function parseArgs(argv) {
  const args = { check: false, files: [] };
  for (const arg of argv) {
    if (arg === "--check") args.check = true;
    else if (arg.startsWith("-")) fail("unknown argument " + arg);
    else args.files.push(arg);
  }
  if (args.files.length === 0) {
    fail("usage: node tools/fmt.mjs [--check] <file.rs>... (name the files explicitly)");
  }
  return args;
}

/// Only files under src/ and tests/ are Rust sources; anything else is a typo
/// that must not be formatted.
function checkPath(file) {
  const full = resolve(REPO, file);
  if (!existsSync(full)) fail("missing file: " + file);
  if (!statSync(full).isFile()) fail("not a file (name the .rs files explicitly): " + file);
  const relativePath = relative(REPO, full).split("\\").join("/");
  if (isAbsolute(relativePath) || relativePath.startsWith("..")) {
    fail("outside the repo: " + file);
  }
  if (!relativePath.startsWith("src/") && !relativePath.startsWith("tests/")) {
    fail("only src/ and tests/ hold Rust sources: " + file);
  }
  if (!relativePath.endsWith(".rs")) fail("not a Rust file: " + file);
  return { full, relativePath };
}

const args = parseArgs(process.argv.slice(2));
const targets = args.files.map(checkPath);

const failed = [];
for (const target of targets) {
  const result = spawnSync(
    "rustfmt",
    [
      "--edition",
      "2021",
      // The whole point of the wrapper: a module root must not reformat its
      // children (rustfmt's default is to recurse).
      "--config",
      "skip_children=true",
      ...(args.check ? ["--check"] : []),
      target.full,
    ],
    { cwd: REPO, encoding: "utf8", timeout: RUSTFMT_TIMEOUT_MS },
  );
  if (result.error) {
    fail("cannot run rustfmt: " + result.error.message);
  }
  if (result.status === 0) {
    console.log(`fmt: ${args.check ? "ok    " : "wrote "} ${target.relativePath}`);
  } else {
    failed.push(target.relativePath);
    if (args.check) {
      // `--check` prints the diff on stdout; keep it, it is the useful part.
      process.stderr.write(result.stdout ?? "");
      process.stderr.write(result.stderr ?? "");
    } else {
      console.error(`fmt: FAILED ${target.relativePath}`);
      process.stderr.write(result.stderr ?? "");
    }
  }
}

if (failed.length > 0) {
  console.error(
    `fmt: ${failed.length} file(s) ${args.check ? "need formatting" : "failed"}: ${failed.join(", ")}`,
  );
  process.exit(1);
}
