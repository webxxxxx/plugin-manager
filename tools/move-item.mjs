#!/usr/bin/env node
// move-item: move whole Rust items from one file to another, then prove the
// tree still compiles.
//
// The wide-refactor doc has `codemod` for literal edits; "move these fifteen
// items into a new module" had no primitive, so every slice was a hand-written
// Python extractor. Two of those broke (#193): a `#[cfg(unix)]` /
// `#[cfg(not(unix))]` pair collapsed to one item because both share a name,
// and a re-export block between two items was swallowed into the target file.
// This tool makes those two failures impossible.
//
//   node tools/move-item.mjs plan.json             dry run: report, write nothing
//   node tools/move-item.mjs plan.json --apply     write, then run the gate
//   node tools/move-item.mjs plan.json --apply --skip-verify
//   node tools/move-item.mjs --selftest            exercise the item scanner
//
// Plan format:
//
//   {
//     "moves": [
//       {
//         "from": "src/manage/package.rs",
//         "to": "src/manage/model/ustar.rs",
//         "header": "//! ustar codec.\n\nuse std::io::Write;\n",
//         "items": ["TarEntry", "read_tar", "impl TarPlan"]
//       }
//     ]
//   }
//
// An item is a top-level `fn` / `struct` / `enum` / `trait` / `impl` / `const`
// / `static` / `type` / `mod`, together with the `///` and `#[...]` lines
// directly above it. `impl` names are matched by prefix: `impl TarPlan` finds
// `impl TarPlan` and `impl<'a> Tar<'a>`, while `impl Iterator for Tar` finds
// `impl<'a> Iterator for Tar<'a>`.
//
// A name that occurs more than once is refused, with every line listed: that is
// the `#[cfg]` pair this tool exists to catch. Disambiguate with `name@LINE`
// (any line inside the item) when two copies really should both move.

import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");
const GATE_TIMEOUT_MS = 10 * 60 * 1000;

function fail(message) {
  console.error("move-item: " + message);
  process.exit(2);
}

function parseArgs(argv) {
  const args = { plan: "", apply: false, skipVerify: false, selftest: false, verify: "" };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--apply") args.apply = true;
    else if (arg === "--skip-verify") args.skipVerify = true;
    else if (arg === "--selftest") args.selftest = true;
    else if (arg === "--verify") {
      args.verify = argv[(index += 1)] ?? "";
      if (!args.verify) fail("--verify needs a command");
    } else if (arg.startsWith("-")) fail("unknown argument " + arg);
    else if (!args.plan) args.plan = arg;
    else fail("unexpected argument " + arg);
  }
  if (!args.plan && !args.selftest) {
    fail("usage: node tools/move-item.mjs <plan.json> [--apply] [--verify \"cmd\"]");
  }
  return args;
}

// ---------------------------------------------------------------------------
// Item scanner
// ---------------------------------------------------------------------------

const ITEM_START =
  /^(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(fn|struct|enum|trait|impl|const|static|type|mod)\b/;

/// `///` and `#[...]` directly above an item belong to it; a blank line or code
/// ends the run (so two items never share their docs).
function isPrefixLine(line) {
  return /^\s*(#\[|\/\/\/|\/\/[^!/])/.test(line);
}

function isCharStart(text, index) {
  if (text[index] !== "'") return false;
  const next = text[index + 1];
  if (next === "\\") return text[index + 3] === "'";
  return text[index + 2] === "'";
}

function isRawStart(text, index) {
  if (text[index] !== "r") return false;
  if (index > 0 && /[A-Za-z0-9_]/.test(text[index - 1])) return false;
  let cursor = index + 1;
  while (text[cursor] === "#") cursor += 1;
  return text[cursor] === '"';
}

/// Last line of the item that starts at `startLine`. Braces/parens/brackets are
/// counted outside comments, strings, chars and raw strings, so `"{ }"` inside a
/// doc string or a literal cannot end the item early.
function scanEnd(lines, startLine) {
  const text = lines.join("\n");
  const lineStarts = [0];
  for (let index = 0; index < text.length; index += 1) {
    if (text[index] === "\n") lineStarts.push(index + 1);
  }
  const lineAt = (offset) => {
    let low = 0;
    let high = lineStarts.length - 1;
    while (low < high) {
      const mid = (low + high + 1) >> 1;
      if (lineStarts[mid] <= offset) low = mid;
      else high = mid - 1;
    }
    return low;
  };

  let cursor = lineStarts[startLine];
  let state = "code";
  let blockDepth = 0;
  let rawHashes = 0;
  let braces = 0;
  let parens = 0;
  let brackets = 0;
  let sawBrace = false;

  while (cursor < text.length) {
    const char = text[cursor];
    const next = text[cursor + 1];

    if (state === "line") {
      if (char === "\n") state = "code";
      cursor += 1;
      continue;
    }
    if (state === "block") {
      if (char === "/" && next === "*") {
        blockDepth += 1;
        cursor += 2;
        continue;
      }
      if (char === "*" && next === "/") {
        blockDepth -= 1;
        cursor += 2;
        if (blockDepth === 0) state = "code";
        continue;
      }
      cursor += 1;
      continue;
    }
    if (state === "str" || state === "char") {
      if (char === "\\") cursor += 2;
      else if ((state === "str" && char === '"') || (state === "char" && char === "'")) {
        state = "code";
        cursor += 1;
      } else cursor += 1;
      continue;
    }
    if (state === "raw") {
      if (char === '"') {
        let hashes = 0;
        while (text[cursor + 1 + hashes] === "#") hashes += 1;
        if (hashes === rawHashes) {
          state = "code";
          cursor += 1 + rawHashes;
          continue;
        }
      }
      cursor += 1;
      continue;
    }

    // state === "code"
    if (char === "/" && next === "/") {
      state = "line";
      cursor += 2;
      continue;
    }
    if (char === "/" && next === "*") {
      state = "block";
      blockDepth = 1;
      cursor += 2;
      continue;
    }
    if (isRawStart(text, cursor)) {
      let hashes = 0;
      let probe = cursor + 1;
      while (text[probe] === "#") {
        hashes += 1;
        probe += 1;
      }
      rawHashes = hashes;
      state = "raw";
      cursor = probe + 1;
      continue;
    }
    if (char === "r" && (next === '"' || next === "#")) {
      // handled by isRawStart above
    }
    if (char === '"') {
      state = "str";
      cursor += 1;
      continue;
    }
    if (isCharStart(text, cursor)) {
      state = "char";
      cursor += 1;
      continue;
    }
    if (char === "{") {
      braces += 1;
      sawBrace = true;
    } else if (char === "}") {
      braces -= 1;
      if (sawBrace && braces === 0 && parens === 0 && brackets === 0) {
        return lineAt(cursor);
      }
    } else if (char === "(") parens += 1;
    else if (char === ")") parens -= 1;
    else if (char === "[") brackets += 1;
    else if (char === "]") brackets -= 1;
    else if (char === ";" && !sawBrace && braces === 0 && parens === 0 && brackets === 0) {
      return lineAt(cursor);
    }
    cursor += 1;
  }
  return lines.length - 1;
}

function collapse(text) {
  return text.replace(/\s+/g, " ").trim();
}

/// Canonical item name used for plan matching.
function itemName(headLine) {
  const kind = headLine.match(ITEM_START)?.[1];
  if (!kind) return "";
  if (kind === "impl") {
    let rest = collapse(headLine.slice(headLine.indexOf("impl") + 4));
    rest = rest.replace(/\s*\{.*$/, "").trim();
    // Drop the generics right after `impl` (`impl<'a> Tar<'a>` -> `Tar<'a>`)
    if (rest.startsWith("<")) {
      let depth = 0;
      let end = -1;
      for (let index = 0; index < rest.length; index += 1) {
        if (rest[index] === "<") depth += 1;
        else if (rest[index] === ">") {
          depth -= 1;
          if (depth === 0) {
            end = index;
            break;
          }
        }
      }
      if (end >= 0) rest = rest.slice(end + 1).trim();
    }
    return "impl " + rest;
  }
  const after = headLine.slice(headLine.indexOf(kind) + kind.length).trim();
  const name = after.split(/[<({\s:]/)[0];
  return kind + " " + name;
}

function findItems(lines) {
  const items = [];
  let inBlockComment = false;
  for (let line = 0; line < lines.length; line += 1) {
    const text = lines[line];
    if (inBlockComment) {
      if (text.includes("*/")) inBlockComment = false;
      continue;
    }
    if (/^\s*\/\*/.test(text) && !text.includes("*/")) {
      inBlockComment = true;
      continue;
    }
    if (!/^\S/.test(text) || !ITEM_START.test(text)) continue;
    if (/^impl\b/.test(text) && !text.includes("{")) continue; // `impl Trait for X` wrapped
    const start = (() => {
      let probe = line;
      while (probe > 0 && isPrefixLine(lines[probe - 1])) probe -= 1;
      return probe;
    })();
    const name = itemName(text);
    const short = name.replace(/^(fn|struct|enum|trait|const|static|type|mod) /, "");
    items.push({ name, short, start, end: scanEnd(lines, line), line: line + 1 });
  }
  return items;
}

/// Resolve `items` requests against the scanned items. Throws with a readable
/// message on a missing or ambiguous request.
function selectItems(items, requests) {
  const chosen = [];
  for (const request of requests) {
    const at = request.lastIndexOf("@");
    const wantsLine = at > 0 && /^\d+$/.test(request.slice(at + 1));
    const name = wantsLine ? request.slice(0, at) : request;
    const line = wantsLine ? Number(request.slice(at + 1)) : null;
    // Exact first; then a prefix that ends on `<` or a space, so
    // `impl Iterator for Tar` matches `impl Iterator for Tar<'a>` without
    // letting `fn foo` match `fn foobar`.
    const matchesName = (candidate) => {
      if (candidate === name) return true;
      if (!candidate.startsWith(name)) return false;
      const next = candidate[name.length];
      return next === "<" || next === " ";
    };
    const kindPrefixed = /^(fn|struct|enum|trait|const|static|type|mod|impl)\s/.test(name);
    let matches = items.filter((item) =>
      kindPrefixed ? matchesName(item.name) : matchesName(item.name) || matchesName(item.short),
    );
    if (matches.length === 0) {
      throw new Error("item not found: " + name);
    }
    if (line !== null) matches = matches.filter((item) => item.start + 1 <= line && line <= item.end + 1);
    if (matches.length === 0) {
      throw new Error("item not found at the given line: " + request);
    }
    if (matches.length > 1) {
      const where = matches.map((item) => `line ${item.line} (${item.name})`).join(", ");
      throw new Error(
        `${name} occurs ${matches.length} times: ${where}; this is usually a #[cfg] pair — move each half explicitly with ${name}@LINE`,
      );
    }
    chosen.push(matches[0]);
  }
  return chosen;
}

/// Remove the chosen items from `lines`, collapsing runs of blank lines.
function removeItems(lines, chosen) {
  const doomed = new Set();
  for (const item of chosen) {
    for (let line = item.start; line <= item.end; line += 1) doomed.add(line);
  }
  const kept = lines.filter((_, index) => !doomed.has(index));
  const out = [];
  for (const line of kept) {
    if (line.trim() === "" && out.length > 0 && out[out.length - 1].trim() === "") continue;
    out.push(line);
  }
  while (out.length > 0 && out[out.length - 1].trim() === "") out.pop();
  return out;
}

function itemText(lines, item) {
  return lines.slice(item.start, item.end + 1).join("\n");
}

// ---------------------------------------------------------------------------
// Plan execution
// ---------------------------------------------------------------------------

function readPlan(path) {
  const plan = JSON.parse(readFileSync(path, "utf8"));
  if (!Array.isArray(plan.moves) || plan.moves.length === 0) {
    fail("plan has no moves");
  }
  for (const move of plan.moves) {
    if (!move.from || !move.to) fail("every move needs from/to");
    if (!Array.isArray(move.items) || move.items.length === 0) {
      fail("every move needs a non-empty items list");
    }
  }
  return plan;
}

function applyMove(move, dryRun) {
  const fromPath = resolve(REPO, move.from);
  if (!existsSync(fromPath)) fail("missing source file: " + move.from);
  const fromText = readFileSync(fromPath, "utf8");
  const fromLines = fromText.split("\n");

  let chosen;
  try {
    chosen = selectItems(findItems(fromLines), move.items);
  } catch (error) {
    fail(`${move.from}: ${error.message}`);
  }

  // Keep plan order: `items` decides the order in the target file.
  const ordered = move.items.map((_, index) => chosen[index]);
  const moved = ordered.map((item) => itemText(fromLines, item)).join("\n\n");
  const remaining = removeItems(fromLines, ordered);
  const toPath = resolve(REPO, move.to);
  const exists = existsSync(toPath);
  const header = exists ? "" : move.header ?? "";

  console.log(`move-item: ${move.from} -> ${move.to}`);
  for (const item of ordered) {
    console.log(`  ${item.name}  (lines ${item.start + 1}-${item.end + 1})`);
  }
  console.log(`  target: ${exists ? "append" : "create"}${exists ? "" : ` (header ${header.length} bytes)`}`);
  console.log(`  source: ${fromLines.length} -> ${remaining.length} lines`);

  if (dryRun) return;

  const targetText = exists ? `${readFileSync(toPath, "utf8").trimEnd()}\n\n${moved}\n` : `${header}${moved}\n`;
  writeFileSync(toPath, targetText);
  writeFileSync(fromPath, remaining.join("\n") + "\n");
  console.log("  wrote");
}

function runGate(args) {
  if (args.skipVerify) {
    console.log("move-item: gate skipped (--skip-verify)");
    return true;
  }
  const command = args.verify || "cargo check --all-targets";
  console.log(`move-item: gate: ${command}`);
  const result = spawnSync(command, {
    cwd: REPO,
    shell: true,
    encoding: "utf8",
    timeout: GATE_TIMEOUT_MS,
  });
  if (result.status === 0) {
    console.log("move-item: gate ok");
    return true;
  }
  const tail = `${result.stdout ?? ""}\n${result.stderr ?? ""}`
    .split("\n")
    .filter(Boolean)
    .slice(-40)
    .join("\n");
  console.log("move-item: gate FAILED");
  console.log(tail);
  return false;
}

function applyPlan(plan, args) {
  // Everything is planned before anything is written: a bad item list must
  // leave both trees untouched.
  for (const move of plan.moves) {
    const fromPath = resolve(REPO, move.from);
    if (!existsSync(fromPath)) fail("missing source file: " + move.from);
    const lines = readFileSync(fromPath, "utf8").split("\n");
    try {
      selectItems(findItems(lines), move.items);
    } catch (error) {
      fail(`${move.from}: ${error.message}`);
    }
  }

  if (!args.apply) {
    for (const move of plan.moves) applyMove(move, true);
    console.log("move-item: dry run; pass --apply to write");
    return 0;
  }

  const backups = [];
  for (const move of plan.moves) {
    for (const path of [resolve(REPO, move.from), resolve(REPO, move.to)]) {
      backups.push({ path, text: existsSync(path) ? readFileSync(path, "utf8") : null });
    }
  }
  try {
    for (const move of plan.moves) applyMove(move, false);
  } catch (error) {
    fail(String(error));
  }

  if (runGate(args)) return 0;

  for (const backup of backups) {
    if (backup.text === null) {
      rmSync(backup.path, { force: true });
    } else {
      mkdirSync(dirname(backup.path), { recursive: true });
      writeFileSync(backup.path, backup.text);
    }
  }
  console.log("move-item: restored both files after a failed gate");
  return 1;
}

// ---------------------------------------------------------------------------
// Self-test
// ---------------------------------------------------------------------------

const SELFTEST = [
  {
    name: "doc + attribute travel with the item",
    source: [
      "fn before() {}",
      "",
      "/// Doc line.",
      "#[cfg(unix)]",
      "fn probe() {",
      "    let _ = 1;",
      "}",
      "",
      "fn after() {}",
      "",
    ].join("\n"),
    request: "fn probe",
    expectLines: [3, 7],
    expectItem: true,
  },
  {
    name: "braces inside strings do not end the item",
    source: ['fn probe() {', '    let s = "}";', '    let r = r#"{"#;', "}", ""].join("\n"),
    request: "fn probe",
    expectLines: [1, 4],
    expectItem: true,
  },
  {
    name: "impl generics are stripped from the name",
    source: ["impl<'a> Iterator for Tar<'a> {", "    fn next(&mut self) {}", "}", ""].join("\n"),
    request: "impl Iterator for Tar",
    expectLines: [1, 3],
    expectItem: true,
  },
  {
    name: "a bare type name finds the item",
    source: ["struct TarEntry {", "    path: String,", "}", ""].join("\n"),
    request: "TarEntry",
    expectLines: [1, 3],
    expectItem: true,
  },
  {
    name: "a cfg pair is refused with both lines",
    source: [
      "#[cfg(unix)]",
      "fn set_mode() {}",
      "",
      "#[cfg(not(unix))]",
      "fn set_mode() {}",
      "",
    ].join("\n"),
    request: "fn set_mode",
    expectError: "occurs 2 times",
  },
  {
    name: "@LINE disambiguates one half of a cfg pair",
    source: [
      "#[cfg(unix)]",
      "fn set_mode() {}",
      "",
      "#[cfg(not(unix))]",
      "fn set_mode() {}",
      "",
    ].join("\n"),
    request: "fn set_mode@5",
    expectLines: [4, 5],
    expectItem: true,
  },
];

function selftest() {
  let failures = 0;
  for (const item of SELFTEST) {
    let ok = false;
    let got = "";
    try {
      const [chosen] = selectItems(findItems(item.source.split("\n")), [item.request]);
      got = `lines ${chosen.start + 1}-${chosen.end + 1}`;
      ok =
        !item.expectError &&
        chosen.start + 1 === item.expectLines[0] &&
        chosen.end + 1 === item.expectLines[1];
    } catch (error) {
      got = error.message;
      ok = Boolean(item.expectError) && error.message.includes(item.expectError);
    }
    console.log(`selftest: ${ok ? "ok" : "FAILED"}  ${item.name}`);
    if (!ok) {
      failures += 1;
      console.log(`  expected ${item.expectError ?? JSON.stringify(item.expectLines)}, got ${got}`);
    }
  }
  console.log(`selftest: ${SELFTEST.length - failures}/${SELFTEST.length} 通过`);
  return failures;
}

const args = parseArgs(process.argv.slice(2));
if (args.selftest) {
  process.exit(selftest() === 0 ? 0 : 1);
}
process.exit(applyPlan(readPlan(args.plan), args));
