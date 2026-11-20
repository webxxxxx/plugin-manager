#!/usr/bin/env node
// size-lint: 文件预算棘轮（#239）。
//
// 规则：
//   - 统计 src/**/*.rs 的生产行数（扣掉 \`#[cfg(test)] mod tests\` 及其后内容）
//     与总行数；
//   - 新文件生产 ≤ 700 行且总行数 ≤ 1,200 行；
//   - 存量超预算文件登记在 tools/size-budget.json，记录当前值，只许降不许升；
//   - 未登记文件超预算、或白名单文件变大 → 退出 1；--report 只打印，退出 0。
//
// 用法：
//   node tools/size-lint.mjs                 人读输出；违规退出 1
//   node tools/size-lint.mjs --report        只打印（超预算/白名单文件），退出 0
//   node tools/size-lint.mjs --selftest      注入用例：坏输入先红，恢复后绿
//   node tools/size-lint.mjs --src <dir> --budget <file>   诊断/自测用
//
// 不设最小行数：这个门只防“文件无限长”，不奖励把模块拆成一行一段的碎片。
// 小文件本身不是问题，给行数设下限只会逼实现加样板、注释或空洞的抽象。

import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");
const DEFAULT_SRC = join(REPO, "src");
const DEFAULT_BUDGET = join(HERE, "size-budget.json");

const PRODUCTION_BUDGET = 700;
const TOTAL_BUDGET = 1200;

function parseArgs(argv) {
  const args = { report: false, selftest: false, src: DEFAULT_SRC, budget: DEFAULT_BUDGET };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--report") args.report = true;
    else if (arg === "--selftest") args.selftest = true;
    else if (arg === "--src" || arg === "--budget") {
      const value = argv[(index += 1)];
      if (!value) {
        console.error(`size-lint: ${arg} 需要一个路径`);
        process.exit(2);
      }
      args[arg.slice(2)] = resolve(value);
    } else {
      console.error(`size-lint: 未知参数 ${arg}`);
      process.exit(2);
    }
  }
  return args;
}

/** 白名单：{ files: { "src/x.rs": { production, total } } }。缺文件按空名单处理。 */
function loadBudget(path) {
  const files = new Map();
  if (!existsSync(path)) return { files };
  let parsed;
  try {
    parsed = JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    console.error(`size-lint: ${path} 不是合法 JSON: ${error.message}`);
    process.exit(2);
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    console.error(`size-lint: ${path} 顶层必须是对象`);
    process.exit(2);
  }
  const entries = parsed.files ?? {};
  if (typeof entries !== "object" || Array.isArray(entries)) {
    console.error(`size-lint: ${path} 的 files 必须是对象`);
    process.exit(2);
  }
  for (const [file, value] of Object.entries(entries)) {
    const valid =
      value &&
      typeof value === "object" &&
      Number.isInteger(value.production) &&
      value.production >= 0 &&
      Number.isInteger(value.total) &&
      value.total >= value.production;
    if (!valid) {
      console.error(
        `size-lint: ${path} 的 ${file} 必须记录非负整数 production/total，且 total >= production`,
      );
      process.exit(2);
    }
    files.set(file, { production: value.production, total: value.total });
  }
  return { files };
}

const CFG_TEST_LINE = /^\s*#\[cfg\(.*\btest\b.*\)\]\s*(?:\/\/.*)?$/;

/** cfg(test) 与 mod tests 之间允许空行/注释/属性；遇到代码就停。 */
function nextCodeLine(lines, start) {
  let next = start;
  while (
    next < lines.length &&
    (/^\s*$/.test(lines[next]) ||
      /^\s*\/\//.test(lines[next]) ||
      /^\s*#\[/.test(lines[next]))
  ) {
    next += 1;
  }
  return next;
}

/**
 * 生产行 = 文件总行数减去内联测试模块（`#[cfg(test)]` / `#[cfg(all(test, ...))]`
 * 后接 `mod tests`）及其后的所有内容。
 * 外置测试文件（父模块只写 `#[cfg(test)] mod tests;`）生产行为 0，总行数照常计入。
 */
function measureSource(text, testOnly = false) {
  const lines = text.split(/\r?\n/);
  const total = lines.length > 0 && lines[lines.length - 1] === "" ? lines.length - 1 : lines.length;
  if (testOnly) return { production: 0, total };
  for (let index = 0; index < lines.length; index += 1) {
    if (!CFG_TEST_LINE.test(lines[index])) continue;
    const next = nextCodeLine(lines, index + 1);
    if (next < lines.length && /^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+tests\b/.test(lines[next])) {
      return { production: index, total };
    }
  }
  return { production: total, total };
}

/** `mod tests;` 的落点：`foo.rs` → `foo/tests.rs`，`foo/mod.rs` → `foo/tests.rs`。 */
function externalTestsPath(file) {
  if (file.endsWith("/mod.rs")) return file.slice(0, -"mod.rs".length) + "tests.rs";
  return file.slice(0, -".rs".length) + "/tests.rs";
}

/** 父模块里出现 `#[cfg(test)] mod tests;` 即认定外置测试文件。 */
function declaresExternalTests(text) {
  const lines = text.split(/\r?\n/);
  for (let index = 0; index < lines.length; index += 1) {
    if (!CFG_TEST_LINE.test(lines[index])) continue;
    const next = nextCodeLine(lines, index + 1);
    if (next < lines.length && /^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+tests\s*;/.test(lines[next])) {
      return true;
    }
  }
  return false;
}

function collectSources(srcDir) {
  const out = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.name.endsWith(".rs")) out.push(full);
    }
  };
  walk(srcDir);
  return out.sort((left, right) => left.localeCompare(right));
}

function sourceKey(srcDir, full) {
  return "src/" + relative(srcDir, full).split("\\").join("/");
}

/** 纯函数式的评估：selftest 直接注入文件内容，不依赖真实仓库。 */
function evaluate(srcDir, budget) {
  const rows = [];
  const violations = [];
  const sources = new Map();
  for (const full of collectSources(srcDir)) {
    sources.set(sourceKey(srcDir, full), readFileSync(full, "utf8"));
  }
  // 外置测试文件由父模块的 `#[cfg(test)] mod tests;` 声明，生产行按 0 计。
  const testOnly = new Set();
  for (const [file, text] of sources) {
    if (!declaresExternalTests(text)) continue;
    const target = externalTestsPath(file);
    if (sources.has(target)) testOnly.add(target);
  }
  for (const [file, text] of sources) {
    const { production, total } = measureSource(text, testOnly.has(file));
    const allowed = budget.files.get(file) ?? null;
    const values = { production, total };
    if (allowed) {
      for (const metric of ["production", "total"]) {
        if (values[metric] > allowed[metric]) {
          violations.push({
            file,
            kind: "growth",
            metric,
            value: values[metric],
            recorded: allowed[metric],
          });
        }
      }
    } else {
      for (const metric of ["production", "total"]) {
        const limit = metric === "production" ? PRODUCTION_BUDGET : TOTAL_BUDGET;
        if (values[metric] > limit) {
          violations.push({ file, kind: "over-budget", metric, value: values[metric], limit });
        }
      }
    }
    rows.push({ file, production, total, allowed });
  }
  const stale = [...budget.files.keys()].filter((file) => !sources.has(file)).sort();
  return { rows, violations, stale };
}

function violationLine(violation) {
  if (violation.kind === "growth") {
    const delta = violation.value - violation.recorded;
    return `  ${violation.file}: ${violation.metric} ${violation.value} > baseline ${violation.recorded} (+${delta})`;
  }
  return `  ${violation.file}: ${violation.metric} ${violation.value} > budget ${violation.limit}`;
}

function rowLine(row) {
  const baseline = row.allowed
    ? `baseline ${row.allowed.production}/${row.allowed.total}`
    : "no baseline";
  return `  ${row.file}  production ${row.production}, total ${row.total}  (${baseline})`;
}

function printReport(result, budget, reportOnly) {
  console.log(
    `size: ${result.rows.length} files; budget production ${PRODUCTION_BUDGET} / total ${TOTAL_BUDGET}; ` +
      `${budget.files.size} whitelisted`,
  );
  if (reportOnly) {
    const interesting = result.rows
      .filter((row) => row.allowed || row.production > PRODUCTION_BUDGET || row.total > TOTAL_BUDGET)
      .sort((left, right) => right.production - left.production || right.total - left.total);
    for (const row of interesting) console.log(rowLine(row));
    if (result.stale.length > 0) console.log(`  stale whitelist entries: ${result.stale.join(", ")}`);
    console.log("size: report only (no gate)");
    return;
  }
  for (const violation of result.violations) console.log(violationLine(violation));
  if (result.stale.length > 0) console.log(`  stale whitelist entries: ${result.stale.join(", ")}`);
  if (result.violations.length > 0) {
    console.log(`size: FAILED (${result.violations.length} violation(s))`);
  } else {
    console.log(`size: ok (${result.rows.length} files; ${budget.files.size} whitelisted, only down)`);
  }
}

function syntheticSource(productionLines, testBodyLines = 0) {
  const lines = [];
  for (let index = 0; index < productionLines; index += 1) lines.push(`// production ${index + 1}`);
  if (testBodyLines > 0) {
    lines.push("#[cfg(test)]", "mod tests {");
    for (let index = 0; index < testBodyLines; index += 1) lines.push(`    // test ${index + 1}`);
    lines.push("}");
  }
  return lines.join("\n") + "\n";
}

function syntheticTestBody(lineCount) {
  return Array.from({ length: lineCount }, (_, index) => `// test ${index + 1}`).join("\n") + "\n";
}

function writeFixture(root, files, budget) {
  rmSync(join(root, "src"), { recursive: true, force: true });
  mkdirSync(join(root, "src"), { recursive: true });
  for (const [file, content] of Object.entries(files)) {
    const full = join(root, file);
    mkdirSync(dirname(full), { recursive: true });
    writeFileSync(full, content);
  }
  writeFileSync(join(root, "size-budget.json"), JSON.stringify({ files: budget }, null, 2) + "\n");
}

function runCli(args) {
  return spawnSync(process.execPath, [fileURLToPath(import.meta.url), ...args], {
    encoding: "utf8",
    timeout: 60 * 1000,
  });
}

function selftest() {
  let total = 0;
  let failures = 0;
  const check = (name, condition, detail) => {
    total += 1;
    const ok = Boolean(condition);
    console.log(`selftest: ${ok ? "ok" : "FAILED"}  ${name}`);
    if (!ok) {
      failures += 1;
      if (detail) console.log(`  ${detail}`);
    }
  };

  const root = mkdtempSync(join(tmpdir(), "svcd-size-lint-"));
  const srcDir = join(root, "src");
  const budgetPath = join(root, "size-budget.json");
  const cliArgs = ["--src", srcDir, "--budget", budgetPath];
  try {
    // 1) 新增文件生产超预算：坏输入红，恢复后绿。
    writeFixture(root, { "src/fresh.rs": syntheticSource(701) }, {});
    let result = evaluate(srcDir, loadBudget(budgetPath));
    check(
      "新增文件生产 701 行 -> 红",
      result.violations.some(
        (v) => v.file === "src/fresh.rs" && v.kind === "over-budget" && v.metric === "production",
      ),
      JSON.stringify(result.violations),
    );
    writeFixture(root, { "src/fresh.rs": syntheticSource(700) }, {});
    result = evaluate(srcDir, loadBudget(budgetPath));
    check("恢复到生产 700 行 -> 绿", result.violations.length === 0, JSON.stringify(result.violations));

    // 2) 白名单文件变大 1 行：坏输入红，恢复后绿。
    writeFixture(
      root,
      { "src/known.rs": syntheticSource(710) },
      { "src/known.rs": { production: 710, total: 710 } },
    );
    result = evaluate(srcDir, loadBudget(budgetPath));
    check("白名单基线本身 -> 绿", result.violations.length === 0, JSON.stringify(result.violations));
    writeFixture(
      root,
      { "src/known.rs": syntheticSource(711) },
      { "src/known.rs": { production: 710, total: 710 } },
    );
    result = evaluate(srcDir, loadBudget(budgetPath));
    check(
      "白名单文件长大 1 行 -> 红",
      result.violations.some((v) => v.file === "src/known.rs" && v.kind === "growth"),
      JSON.stringify(result.violations),
    );
    writeFixture(
      root,
      { "src/known.rs": syntheticSource(710) },
      { "src/known.rs": { production: 710, total: 710 } },
    );
    result = evaluate(srcDir, loadBudget(budgetPath));
    check("白名单文件改回基线 -> 绿", result.violations.length === 0, JSON.stringify(result.violations));

    // 3) 总行数单独超预算。
    writeFixture(root, { "src/wide.rs": syntheticSource(600, 700) }, {});
    result = evaluate(srcDir, loadBudget(budgetPath));
    check(
      "总行数 1,303 -> 红",
      result.violations.some(
        (v) => v.file === "src/wide.rs" && v.kind === "over-budget" && v.metric === "total",
      ),
      JSON.stringify(result.violations),
    );
    writeFixture(root, { "src/wide.rs": syntheticSource(600, 500) }, {});
    result = evaluate(srcDir, loadBudget(budgetPath));
    check("总行数降到 1,103 -> 绿", result.violations.length === 0, JSON.stringify(result.violations));

    // 4) 内联测试模块不计生产行，但仍计入总行数。
    writeFixture(root, { "src/unit.rs": syntheticSource(700, 400) }, {});
    result = evaluate(srcDir, loadBudget(budgetPath));
    check(
      "生产 700 + 测试 403 行 -> 绿（生产不吃测试行）",
      result.violations.length === 0,
      JSON.stringify(result.violations),
    );

    // 5) 外置 tests.rs 生产按 0 计；总行数仍受 1,200 预算。
    writeFixture(
      root,
      {
        "src/parent.rs": "#[cfg(test)]\nmod tests;\n",
        "src/parent/tests.rs": syntheticTestBody(800),
      },
      {},
    );
    result = evaluate(srcDir, loadBudget(budgetPath));
    check(
      "外置 tests.rs 800 行 -> 绿（生产按 0 计）",
      result.violations.length === 0,
      JSON.stringify(result.violations),
    );
    writeFixture(
      root,
      {
        "src/parent.rs": "#[cfg(test)]\nmod tests;\n",
        "src/parent/tests.rs": syntheticTestBody(1301),
      },
      {},
    );
    result = evaluate(srcDir, loadBudget(budgetPath));
    check(
      "外置 tests.rs 1,301 行 -> 总预算红",
      result.violations.some(
        (v) => v.file === "src/parent/tests.rs" && v.kind === "over-budget" && v.metric === "total",
      ),
      JSON.stringify(result.violations),
    );

    // 6) 真实 CLI：坏输入退出 1，--report 只打印退出 0，恢复后退出 0。
    writeFixture(root, { "src/cli-check.rs": syntheticSource(701) }, {});
    let cli = runCli(cliArgs);
    check(
      "CLI 坏输入退出 1",
      cli.status === 1 && cli.stdout.includes("src/cli-check.rs"),
      `status=${cli.status} stdout=${cli.stdout}`,
    );
    cli = runCli([...cliArgs, "--report"]);
    check(
      "CLI --report 坏输入退出 0 且仍打印",
      cli.status === 0 && cli.stdout.includes("src/cli-check.rs"),
      `status=${cli.status} stdout=${cli.stdout}`,
    );
    writeFixture(root, { "src/cli-check.rs": syntheticSource(700) }, {});
    cli = runCli(cliArgs);
    check(
      "CLI 恢复后退出 0",
      cli.status === 0 && cli.stdout.includes("size: ok"),
      `status=${cli.status} stdout=${cli.stdout}`,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }

  console.log(`selftest: ${total - failures}/${total} 通过`);
  return failures;
}

const args = parseArgs(process.argv.slice(2));
if (args.selftest) {
  process.exit(selftest() === 0 ? 0 : 1);
}
const budget = loadBudget(args.budget);
const result = evaluate(args.src, budget);
printReport(result, budget, args.report);
process.exit(args.report || result.violations.length === 0 ? 0 : 1);
