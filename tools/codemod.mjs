#!/usr/bin/env node
// codemod: apply a literal-edit plan, then prove the tree still compiles.
//
// A wide refactor (retype a result type, rename a symbol) is one mechanical
// change with hundreds of call sites. Hand-written `str.replace` scripts have
// silently mangled files before (#151): a global replace drops a paren, or eats
// the error type out of every `Result<_, _>`. This tool makes the destructive
// step explicit and reversible.
//
//   node tools/codemod.mjs plan.json             dry run: report, write nothing
//   node tools/codemod.mjs plan.json --apply     write, then run the gate
//   node tools/codemod.mjs plan.json --apply --verify "cargo check --quiet"
//   node tools/codemod.mjs plan.json --apply --skip-verify
//   node tools/codemod.mjs --selftest            exercise this tool itself
//
// Plan format:
//
//   {
//     "edits": [
//       { "file": "src/package.rs", "old": "Result<_, String>", "new": "Result<_>", "count": 66 },
//       { "file": "src/plugin.rs", "old": "return Err(error);", "new": "return Err(error.into());" }
//     ]
//   }
//
// Every `old` must appear exactly `count` times in the file as it stands at that
// point in the batch (`count` defaults to 1). One mismatch aborts the whole
// batch before anything is written: a replace that matches nothing, or matches
// more than declared, is the signal that the plan and the tree disagree.
//
// After writing, the gate runs (`cargo check --message-format=short
// --all-targets` by default). A red gate restores every file byte for byte and
// prints the first compiler errors, so a broken batch never reaches `git`.

import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const DEFAULT_ROOT = resolve(HERE, "..");
const DEFAULT_VERIFY = "cargo check --message-format=short --all-targets";
const ERROR_CAP = 30;

function usage() {
  return [
    "usage: node tools/codemod.mjs <plan.json> [--apply] [--verify <cmd>] [--skip-verify] [--root <dir>]",
    "       node tools/codemod.mjs --selftest",
  ].join("\n");
}

function parseArgs(argv) {
  const args = { plan: null, apply: false, verify: DEFAULT_VERIFY, verifySet: false, skipVerify: false, root: DEFAULT_ROOT, selftest: false };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--apply") args.apply = true;
    else if (arg === "--skip-verify") args.skipVerify = true;
    else if (arg === "--selftest") args.selftest = true;
    else if (arg === "--verify") {
      args.verify = argv[(index += 1)] ?? "";
      args.verifySet = true;
    } else if (arg === "--root") args.root = resolve(argv[(index += 1)] ?? ".");
    else if (arg.startsWith("--")) throw new Error(`unknown argument ${arg}`);
    else if (args.plan === null) args.plan = arg;
    else throw new Error(`unexpected argument ${arg}`);
  }
  return args;
}

function loadPlan(path) {
  const plan = JSON.parse(readFileSync(path, "utf8"));
  if (!Array.isArray(plan.edits) || plan.edits.length === 0) {
    throw new Error("plan needs a non-empty `edits` array");
  }
  for (const [index, edit] of plan.edits.entries()) {
    if (typeof edit.file !== "string" || edit.file === "") {
      throw new Error(`edit ${index}: missing file`);
    }
    if (typeof edit.old !== "string" || edit.old === "") {
      throw new Error(`edit ${index}: missing old`);
    }
    if (typeof edit.new !== "string") throw new Error(`edit ${index}: missing new`);
    if (edit.count !== undefined && (!Number.isInteger(edit.count) || edit.count < 1)) {
      throw new Error(`edit ${index}: count must be a positive integer`);
    }
  }
  return plan;
}

/// Apply the whole batch in memory. Files map to `{before, after}`; problems are
/// collected with the number of occurrences actually found.
function planBatch(root, plan) {
  const files = new Map();
  const problems = [];
  for (const [index, edit] of plan.edits.entries()) {
    const path = resolve(root, edit.file);
    if (!files.has(path)) {
      let before;
      try {
        before = readFileSync(path, "utf8");
      } catch {
        problems.push(`edit ${index}: ${edit.file} does not exist`);
        continue;
      }
      files.set(path, { before, after: before });
    }
    const entry = files.get(path);
    const expected = edit.count ?? 1;
    const found = entry.after.split(edit.old).length - 1;
    if (found !== expected) {
      problems.push(
        `edit ${index}: ${edit.file} contains ${JSON.stringify(edit.old.slice(0, 60))} ${found}×, plan declares ${expected}×`,
      );
      continue;
    }
    entry.after = entry.after.split(edit.old).join(edit.new);
  }
  return { files, problems };
}

function writeFiles(files) {
  for (const [path, entry] of files) {
    if (entry.after !== entry.before) writeFileSync(path, entry.after);
  }
}

function restoreFiles(files) {
  for (const [path, entry] of files) {
    if (entry.after !== entry.before) writeFileSync(path, entry.before);
  }
}

function changedFiles(files) {
  return [...files.entries()].filter(([, entry]) => entry.after !== entry.before);
}

/// Run the gate; return `{ok, output}`. The command is a shell line so it can
/// carry flags, and it runs from the repo root (or `--root`).
function runGate(command, root) {
  const result = spawnSync(command, {
    cwd: root,
    shell: true,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.error) return { ok: false, output: `codemod: cannot run gate: ${result.error.message}` };
  return { ok: result.status === 0, output: `${result.stdout ?? ""}${result.stderr ?? ""}` };
}

function gateErrors(output) {
  const lines = output.split(/\r?\n/).filter(Boolean);
  const selected = lines.filter(
    (line) => /^error(\[|:)/.test(line) || /:\d+:\d+: /.test(line) || line.includes("could not compile"),
  );
  return (selected.length > 0 ? selected : lines).slice(0, ERROR_CAP);
}

/// One full pass. Returns the exit code and writes human output through `log`.
function run(options, log = console.log) {
  const plan = typeof options.plan === "string" ? loadPlan(options.plan) : options.plan;
  const { files, problems } = planBatch(options.root, plan);
  const changed = changedFiles(files);
  const edits = plan.edits.length;

  if (problems.length > 0) {
    for (const problem of problems) log(`codemod: ${problem}`);
    log("codemod: batch aborted, nothing written");
    return 1;
  }
  if (changed.length === 0) {
    log(`codemod: ${edits} edit(s) matched but changed nothing`);
    return 0;
  }
  log(`codemod: ${edits} edit(s) across ${changed.length} file(s)`);
  for (const [path, entry] of changed) {
    const before = entry.before.split("\n").length;
    const after = entry.after.split("\n").length;
    log(`  ${path.slice(options.root.length + 1).split("\\").join("/")}  ${before} → ${after} lines`);
  }
  if (!options.apply) {
    log("codemod: dry run, nothing written (--apply writes and runs the gate)");
    return 0;
  }

  writeFiles(files);
  if (options.skipVerify) {
    log("codemod: wrote, gate skipped (--skip-verify)");
    return 0;
  }
  const gate = runGate(options.verify, options.root);
  if (gate.ok) {
    log(`codemod: wrote, gate ok (${options.verify})`);
    return 0;
  }
  restoreFiles(files);
  log(`codemod: gate FAILED (${options.verify}); restored ${changed.length} file(s)`);
  for (const line of gateErrors(gate.output)) log(`  ${line}`);
  return 1;
}

const SELFTEST_EDITS = [
  { file: "a.rs", old: "Result<_, String>", new: "Result<_>", count: 2 },
  { file: "a.rs", old: "Ok(())", new: "Ok::<(), ProblemError>(())", count: 2 },
];

function selftest() {
  const root = mkdtempSync(join(tmpdir(), "svcd-codemod-"));
  const source = "fn a() -> Result<_, String> { Ok(()) }\nfn b() -> Result<_, String> { Ok(()) }\n";
  let failures = 0;
  const check = (name, ok, detail) => {
    if (ok) {
      console.log(`selftest: ok  ${name}`);
    } else {
      failures += 1;
      console.log(`selftest: FAILED  ${name}${detail ? `\n  ${detail}` : ""}`);
    }
  };
  const quiet = () => {};
  try {
    writeFileSync(join(root, "a.rs"), source);

    // 1. A count mismatch aborts before writing anything.
    const wrong = { edits: [{ file: "a.rs", old: "Result<_, String>", new: "Result<_>", count: 3 }] };
    const messages = [];
    const wrongCode = run({ root, plan: wrong, apply: true, verify: "node -e process.exit(0)" }, (line) => messages.push(line));
    check(
      "count mismatch aborts",
      wrongCode === 1 && readFileSync(join(root, "a.rs"), "utf8") === source && messages.some((line) => line.includes("3×")),
      messages.join(" | "),
    );

    // 2. Dry run reports but leaves the file alone.
    const dryCode = run({ root, plan: { edits: SELFTEST_EDITS }, apply: false, verify: "node -e process.exit(0)" }, quiet);
    check("dry run writes nothing", dryCode === 0 && readFileSync(join(root, "a.rs"), "utf8") === source);

    // 3. Apply with a green gate rewrites every declared occurrence.
    const greenCode = run({ root, plan: { edits: SELFTEST_EDITS }, apply: true, verify: "node -e process.exit(0)" }, quiet);
    const applied = readFileSync(join(root, "a.rs"), "utf8");
    check(
      "apply rewrites occurrences",
      greenCode === 0 &&
        applied ===
          "fn a() -> Result<_> { Ok::<(), ProblemError>(()) }\nfn b() -> Result<_> { Ok::<(), ProblemError>(()) }\n",
      applied,
    );

    // 4. A red gate restores the file byte for byte.
    writeFileSync(join(root, "a.rs"), source);
    const redCode = run({ root, plan: { edits: SELFTEST_EDITS }, apply: true, verify: "node -e process.exit(1)" }, quiet);
    check("red gate restores", redCode === 1 && readFileSync(join(root, "a.rs"), "utf8") === source);

    // 5. An old string that is not there is a mismatch, not a silent no-op.
    const absent = { edits: [{ file: "a.rs", old: "not present", new: "x" }] };
    check("absent old aborts", run({ root, plan: absent, apply: true, verify: "true" }, quiet) === 1);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
  console.log(`selftest: ${5 - failures}/5 通过`);
  return failures === 0 ? 0 : 1;
}

let args;
try {
  args = parseArgs(process.argv.slice(2));
} catch (error) {
  console.error(`codemod: ${error.message}`);
  console.error(usage());
  process.exit(2);
}

if (args.selftest) process.exit(selftest());
if (args.plan === null) {
  console.error(usage());
  process.exit(2);
}

try {
  process.exit(
    run(
      {
        root: args.root,
        plan: args.plan,
        apply: args.apply,
        verify: args.verify,
        skipVerify: args.skipVerify,
      },
      console.log,
    ),
  );
} catch (error) {
  console.error(`codemod: ${error.message}`);
  process.exit(2);
}
