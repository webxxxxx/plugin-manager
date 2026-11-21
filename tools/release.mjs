#!/usr/bin/env node
// release: bump the crate version, commit it, tag it, push both.
//
//   node tools/release.mjs patch              # 0.1.2 -> 0.1.3
//   node tools/release.mjs minor              # 0.1.2 -> 0.2.0
//   node tools/release.mjs major              # 0.1.2 -> 1.0.0
//   node tools/release.mjs 0.2.0              # explicit version
//   node tools/release.mjs patch --dry-run    # print the plan, touch nothing
//   node tools/release.mjs patch --no-push    # commit + tag locally only
//
// What it does, in this order, stopping at the first failure:
//   1  refuse unless the clone is on main, tracks no modified file, and the
//      version/tag it is about to write does not exist yet (local or origin);
//   2  rewrite `[package] version` in Cargo.toml and the `svcd` entry in
//      Cargo.lock (those two lines are the whole version);
//   3  verify the manifest and the lock agree (cargo metadata --locked);
//   4  run node tools/check.mjs quick (only with --check);
//   5  commit `chore(release): bump version to X` on main;
//   6  create the annotated tag `vX`;
//   7  push main, then the tag, with retries.
//
// The pre-commit hook blocks direct commits on main, so the commit step sets
// SVCD_ALLOW_MAIN_COMMIT=1 itself: a release commit belongs on main by design.
// A failed push keeps the commit and the tag local and prints the two commands
// that finish the job, so re-running the script is never required.

import { readFileSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");
const USAGE = `usage: node tools/release.mjs <patch|minor|major|X.Y.Z> [options]

  --dry-run             print the plan and every edit, write nothing
  --no-push             commit and tag locally, skip the push
  --no-fetch            skip the fetch and the origin/main check
  --allow-dirty         do not refuse on modified tracked files
  --skip-verify         skip cargo metadata --locked --offline
  --check               run node tools/check.mjs quick before the commit
  --message <text>      commit message (default: chore(release): bump version to X)
  --tag-message <text>  tag message (default: vX)`;

function fail(message) {
  console.error(`release: ${message}`);
  process.exit(1);
}

function step(number, message) {
  console.log(`release: [${number}/7] ${message}`);
}

function capture(command, args, env = {}) {
  const result = spawnSync(command, args, {
    cwd: REPO,
    encoding: "utf8",
    env: { ...process.env, ...env },
  });
  if (result.error) fail(`${command} ${args.join(" ")}: ${result.error.message}`);
  return result;
}

function git(args, env = {}) {
  const result = capture("git", args, env);
  if (result.status !== 0) {
    fail(`git ${args.join(" ")}\n${(result.stderr || result.stdout || "").trim()}`);
  }
  return result.stdout;
}

function gitQuiet(args) {
  const result = capture("git", args);
  return result.status === 0 ? result.stdout.trim() : "";
}

function sleep(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function parseArgs(argv) {
  const options = {
    target: null,
    dryRun: false,
    push: true,
    fetch: true,
    allowDirty: false,
    verify: true,
    check: false,
    message: null,
    tagMessage: null,
  };
  const valueFlags = new Set(["--message", "--tag-message"]);
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      console.log(USAGE);
      process.exit(0);
    }
    if (arg === "--dry-run") options.dryRun = true;
    else if (arg === "--no-push") options.push = false;
    else if (arg === "--no-fetch") options.fetch = false;
    else if (arg === "--allow-dirty") options.allowDirty = true;
    else if (arg === "--skip-verify") options.verify = false;
    else if (arg === "--check") options.check = true;
    else if (valueFlags.has(arg)) {
      const value = argv[index + 1];
      if (value === undefined) fail(`${arg} needs a value`);
      if (arg === "--message") options.message = value;
      else options.tagMessage = value;
      index += 1;
    } else if (arg.startsWith("-")) {
      fail(`unknown option ${arg}\n${USAGE}`);
    } else if (options.target === null) {
      options.target = arg;
    } else {
      fail(`unexpected argument ${arg}\n${USAGE}`);
    }
  }
  if (options.target === null) fail(`missing version\n${USAGE}`);
  return options;
}

function readVersion(text) {
  // The checkout may hold CRLF (core.autocrlf), so the line break is optional.
  const match = /^name = "svcd"\r?\nversion = "([^"]+)"/m.exec(text);
  return match ? match[1] : null;
}

function bump(current, target) {
  const parts = current.split(".").map(Number);
  if (parts.length !== 3 || parts.some((part) => Number.isNaN(part))) {
    fail(`Cargo.toml carries an unexpected version ${current}`);
  }
  const [major, minor, patch] = parts;
  if (target === "patch") return `${major}.${minor}.${patch + 1}`;
  if (target === "minor") return `${major}.${minor + 1}.0`;
  if (target === "major") return `${major + 1}.0.0`;
  if (!/^\d+\.\d+\.\d+$/.test(target)) {
    fail(`not a version: ${target} (want patch, minor, major or X.Y.Z)`);
  }
  return target;
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  const dry = options.dryRun;

  // 1. Preconditions. Each refusal names the command that fixes it.
  step(1, "preconditions: on main, clean tree, unused version and tag");
  const branch = gitQuiet(["symbolic-ref", "--quiet", "--short", "HEAD"]);
  if (branch !== "main") {
    fail(`on ${branch || "a detached HEAD"}; releases are cut from main (merge your branch first)`);
  }
  if (!options.allowDirty) {
    const dirty = gitQuiet(["status", "--porcelain", "--untracked-files=no"]);
    if (dirty) fail(`uncommitted changes on main:\n${dirty}`);
  }

  const manifestPath = join(REPO, "Cargo.toml");
  const lockPath = join(REPO, "Cargo.lock");
  const manifest = readFileSync(manifestPath, "utf8");
  const lock = readFileSync(lockPath, "utf8");
  const current = readVersion(manifest);
  if (!current) fail("could not read `name = \"svcd\"` + `version` from Cargo.toml");
  const version = bump(current, options.target);
  const tag = `v${version}`;
  const commitMessage = options.message || `chore(release): bump version to ${version}`;
  const tagMessage = options.tagMessage || tag;

  if (version === current) fail(`already at ${current}; nothing to bump`);
  if (gitQuiet(["rev-parse", "--verify", "--quiet", `refs/tags/${tag}`])) {
    fail(`tag ${tag} already exists locally (git tag -d ${tag} to drop it)`);
  }

  if (options.fetch && options.push) {
    const fetched = capture("git", ["fetch", "origin", "main", "--tags"]);
    if (fetched.status !== 0) {
      fail(`git fetch origin failed, refusing to guess the remote head:\n${(fetched.stderr || "").trim()}\n  (use --no-fetch to release against the local state)`);
    }
    const remoteMain = gitQuiet(["rev-parse", "origin/main"]);
    const localMain = gitQuiet(["rev-parse", "main"]);
    if (remoteMain && localMain && remoteMain !== localMain) {
      fail(`main and origin/main differ (${localMain.slice(0, 8)} vs ${remoteMain.slice(0, 8)}); pull or push first`);
    }
    const remoteTag = gitQuiet(["ls-remote", "--tags", "origin", `refs/tags/${tag}`]);
    if (remoteTag) fail(`tag ${tag} already exists on origin`);
  }

  console.log(`release: ${current} -> ${version} (${tag})${dry ? " [dry run]" : ""}`);

  // 2. The two lines that carry the version.
  const nextManifest = manifest.replace(
    /^(name = "svcd"\r?\nversion = ")[^"]+(")/m,
    `$1${version}$2`,
  );
  const nextLock = lock.replace(
    /(\[\[package\]\]\r?\nname = "svcd"\r?\nversion = ")[^"]+(")/,
    `$1${version}$2`,
  );
  if (nextManifest === manifest) fail("Cargo.toml version line did not match");
  if (nextLock === lock) fail("Cargo.lock has no `[[package]] svcd` entry");
  step(2, `Cargo.toml and Cargo.lock -> ${version}`);
  if (dry) {
    console.log(`  - Cargo.toml: version = "${current}" -> version = "${version}"`);
    console.log(`  - Cargo.lock: [[package]] svcd ${current} -> ${version}`);
  } else {
    writeFileSync(manifestPath, nextManifest);
    writeFileSync(lockPath, nextLock);
  }

  // 3. The lock is what `cargo build --locked` will use, so prove it agrees
  //    before the bump reaches a commit.
  if (options.verify) {
    step(3, "cargo metadata --locked --offline");
    if (dry) {
      console.log("  - skipped in a dry run");
    } else {
      const checked = capture("cargo", ["metadata", "--locked", "--offline", "--format-version", "1"]);
      if (checked.status !== 0) {
        git(["checkout", "--", "Cargo.toml", "Cargo.lock"]);
        fail(`the manifest and the lock disagree, reverted both files:\n${(checked.stderr || "").trim()}`);
      }
    }
  } else {
    step(3, "cargo metadata skipped (--skip-verify)");
  }

  // 4. Commit, tag, push.
  // 4. The optional gate before the version becomes a commit.
  step(4, options.check ? "node tools/check.mjs quick" : "build check skipped (--check not given)");
  if (options.check && !dry) {
    const checked = capture(process.execPath, [join(REPO, "tools", "check.mjs"), "quick"]);
    if (checked.status !== 0) {
      git(["checkout", "--", "Cargo.toml", "Cargo.lock"]);
      fail(`checks failed, reverted both files:\n${(checked.stdout || "").trim()}`);
    }
  }

  step(5, `commit on main: ${commitMessage}`);
  if (dry) {
    console.log("  - git add Cargo.toml Cargo.lock");
    console.log(`  - git commit -m "${commitMessage}"`);
  } else {
    git(["add", "Cargo.toml", "Cargo.lock"]);
    const committed = capture("git", ["commit", "-m", commitMessage], { SVCD_ALLOW_MAIN_COMMIT: "1" });
    if (committed.status !== 0) {
      git(["checkout", "--", "Cargo.toml", "Cargo.lock"]);
      fail(`git commit failed, reverted both files:\n${(committed.stdout || "").trim()}\n${(committed.stderr || "").trim()}`);
    }
    console.log(`  - ${gitQuiet(["log", "--oneline", "-1"])}`);
  }

  step(6, `annotated tag ${tag}`);
  if (dry) {
    console.log(`  - git tag -a ${tag} -m "${tagMessage}"`);
  } else {
    git(["tag", "-a", tag, "-m", tagMessage]);
    console.log(`  - ${gitQuiet(["for-each-ref", `refs/tags/${tag}`, "--format=%(objecttype) %(objectname:short) -> %(*objectname:short)"])}`);
  }

  if (!options.push) {
    step(7, "push skipped (--no-push)");
    console.log(`release: local only; finish with:\n  git push origin main\n  git push origin ${tag}`);
    return;
  }
  step(7, `push main and ${tag}`);
  if (dry) {
    console.log("  - git push origin main");
    console.log(`  - git push origin ${tag}`);
    return;
  }
  let pushed = false;
  for (let attempt = 1; attempt <= 3 && !pushed; attempt += 1) {
    const result = capture("git", ["push", "origin", "main", tag]);
    if (result.status === 0) {
      pushed = true;
      break;
    }
    console.log(`  - push attempt ${attempt} failed: ${(result.stderr || "").trim().split("\n").slice(-1)[0]}`);
    if (attempt < 3) sleep(3000);
  }
  if (!pushed) {
    fail(`push failed after 3 attempts; the commit and tag are local:\n  git push origin main\n  git push origin ${tag}`);
  }
  const head = gitQuiet(["rev-parse", "--short", "HEAD"]);
  console.log(`release: pushed ${head} and ${tag} to origin`);
}

main();
