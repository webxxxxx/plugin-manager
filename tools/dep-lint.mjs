#!/usr/bin/env node
// dep-lint: 把 docs/architecture.md 的结构规则变成可运行的检查。
//
// 规则（对应文档 §7 四条依赖不变量与 §4 文件名纪律）：
//   1. 层方向：调用者的层 rank 必须 >= 被调用者；反向即违规。
//   2. 循环依赖：模块图必须是 DAG；报告强连通分量，并给出一条具体短环。
//   3. 文件名后缀：协议名只该在 server/ 出现一次（_http / _handler / _manager / utils）。
//
// 用法：
//   node tools/dep-lint.mjs              人读输出；有违规退出码 1
//   node tools/dep-lint.mjs --json       机器可读（CI 用）
//   node tools/dep-lint.mjs --no-fail    已知基线模式：始终退出 0
//   node tools/dep-lint.mjs --map <p>    指定映射文件
//
// 不做：model/check/ops 三层检查（代码还没分层，会全是假阳性），也不改任何代码。

import { readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

function parseArgs(argv) {
  const args = { json: false, fail: true, map: join(HERE, "arch-map.json"), src: join(REPO, "src") };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--json") args.json = true;
    else if (arg === "--no-fail") args.fail = false;
    else if (arg === "--map") args.map = resolve(argv[(index += 1)]);
    else if (arg === "--src") args.src = resolve(argv[(index += 1)]);
    else {
      console.error(`dep-lint: 未知参数 ${arg}`);
      process.exit(2);
    }
  }
  return args;
}

function loadMap(path) {
  const map = JSON.parse(readFileSync(path, "utf8"));
  const rank = new Map(map.layers.map((layer) => [layer.name, layer.rank]));
  const modules = new Map(Object.entries(map.modules));
  return { map, rank, modules };
}

/** 只扫 src/ 顶层的 .rs；目录重排后再扩展成递归。 */
function scanSources(srcDir, modules) {
  const files = readdirSync(srcDir)
    .filter((name) => name.endsWith(".rs"))
    .sort();
  const edges = [];
  const unknown = [];
  for (const name of files) {
    const caller = name.slice(0, -3);
    if (!modules.has(caller)) unknown.push(name);
    const lines = stripComments(readFileSync(join(srcDir, name), "utf8")).split("\n");
    for (let line = 0; line < lines.length; line += 1) {
      for (const match of lines[line].matchAll(/crate::([a-z_0-9]+)/g)) {
        const callee = match[1];
        if (callee === caller || !modules.has(callee)) continue;
        edges.push({ from: caller, to: callee, file: `src/${name}`, line: line + 1 });
      }
    }
  }
  return { files, edges, unknown };
}

/**
 * 遮罩 Rust 注释与字符串字面量（换成空格，换行一个不丢，所以行号永远准确）。
 * 目的：文档注释或测试字符串里提到 `crate::foo` 不算依赖。
 * 状态机覆盖：行注释、块注释、普通字符串、原始字符串 r#"…"# / br#"…"#、字符字面量。
 */
function stripComments(source) {
  const lines = [];
  let current = [];
  let state = "code";
  let rawHashes = 0;
  const emit = (char, keep) => {
    if (char === "\n") {
      lines.push(current.join(""));
      current = [];
    } else {
      current.push(keep ? char : " ");
    }
  };
  let index = 0;
  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];
    if (state === "line") {
      if (char === "\n") state = "code";
      emit(char, char === "\n");
      index += 1;
      continue;
    }
    if (state === "block") {
      if (char === "*" && next === "/") {
        emit(char, false);
        emit(next, false);
        index += 2;
        state = "code";
        continue;
      }
      emit(char, char === "\n");
      index += 1;
      continue;
    }
    if (state === "string" || state === "char") {
      const quote = state === "string" ? '"' : "'";
      if (char === "\\") {
        emit(char, false);
        if (next !== undefined) emit(next, false);
        index += 2;
        continue;
      }
      if (char === quote) state = "code";
      emit(char, char === "\n");
      index += 1;
      continue;
    }
    if (state === "raw") {
      const terminator = `"${"#".repeat(rawHashes)}`;
      if (source.startsWith(terminator, index)) {
        for (const piece of terminator) emit(piece, false);
        index += terminator.length;
        state = "code";
        continue;
      }
      emit(char, char === "\n");
      index += 1;
      continue;
    }
    // state === "code"
    if (char === "/" && next === "/") {
      state = "line";
      index += 2;
      continue;
    }
    if (char === "/" && next === "*") {
      state = "block";
      index += 2;
      continue;
    }
    const raw = /^(?:b?r)(#*)"/.exec(source.slice(index, index + 24));
    if (raw) {
      rawHashes = raw[1].length;
      for (const piece of raw[0]) emit(piece, false);
      index += raw[0].length;
      state = "raw";
      continue;
    }
    if (char === '"') {
      state = "string";
      emit(char, false);
      index += 1;
      continue;
    }
    // 字符字面量 vs 生命周期：'x' 是字符，'a 是生命周期。
    if (char === "'") {
      if (next === "\\" || source[index + 2] === "'") {
        state = "char";
        emit(char, false);
        index += 1;
        continue;
      }
    }
    emit(char, true);
    index += 1;
  }
  lines.push(current.join(""));
  return lines.join("\n");
}

function layerViolations(edges, modules, rank) {
  const out = [];
  for (const edge of edges) {
    const from = modules.get(edge.from);
    const to = modules.get(edge.to);
    const fromRank = rank.get(from.layer);
    const toRank = rank.get(to.layer);
    if (fromRank < toRank) {
      out.push({ ...edge, fromLayer: from.layer, toLayer: to.layer });
    }
  }
  return out;
}

/** Tarjan：返回所有强连通分量（含单点）。 */
function stronglyConnected(nodes, adjacency) {
  let index = 0;
  const indexes = new Map();
  const low = new Map();
  const stack = [];
  const onStack = new Set();
  const components = [];

  const visit = (node) => {
    indexes.set(node, index);
    low.set(node, index);
    index += 1;
    stack.push(node);
    onStack.add(node);
    for (const next of adjacency.get(node) ?? []) {
      if (!indexes.has(next)) {
        visit(next);
        low.set(node, Math.min(low.get(node), low.get(next)));
      } else if (onStack.has(next)) {
        low.set(node, Math.min(low.get(node), indexes.get(next)));
      }
    }
    if (low.get(node) === indexes.get(node)) {
      const component = [];
      for (;;) {
        const member = stack.pop();
        onStack.delete(member);
        component.push(member);
        if (member === node) break;
      }
      components.push(component.sort());
    }
  };
  for (const node of nodes) if (!indexes.has(node)) visit(node);
  return components;
}

/** 在分量内找一条穿过 start 的最短环，用来把"39 个模块成环"变成具体证据。 */
function shortestCycle(start, adjacency, members) {
  const queue = [[start]];
  const seen = new Set([start]);
  while (queue.length > 0) {
    const path = queue.shift();
    const tail = path[path.length - 1];
    for (const next of adjacency.get(tail) ?? []) {
      if (!members.has(next)) continue;
      if (next === start && path.length > 1) return [...path, start];
      if (seen.has(next)) continue;
      seen.add(next);
      queue.push([...path, next]);
    }
  }
  return null;
}

function cycleViolations(nodes, edges) {
  const adjacency = new Map();
  for (const edge of edges) {
    if (!adjacency.has(edge.from)) adjacency.set(edge.from, new Set());
    adjacency.get(edge.from).add(edge.to);
  }
  const components = stronglyConnected(nodes, adjacency).filter((members) => members.length > 1);
  return components.map((members) => {
    const set = new Set(members);
    let cycle = null;
    for (const member of members) {
      cycle = shortestCycle(member, adjacency, set);
      if (cycle) break;
    }
    return { members, cycle };
  });
}

function suffixViolations(files, map) {
  const errors = [];
  const warnings = [];
  for (const name of files) {
    const base = name.slice(0, -3);
    for (const suffix of map.suffixErrors ?? []) {
      if (base !== suffix && base.endsWith(suffix)) errors.push({ file: `src/${name}`, suffix });
    }
    for (const suffix of map.suffixWarnings ?? []) {
      if (base !== suffix && base.endsWith(suffix)) warnings.push({ file: `src/${name}`, suffix });
    }
  }
  return { errors, warnings };
}

function report({ map, modules, files, edges, unknown, layers, cycles, suffixes }) {
  const lines = [];
  lines.push("dep-lint · 观测基线（只读，不改代码）");
  lines.push(`模块 ${files.length} 个，引用边 ${edges.length} 条，映射 ${modules.size} 项`);
  const byLayer = new Map();
  for (const entry of modules.values()) byLayer.set(entry.layer, (byLayer.get(entry.layer) ?? 0) + 1);
  lines.push(
    "层分布：" +
      [...byLayer.entries()]
        .sort((a, b) => a[0].localeCompare(b[0]))
        .map(([layer, count]) => `${layer} ${count}`)
        .join(" · "),
  );
  if (unknown.length > 0) lines.push(`⚠ 未映射的模块：${unknown.join(", ")}`);

  const pairs = new Map();
  for (const item of layers) {
    const key = `${item.from} → ${item.to}`;
    pairs.set(key, (pairs.get(key) ?? 0) + 1);
  }
  lines.push("");
  lines.push(`[V1] 层方向：下层调用上层（${layers.length} 次调用 / ${pairs.size} 组）`);
  if (pairs.size > 0) {
    lines.push(
      "  " +
        [...pairs.entries()]
          .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
          .map(([key, count]) => `${key}${count > 1 ? ` ×${count}` : ""}`)
          .join(" · "),
    );
  }
  for (const item of layers) {
    lines.push(
      `  ${item.file}:${item.line}  ${item.from}(${item.fromLayer}) → ${item.to}(${item.toLayer})`,
    );
  }
  if (layers.length === 0) lines.push("  （无）");

  lines.push("");
  lines.push(`[V2] 循环依赖（${cycles.length} 个强连通分量）`);
  for (const component of cycles) {
    lines.push(`  ${component.members.length} 个模块成环：${component.members.join(", ")}`);
    if (component.cycle) lines.push(`    例：${component.cycle.join(" → ")}`);
  }
  if (cycles.length === 0) lines.push("  （无）");

  lines.push("");
  lines.push(`[V3] 文件名后缀：协议名只该在 server/ 出现一次（${suffixes.errors.length}）`);
  for (const item of suffixes.errors) lines.push(`  ${item.file}  后缀 ${item.suffix}`);
  if (suffixes.errors.length === 0) lines.push("  （无）");
  if (suffixes.warnings.length > 0) {
    lines.push(`  警告（不计入失败）：${suffixes.warnings.map((item) => item.file).join(", ")}`);
  }

  const total = layers.length + cycles.length + suffixes.errors.length;
  lines.push("");
  lines.push(`合计 ${total} 条违规（层方向 ${layers.length} · 循环分量 ${cycles.length} · 后缀 ${suffixes.errors.length}）`);
  return { text: lines.join("\n"), total };
}

const args = parseArgs(process.argv.slice(2));
const { map, rank, modules } = loadMap(args.map);
const { files, edges, unknown } = scanSources(args.src, modules);
const nodes = [...modules.keys()].filter((name) => files.includes(`${name}.rs`));
const layers = layerViolations(edges, modules, rank);
const cycles = cycleViolations(nodes, edges);
const suffixes = suffixViolations(files, map);
const { text, total } = report({ map, modules, files, edges, unknown, layers, cycles, suffixes });

if (args.json) {
  console.log(JSON.stringify({ total, layers, cycles, suffixes, unknown, files: files.length, edges: edges.length }, null, 2));
} else {
  console.log(text);
}
process.exit(total > 0 && args.fail ? 1 : 0);
