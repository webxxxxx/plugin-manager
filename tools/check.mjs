#!/usr/bin/env node
// check: run a test/check tier and print a compact summary instead of the
// thousands of lines cargo emits. Full logs stay in memory; only failures are
// printed, capped.
//
//   node tools/check.mjs quick            cargo check --all-targets
//   node tools/check.mjs lint             cargo clippy --all-targets -- -D warnings
//   node tools/check.mjs fmt              rustfmt --check on every src/ and tests/ file
//   node tools/check.mjs size             src/**/*.rs 文件行数预算（size-lint.mjs）
//   node tools/check.mjs guardrail        tools/check_spk.mjs --self-test + tools/check-console.mjs --self-test（校验器注入测试）
//   node tools/check.mjs no-default       cargo check --no-default-features --bin svcd
//   node tools/check.mjs cross [target]   cargo check --target <target> --all-targets
//   node tools/check.mjs linux [filter]   Windows 宿主：在 WSL2 里跑 Linux 全量（或一个 filter）
//   node tools/check.mjs test <filter>    cargo test <filter> --no-fail-fast -- --test-threads=1
//   node tools/check.mjs integration      all tests/*.rs, one cap per target (CI's integration step)
//   node tools/check.mjs full             unit parallel, then `integration`
//   ... --parallel                        drop the serial knob for every target (one-off debugging)
//   node tools/check.mjs --selftest       discovery + empty-target-dir guard injection test
//
// `cross` exists because a Windows host never compiles `cfg(unix)` code:
// a green `quick`/`full` says nothing about the Linux build (#139).
//
// Integration tiers run one cargo invocation per tests/*.rs target, so each
// gets its own wall-clock cap. Except the HTTP suite, which is isolated enough
// to run four-wide (5 consecutive green runs), targets stay serial. Unit tests
// have no sandbox and run in parallel: 826 of them shrank from 19 s to 3 s on a
// 16-core host. The daemon suite still spawns crash loops that share pid
// assumptions, so it stays serial until it proves order-independence (#153).
//
// Every tier has a wall-clock cap: a hung test prints TIMEOUT and the last
// lines instead of looking like a slow compile. Override with
// `SVCD_CHECK_TIMEOUT_MS` while debugging.

import { mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { basename, dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

/// 编译档和测试档的墙钟上限；`SVCD_CHECK_TIMEOUT_MS` 覆盖（调试卡死用）。
const COMPILE_TIMEOUT_MS = Number(process.env.SVCD_CHECK_TIMEOUT_MS) || 10 * 60 * 1000;
const TEST_TIMEOUT_MS = Number(process.env.SVCD_CHECK_TIMEOUT_MS) || 20 * 60 * 1000;

/// `no-default` 的警告预算，棘轮语义：只许降不许升。
///
/// 2026-11-26（#326）：`cargo check --no-default-features --bin svcd` 在 Windows
/// 与 Linux 都是 **0 条**。之前的 24/23 是同一类问题的积压——「只有 http 面会用、
/// 但模块 lean 也要编」的函数/字段/import。现在按 feature 边界切开：整块能切的用
/// `#[cfg(feature = "http")]`，会波及 lean 也要编的调用方或测试的用
/// `cfg_attr(not(feature = "http"), allow(dead_code))` 逐条标注。
///
/// rustc 升级或新代码再引入警告时，先跑
/// `cargo check --no-default-features --message-format short | grep -c ': warning:'`
/// 判断是否合理；任何一个警告都会让这个档变红，而不是被预算吃掉。
/// `SVCD_NO_DEFAULT_BUDGET` 可临时覆盖（注入测试与调试用）。
const NO_DEFAULT_WARNING_BUDGET = Number(process.env.SVCD_NO_DEFAULT_BUDGET) || 0;

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
/// Large enough that several panic blocks keep their message lines.
const FAILURE_LINE_CAP = 200;

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
      // Keep each panic block whole: deduping message lines globally would
      // strip a later block of an identical line and leave it context-free.
      const block = [line];
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
        if (!block.includes(text)) block.push(text);
      }
      kept.push(...block);
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

/// Directory that owns `tests/*.rs`; the override exists so `--selftest` can
/// prove the empty-directory guard without moving the real suite.
function testsDir() {
  return process.env.SVCD_CHECK_TESTS_DIR || join(REPO, "tests");
}

/// Every top-level `tests/*.rs` file is one integration test target. Cargo's
/// `--tests` also reruns the bin/lib unit harness, so the serial half names
/// each integration target explicitly instead. The `support/` directory is not
/// a target and is skipped.
function integrationTargets() {
  return readdirSync(testsDir(), { withFileTypes: true })
    .filter((entry) => entry.isFile() && entry.name.endsWith(".rs"))
    .map((entry) => entry.name.slice(0, -3))
    .sort();
}

/// The discovered list, with an explicit failure when discovery finds nothing.
/// A silently empty list used to fall back to an unfiltered `cargo test`.
function requiredTargets() {
  const targets = integrationTargets();
  if (targets.length === 0) {
    throw new Error("no integration targets under " + testsDir());
  }
  return targets;
}

/// 单测没有 sandbox，默认并行（本机 8、CI 4，`SVCD_UNIT_THREADS` 覆盖）。
function unitThreads() {
  return process.env.SVCD_UNIT_THREADS || "8";
}

/// 一个 target 的并发度。http 与 daemon 都是进程重活，四条并发，各自以本机
/// 连续五次全绿为准（`SVCD_HTTP_THREADS` / `SVCD_DAEMON_THREADS` 覆盖）；
/// 其余 target 串行，直到它们各自证明与顺序无关。
function targetThreads(target) {
  if (target === "http") return process.env.SVCD_HTTP_THREADS || "4";
  if (target === "daemon") return process.env.SVCD_DAEMON_THREADS || "4";
  return "1";
}

/// 单测步骤。`full`、`linux` 与集成档共用同一份线程数口径。
function unitStep() {
  return {
    args: [
      "test",
      "--bin",
      "svcd",
      "--no-fail-fast",
      "--",
      "--test-threads=" + unitThreads(),
    ],
  };
}

/// 每个 target 一次 cargo 调用，各自吃一个墙钟上限；发现不到 target 时返回
/// null 并报错，而不是退回一次不带 `--test` 的 `cargo test`。
function integrationSteps() {
  let targets;
  try {
    targets = requiredTargets();
  } catch (error) {
    console.error("check: " + error.message);
    return null;
  }
  return targets.map((target) => ({
    args: [
      "test",
      "--test",
      target,
      "--no-fail-fast",
      "--",
      "--test-threads=" + targetThreads(target),
    ],
  }));
}

/// 按顺序跑步骤，第一个失败就停：一个 hung target 只花掉它自己的上限。
function runSteps(steps, run) {
  for (const step of steps) {
    const code = run(step.args);
    if (code !== 0) return code;
  }
  return 0;
}

/// Run every integration target in its own cargo invocation.
function runIntegration() {
  const steps = integrationSteps();
  if (!steps) return 2;
  return runSteps(steps, (args) => runTestTier(args));
}

/// Guardrail injection test: a temp tests dir with two files must discover both,
/// and an empty dir must make `requiredTargets` throw. The real tree is the
/// positive case exercised by every `full`/`integration` run.
function selftest() {
  const root = mkdtempSync(join(tmpdir(), "svcd-check-"));
  const empty = mkdtempSync(join(tmpdir(), "svcd-check-empty-"));
  const previous = process.env.SVCD_CHECK_TESTS_DIR;
  try {
    writeFileSync(join(root, "cli.rs"), "");
    writeFileSync(join(root, "http.rs"), "");
    mkdirSync(join(root, "support"));
    process.env.SVCD_CHECK_TESTS_DIR = root;
    const found = integrationTargets();
    if (found.join(",") !== "cli,http") {
      throw new Error("discovery returned " + JSON.stringify(found));
    }
    process.env.SVCD_CHECK_TESTS_DIR = empty;
    let rejected = false;
    try {
      requiredTargets();
    } catch {
      rejected = true;
    }
    if (!rejected) throw new Error("an empty tests dir was accepted");
    console.log("selftest: ok (discovery, empty-dir guard)");
    return 0;
  } catch (error) {
    console.error("selftest: FAILED " + error.message);
    return 1;
  } finally {
    if (previous === undefined) delete process.env.SVCD_CHECK_TESTS_DIR;
    else process.env.SVCD_CHECK_TESTS_DIR = previous;
    rmSync(root, { recursive: true, force: true });
    rmSync(empty, { recursive: true, force: true });
  }
}

/// Run one cargo test invocation, print its summary and return its exit code.
/// `full` uses this twice: unit tests run in parallel, integration tests keep
/// the serial gate. A timeout is 124, matching the other tiers.
function runTestTier(args) {
  console.log("check: running cargo " + args.join(" "));
  const result = cargo(args, TEST_TIMEOUT_MS);
  if (timedOut(result)) {
    reportTimeout(result);
    return 124;
  }
  summarizeTests(result);
  return result.status ?? 1;
}

const argv = process.argv.slice(2);
if (argv.includes("--selftest")) process.exit(selftest());
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

/// Windows 路径 → WSL 里的 `/mnt/<drive>/...`；非 Windows 路径原样返回。
function wslPath(path) {
  const match = /^([A-Za-z]):[\\/](.*)$/.exec(path);
  if (!match) return path.split("\\").join("/");
  return "/mnt/" + match[1].toLowerCase() + "/" + match[2].split("\\").join("/");
}

function shellQuote(value) {
  return "'" + String(value).replace(/'/g, "'\\''") + "'";
}

/// One cargo invocation inside WSL, with the same summary and timeout contract
/// as [`runTestTier`]: each target gets its own wsl.exe call, so a hung target
/// only spends its own cap and the run stops at the first failure.
function runCargoInWsl(args, distro, target, where) {
  const script = `cd ${shellQuote(where)} && CARGO_TARGET_DIR=${shellQuote(target)} cargo ${args
    .map(shellQuote)
    .join(" ")}`;
  console.log(`check: running cargo ${args.join(" ")} in WSL (${distro}) at ${where}`);
  const result = spawnSync("wsl.exe", ["-d", distro, "--", "bash", "-lc", script], {
    cwd: REPO,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    env: process.env,
    timeout: TEST_TIMEOUT_MS,
  });
  if (result.error && result.error.code !== "ETIMEDOUT") {
    console.error(
      "check: cannot run wsl.exe: " +
        result.error.message +
        " (WSL2 与发行版见 docs/testing.md §6；发行版用 SVCD_WSL_DISTRO 覆盖)",
    );
    return 2;
  }
  if (timedOut(result)) {
    reportTimeout(result);
    return 124;
  }
  summarizeTests(result);
  return result.status ?? 1;
}

/// Windows 宿主的 Linux 半：`cfg(unix)` 测试只能在 WSL2 里真跑（docs/testing.md §6）。
/// 与 `full` 同形——单测并行，然后逐 target 各一次 cargo（http 四条并发，其余串行），
/// 失败即停。target 放 Linux 文件系统，避免与 Windows 的 target 抢锁。
if (tier === "linux") {
  if (rest.length > 1) {
    console.error("check: linux takes at most one test-name filter; use the raw runbook in docs/testing.md §6 for --exact/--test");
    process.exit(2);
  }
  const filter = rest[0] || "";
  const distro = process.env.SVCD_WSL_DISTRO || "Debian";
  const wslTarget = "/tmp/wt-" + basename(REPO) + "-target";
  const where = wslPath(REPO);
  const runCargo =
    process.platform === "win32"
      ? (args) => runCargoInWsl(args, distro, wslTarget, where)
      : (args) => runTestTier(args);
  // A filter keeps the single-invocation shape: it is a debugging probe, and
  // one cargo over every target is what `cargo test <filter>` means.
  if (filter) {
    process.exit(
      runCargo(["test", filter, "--no-fail-fast", "--", "--test-threads=1"]),
    );
  }
  const integration = integrationSteps();
  if (!integration) process.exit(2);
  process.exit(runSteps([unitStep(), ...integration], runCargo));
}

if (tier === "--help" || tier === "-h") {
  console.log("usage: node tools/check.mjs quick|lint|no-default|fmt|size|cross [target]|linux [filter]|full|test <filter>");
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

if (tier === "lint") {
  // A separate tier from `quick`: clippy is the semantic gate, but it is
  // deliberately not part of every test run. CI runs it on Windows and Linux so
  // the `cfg(unix)` half and the Win32 half are both linted.
  console.log("check: running cargo clippy --all-targets -- -D warnings");
  const result = cargo(
    ["clippy", "--all-targets", "--message-format", "short", "--", "-D", "warnings"],
    COMPILE_TIMEOUT_MS,
  );
  if (timedOut(result)) {
    reportTimeout(result);
    process.exit(124);
  }
  if (result.status === 0) {
    console.log("check: ok (clippy; all targets)");
    process.exit(0);
  }
  console.log("check: FAILED (clippy)");
  console.log(failureTail(result));
  process.exit(result.status ?? 1);
}

/// Every Rust source the fmt gate covers.
function rustSources() {
  const out = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.name.endsWith(".rs")) {
        out.push(relative(REPO, full).split("\\").join("/"));
      }
    }
  };
  walk(join(REPO, "src"));
  walk(join(REPO, "tests"));
  return out.sort();
}

if (tier === "fmt") {
  const files = rustSources();
  console.log(`check: running rustfmt --check on ${files.length} files`);
  const result = spawnSync(process.execPath, [join(HERE, "fmt.mjs"), "--check", ...files], {
    cwd: REPO,
    encoding: "utf8",
    timeout: COMPILE_TIMEOUT_MS,
  });
  if (result.error && result.error.code !== "ETIMEDOUT") {
    console.error("check: cannot run fmt.mjs: " + result.error.message);
    process.exit(2);
  }
  if (timedOut(result)) {
    reportTimeout(result);
    process.exit(124);
  }
  if (result.status === 0) {
    console.log(`check: ok (fmt; ${files.length} files)`);
    process.exit(0);
  }
  console.log("check: FAILED (fmt)");
  console.log(failureTail(result));
  process.exit(1);
}

if (tier === "size") {
  // 文件预算棘轮（#239）：新文件 ≤ 700 生产 / 1,200 总行；白名单只降不升。
  console.log("check: running size-lint (src/**/*.rs)");
  const result = spawnSync(process.execPath, [join(HERE, "size-lint.mjs")], {
    cwd: REPO,
    encoding: "utf8",
    timeout: COMPILE_TIMEOUT_MS,
  });
  if (result.error && result.error.code !== "ETIMEDOUT") {
    console.error("check: cannot run size-lint.mjs: " + result.error.message);
    process.exit(2);
  }
  if (timedOut(result)) {
    reportTimeout(result);
    process.exit(124);
  }
  for (const line of lines(result.stdout).concat(lines(result.stderr))) console.log(line);
  if (result.status === 0) console.log("check: ok (size)");
  else console.log("check: FAILED (size)");
  process.exit(result.status ?? 1);
}

if (tier === "guardrail") {
  // Every guardrail ships its own injection test: the check must go red on a
  // deliberately broken input before it earns a place here.
  let status = 0;
  for (const script of ["check_spk.mjs", "check-console.mjs"]) {
    console.log("check: running node tools/" + script + " --self-test");
    const result = spawnSync(process.execPath, [join(HERE, script), "--self-test"], {
      cwd: REPO,
      encoding: "utf8",
      env: process.env,
      timeout: COMPILE_TIMEOUT_MS,
    });
    if (result.error && result.error.code !== "ETIMEDOUT") {
      console.error("check: cannot run " + script + ": " + result.error.message);
      process.exit(2);
    }
    if (timedOut(result)) {
      reportTimeout(result);
      process.exit(124);
    }
    for (const line of lines(result.stdout).concat(lines(result.stderr))) console.log(line);
    if (result.status !== 0) status = result.status ?? 1;
  }
  console.log(status === 0 ? "check: ok (guardrail)" : "check: FAILED (guardrail)");
  process.exit(status);
}

if (tier === "no-default") {
  console.log("check: running cargo check --no-default-features --bin svcd");
  const result = cargo(
    ["check", "--no-default-features", "--bin", "svcd", "--message-format", "short"],
    COMPILE_TIMEOUT_MS,
  );
  if (timedOut(result)) {
    reportTimeout(result);
    process.exit(124);
  }
  if (result.status !== 0) {
    console.log("check: FAILED (no default features)");
    console.log(failureTail(result));
    process.exit(result.status ?? 1);
  }
  // 警告计数守卫（#194）：`lan` 的 `allow(dead_code)` 曾在搬家时丢失，多出 12 条
  // 警告却没人发现。计数只许降不许升，超了就打印新增的那些行。
  const warnings = lines(result.stdout)
    .concat(lines(result.stderr))
    .filter((line) => line.includes(": warning: "));
  if (warnings.length > NO_DEFAULT_WARNING_BUDGET) {
    console.log(
      `check: FAILED (no default features): ${warnings.length} warnings > budget ${NO_DEFAULT_WARNING_BUDGET}`,
    );
    // 无法判断哪几条是新出现的，打印全量（当前 40 行以内），让人自己核对。
    console.log(warnings.join("\n"));
    process.exit(1);
  }
  console.log(
    `check: ok (no default features; warnings ${warnings.length}/${NO_DEFAULT_WARNING_BUDGET})`,
  );
  process.exit(0);
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

if (tier === "test" || tier === "full" || tier === "integration") {
  if (tier === "full") {
    // Guardrail injection tests run before the suite: a checker that silently
    // stopped rejecting bad input is a failure, not a warning.
    const guard = spawnSync(process.execPath, [join(HERE, "check_spk.mjs"), "--self-test"], {
      cwd: REPO,
      encoding: "utf8",
      env: process.env,
      timeout: COMPILE_TIMEOUT_MS,
    });
    for (const line of lines(guard.stdout).concat(lines(guard.stderr))) console.log(line);
    if (guard.status !== 0) {
      console.log("check: FAILED (guardrail)");
      process.exit(guard.status ?? 1);
    }
  }
  const parallel = rest.includes("--parallel");
  const filter = tier === "test" ? rest.filter((arg) => arg !== "--parallel") : [];
  if (tier === "integration") process.exit(runIntegration());
  if (tier === "full" && !parallel) {
    // Unit tests are in-process and isolated per root; the integration half
    // runs each target in its own cargo invocation so one hung target only
    // spends its own timeout. The Linux tier runs the same steps in WSL.
    const integration = integrationSteps();
    if (!integration) process.exit(2);
    process.exit(
      runSteps([unitStep(), ...integration], (args) => runTestTier(args)),
    );
  }
  const args = ["test", ...filter, "--no-fail-fast"];
  if (!parallel) args.push("--", "--test-threads=1");
  process.exit(runTestTier(args));
}

console.error(
  "check: unknown tier " +
    tier +
    "; use quick|no-default|cross [target]|linux [filter]|integration|full [--parallel]|test <filter> [--parallel]",
);
process.exit(2);
