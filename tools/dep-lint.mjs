#!/usr/bin/env node
// dep-lint: 把 docs/architecture.md 的结构规则变成可运行的检查。
//
// 规则（对应文档 §7 四条依赖不变量与 §4 文件名纪律）：
//   1. 层方向：调用者的层 rank 必须 >= 被调用者；反向即违规。
//   2. 循环依赖：模块图必须是 DAG；报告强连通分量，并给出一条具体短环。
//   3. 文件名后缀：协议名只该在 server/ 出现一次（_http / _handler / _manager / utils）。
//   4. 映射卫生：映射里的模块必须在 src/ 存在；src/ 的顶层模块必须已映射；
//      `files` 的每个路径必须在磁盘上存在、每个 value 必须是合法模块键；
//      每个 src/*.rs 都必须登记（新文件要显式选模块，不许静默回退）。
//   5. seam 不变量：子进程 stdio 的句柄与管道只在 src/wire/contract/stdio 里出现；
//      `Stdio::piped` 只在显式 allowlist（已登记的直接调用方）里出现，名单外一律违规。
//   6. 失败类别：`#[cfg(test)]` 之外不按 `detail` / `message` 的文案分支或比较
//      （S5 的投影不变量，见 docs/architecture.md §8）。
//
// 用法：
//   node tools/dep-lint.mjs              人读输出；有违规退出码 1
//   node tools/dep-lint.mjs --json       机器可读（CI 用）
//   node tools/dep-lint.mjs --no-fail    已知基线模式：始终退出 0
//   node tools/dep-lint.mjs --map <p>    指定映射文件
//   node tools/dep-lint.mjs --selftest   只跑规则自身的用例（不扫仓库）
//
// 不做：model/check/ops 三层检查（代码还没分层，会全是假阳性），也不改任何代码。

import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..");

function parseArgs(argv) {
  const args = { json: false, fail: true, selftest: false, map: join(HERE, "arch-map.json"), src: join(REPO, "src") };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--json") args.json = true;
    else if (arg === "--no-fail") args.fail = false;
    else if (arg === "--selftest") args.selftest = true;
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
  // 文件 → 逻辑模块（`files`）。搬家会改路径但不会改模块身份：列出的文件按
  // 映射取键；`files` 覆盖全部 src/*.rs（V4 强制），未登记的路径第一段只作为
  // selftest 的回退用例存在。
  const fileModules = new Map(
    Object.entries(map.files ?? {}).map(([file, moduleName]) => [normalizeFileKey(file), moduleName]),
  );
  return { map, rank, modules, fileModules };
}

function normalizeFileKey(file) {
  return file.split("\\").join("/");
}

/// 文件 → 逻辑模块：先查 `files`，落空按路径第一段（`src/kernel/x.rs` → `kernel`）。
function resolveFileModule(file, fileModules) {
  return (
    fileModules.get(file) ??
    file.replace(/^src\//, "").split("/")[0].replace(/\.rs$/, "")
  );
}

/// `files` 自身的卫生：键必须存在，值必须是合法模块键。`exists` 由调用方注入，
/// 这样 selftest 不需要真文件。
function partitionFileModules(fileModules, modules, exists) {
  const missing = [];
  const unknown = [];
  for (const [file, name] of fileModules) {
    if (!exists(file)) {
      missing.push(file);
      continue;
    }
    if (!modules.has(name)) unknown.push({ file, name });
  }
  return { missing, unknown };
}

function collectSources(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) collectSources(full, out);
    else if (entry.name.endsWith(".rs")) out.push(full);
  }
  return out;
}

/**
 * 递归扫 src/。模块身份取路径第一段（src/kernel/contract.rs → kernel），
 * 因为本工具的粒度是"概念模块"，不是单个文件。
 */
function scanSources(srcDir, modules, fileModules) {
  const sources = collectSources(srcDir)
    .sort()
    .map((full) => {
      const relative = full.slice(srcDir.length + 1).split("\\").join("/");
      const base = relative.slice(relative.lastIndexOf("/") + 1, -3);
      const file = `src/${relative}`;
      const module = resolveFileModule(file, fileModules);
      return { full, file, base, module, explicit: fileModules.has(file) };
    });
  const edges = [];
  const unknown = [];
  for (const source of sources) {
    if (!modules.has(source.module)) unknown.push(source.file);
    const lines = stripComments(readFileSync(source.full, "utf8")).split("\n");
    for (let line = 0; line < lines.length; line += 1) {
      for (const match of lines[line].matchAll(/crate::([a-z_0-9]+)/g)) {
        const callee = match[1];
        if (callee === source.module || !modules.has(callee)) continue;
        edges.push({ from: source.module, to: callee, file: source.file, line: line + 1 });
      }
    }
  }
  return { sources, edges, unknown };
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

function suffixViolations(sources, map) {
  const errors = [];
  const warnings = [];
  for (const source of sources) {
    for (const suffix of map.suffixErrors ?? []) {
      if (source.base !== suffix && source.base.endsWith(suffix)) {
        errors.push({ file: source.file, suffix });
      }
    }
    for (const suffix of map.suffixWarnings ?? []) {
      if (source.base !== suffix && source.base.endsWith(suffix)) {
        warnings.push({ file: source.file, suffix });
      }
    }
  }
  return { errors, warnings };
}

/// V5：seam 的机械不变量。stdio seam 拥有子进程 stdio 的父子句柄；调用方描述
/// 用途（Plan/Target）而不是自己取句柄或开管道。句柄规则用文件 allowlist，
/// 让 reactor 测试里自建管道这类 fixture 不被误判。
const SEAM_RULES = [
  {
    name: "stdio 的父子句柄",
    pattern: /\bChildStd(?:in|out|err)\b/,
    allowed: (file) =>
      file === "src/wire/contract/stdio.rs" ||
      file.startsWith("src/wire/contract/stdio/"),
    hint: "子进程 stdio 的句柄归 src/wire/contract/stdio；调用方用 Plan/Target。",
  },
  {
    name: "stdio 的调用方直接开管道",
    pattern: /\bStdio::piped\b/,
    allowed: (file) =>
      file === "src/wire/contract/stdio.rs" ||
      file.startsWith("src/wire/contract/stdio/"),
    allow: [
      // 已登记的直接调用方：seam 的同步 API 还没覆盖这两种用途。
      "src/manage/check/interpreter.rs",
      "src/manage/ops/config.rs",
      // reactor 的测试 fixture 自建管道。
      "src/wire/contract/reactor.rs",
    ],
    hint: "调用方问 src/wire/contract/stdio 要管道（Plan::notify / exec / hook / stop_hook）；新调用方要么改走 seam，要么登记到 allow 并说明理由。",
  },
];

function seamViolationsIn(file, text) {
  const out = [];
  for (const rule of SEAM_RULES) {
    if (rule.allow && rule.allow.includes(file)) continue;
    if (rule.allowed && rule.allowed(file)) continue;
    const lines = stripComments(text).split("\n");
    for (let line = 0; line < lines.length; line += 1) {
      if (rule.pattern.test(lines[line])) {
        out.push({ rule: rule.name, file, line: line + 1, hint: rule.hint });
      }
    }
  }
  return out;
}

function seamViolations(sources) {
  return sources.flatMap((source) =>
    seamViolationsIn(source.file, readFileSync(source.full, "utf8")),
  );
}

/// V6：失败类别的不变量（#149–#151）。生产代码按 `Problem` 类别决定行为；`detail`
/// 与 `message` 是给人看的文案，拿它们做分支或比较等于把状态判断藏回字符串里。
/// 测试里断言文案是允许的，所以规则只看 `#[cfg(test)]` 之外。
const CATEGORY_RULES = [
  {
    name: "按 detail 分支",
    pattern: /\.detail(?:\(\))?\s*\.\s*(?:contains|starts_with|ends_with|find|split|rsplit)\b/,
    hint: "按 Problem 类别投影（face/support.rs 一张表）；detail 只用于展示。",
  },
  {
    name: "按 detail 比较",
    pattern: /\.detail(?:\(\))?\s*(?:==|!=)/,
    hint: "按 Problem 类别比较，不要比较 detail 文案。",
  },
  {
    name: "按 message 分支",
    pattern: /\bmessage\s*\.\s*(?:contains|starts_with|ends_with|find)\b/,
    hint: "message 是给人看的文案；状态读 error.data.kind / Problem。",
  },
  {
    name: "按 message 比较",
    pattern: /\bmessage\s*(?:==|!=)/,
    hint: "message 是给人看的文案；状态读 error.data.kind / Problem。",
  },
];

/// `#[cfg(test)]` 覆盖的字节区间。属性修饰 mod 时吃到配对的 `}`，修饰单个条目时
/// 吃到 `;`。调用方传的是遮罩过字符串与注释的文本，所以括号计数永远安全。
function testRegions(text) {
  // A test-only file (declared by `#[cfg(test)] mod tests;` in its parent) cannot
  // carry a per-item attribute, so it marks itself with an inner `#![cfg(test)]`.
  if (text.trimStart().startsWith("#![cfg(test)]")) return [[0, text.length]];
  const regions = [];
  let index = 0;
  for (;;) {
    const at = text.indexOf("#[cfg(test)]", index);
    if (at === -1) break;
    index = at + "#[cfg(test)]".length;
    const opener = itemOpener(text, index);
    if (opener) regions.push([at, opener.end]);
  }
  return regions;
}

/// 从属性后面找条目本体：`{` 开 block（配对的 `}` 是终点），`;` 结 statement。
/// `(`/`[` 里的行列不算本体，`fn foo() {` 才不会在参数表里停住。
function itemOpener(text, from) {
  let brackets = 0;
  for (let index = from; index < text.length; index += 1) {
    const char = text[index];
    if (char === "(" || char === "[") brackets += 1;
    else if (char === ")" || char === "]") brackets = Math.max(0, brackets - 1);
    else if (brackets === 0 && char === "{") {
      const end = matchingBrace(text, index);
      return end === -1 ? null : { end };
    } else if (brackets === 0 && char === ";") return { end: index };
  }
  return null;
}

function matchingBrace(text, open) {
  let depth = 0;
  for (let index = open; index < text.length; index += 1) {
    if (text[index] === "{") depth += 1;
    else if (text[index] === "}") {
      depth -= 1;
      if (depth === 0) return index;
    }
  }
  return -1;
}

/// 1-based 行号：二分找最后一个 start <= offset 的行首。
function lineNumberAt(lineStarts, offset) {
  let low = 0;
  let high = lineStarts.length - 1;
  while (low < high) {
    const mid = (low + high + 1) >> 1;
    if (lineStarts[mid] <= offset) low = mid;
    else high = mid - 1;
  }
  return low + 1;
}

function categoryViolationsIn(file, text) {
  const masked = stripComments(text);
  const lineStarts = [0];
  for (let index = 0; index < masked.length; index += 1) {
    if (masked[index] === "\n") lineStarts.push(index + 1);
  }
  const covered = new Set();
  for (const [start, end] of testRegions(masked)) {
    const first = lineNumberAt(lineStarts, start);
    const last = lineNumberAt(lineStarts, end);
    for (let line = first; line <= last; line += 1) covered.add(line);
  }
  const lines = masked.split("\n");
  const out = [];
  for (let line = 0; line < lines.length; line += 1) {
    if (covered.has(line + 1)) continue;
    for (const rule of CATEGORY_RULES) {
      if (rule.pattern.test(lines[line])) {
        out.push({ rule: rule.name, file, line: line + 1, hint: rule.hint });
      }
    }
  }
  return out;
}

function categoryViolations(sources) {
  return sources.flatMap((source) =>
    categoryViolationsIn(source.file, readFileSync(source.full, "utf8")),
  );
}

/// V7：单点组装。同一份「两三个调用拼起来的判定或投影」只许拼一次；第二次
/// 拼接就是 #215–#219 那一类重复。每条规则列一组成员正则，并给出允许点：
/// 一条 fn 里命中的成员数 ≥ `min`（默认 1，即「只许在这里调用」）、且不在
/// 允许点，就报违规。允许点可以只给文件（整文件放行），也可以精确到函数名。
/// 成员定义本身也算命中，所以定义所在的文件或函数要进允许点。
const ASSEMBLY_RULES = [
  {
    name: "静态资源条件请求投影",
    members: [/\betag\(/, /\bweb_headers\(/, /\bnot_modified\(/],
    min: 2,
    allow: [{ file: "src/face/webui.rs", item: "asset_validators" }],
    hint: "ETag/304 只经 face/webui.rs 的 asset_validators 组装（#218）。",
  },
  {
    name: "单调毫秒 → epoch 秒的锚定",
    members: [/\.epoch_of\(/],
    allow: [{ file: "src/kernel/projection.rs" }],
    hint: "锚定只许 kernel/projection.rs 的 anchored_secs*（#215）。",
  },
  {
    name: "epoch 秒 → 单调毫秒的锚定",
    members: [/\.mono_of\(/],
    allow: [{ file: "src/kernel/projection.rs" }],
    hint: "反向锚定只许 kernel/projection.rs 的 anchored_mono_ms（#216）。",
  },
  {
    name: "pid 创建时间探针",
    members: [/start_time\(/],
    allow: [
      { file: "src/platform/host/process.rs" },
      { file: "src/supervise/ops/host.rs", item: "process_probe" },
    ],
    hint: "starttime 只在 process_probe 里探；判定走 runtime::pid_state（#217）。",
  },
  {
    name: "pre-body 认领顺序",
    members: [/\.plugin_ui\(/, /\.package_upload\(/, /\.blob_upload\(/],
    min: 2,
    allow: [{ file: "src/face/prebody.rs", item: "claim" }],
    hint: "三条 pre-body 认领顺序只在 face/prebody.rs 的 claim 里排（#244）。",
  },
  {
    name: "插件 UI 静态准入",
    members: [/\bvalid_name\(/, /\bweb_assets\(/],
    min: 2,
    allow: [{ file: "src/face/plugin_ui.rs", item: "web_assets_for" }],
    hint: "valid_name + web_assets 只在 face/plugin_ui.rs 的 web_assets_for 里组合（#243）。",
  },
  {
    name: "插件 UI token 准入",
    members: [/\bweb_assets_for\(/, /\bvalid_token\(/],
    min: 2,
    allow: [{ file: "src/face/plugin_ui.rs", item: "static_assets" }],
    hint: "静态面的资产 + token 准入只在 face/plugin_ui.rs 的 static_assets 里组合（#243）。",
  },
  {
    name: "插件 UI 资产解析",
    members: [/\brelative_path\(/, /\bweb_file\(/, /\bwants_html\(/],
    min: 3,
    allow: [{ file: "src/face/webui.rs", item: "resolve_asset" }],
    hint: "relative_path + web_file + SPA 回退只在 face/webui.rs 的 resolve_asset 里组合（#243）。",
  },
  {
    name: "插件安装收尾",
    members: [/\bhook_pump\(/, /\.install\(service/, /\bbump_catalog\(/, /\bstatus_for_id\(/],
    min: 2,
    allow: [
      { file: "src/daemon/plugins.rs", item: "finish_install" },
      // uninstall 的收尾是 forget + bump_catalog，只和 install 共用 hook_pump
      // 与 bump_catalog；install 的收尾单点在 finish_install。
      { file: "src/daemon/plugins.rs", item: "uninstall_plugin" },
    ],
    hint: "install 收尾（install / bump_catalog / 日志 / status）只经 daemon/plugins.rs 的 finish_install（#245）。",
  },
  {
    name: "complete 端点的 parts 请求体解析",
    members: [/partNumber/, /\bfrom_slice\(/, /\bas_array\(/],
    min: 2,
    allow: [{ file: "src/face/blob_routes.rs", item: "parse_parts" }],
    hint: "complete 的 parts[] 解码只经 face/blob_routes.rs 的 parse_parts（#246）。",
  },
  {
    name: "导入成功响应的组装",
    members: [/\bconfigApplied\b/, /\bwarnings\b/, /\bsummary\.get\(/],
    min: 2,
    allow: [{ file: "src/face/install.rs", item: "import_success" }],
    hint: "导入成功响应只在 face/install.rs 的 import_success 里拼（#247）。",
  },
];

/// 生产 fn 的正文区间（入参是 `stripComments` 遮罩过的文本）。`#[cfg(test)]`
/// 区域整段跳过；正文取参数表 `)` 之后的 `{` 到配对 `}`，所以签名里的符号
/// 不会被算成组装。
function functionRegions(masked) {
  const tests = testRegions(masked);
  const inTests = (offset) => tests.some(([start, end]) => offset >= start && offset <= end);
  const out = [];
  const pattern = /\bfn\s+([A-Za-z0-9_]+)\s*(?:<[^>{}]*>)?\s*\(/g;
  for (let match = pattern.exec(masked); match; match = pattern.exec(masked)) {
    if (inTests(match.index)) continue;
    const body = functionBody(masked, match.index + match[0].length - 1);
    if (body) out.push({ name: match[1], start: match.index, body });
  }
  return out;
}

/// 从参数表的 `(` 找到配对的 `)`，再找紧跟着的本体 `{` 与其 `}`。
function functionBody(text, open) {
  let depth = 0;
  for (let index = open; index < text.length; index += 1) {
    if (text[index] === "(") depth += 1;
    else if (text[index] === ")") {
      depth -= 1;
      if (depth === 0) {
        const opener = itemOpener(text, index + 1);
        return opener ? text.slice(index + 1, opener.end) : null;
      }
    }
  }
  return null;
}

function assemblyViolationsIn(file, text) {
  const masked = stripComments(text);
  const lineStarts = [0];
  for (let index = 0; index < masked.length; index += 1) {
    if (masked[index] === "\n") lineStarts.push(index + 1);
  }
  const out = [];
  for (const fn of functionRegions(masked)) {
    for (const rule of ASSEMBLY_RULES) {
      const allowed = rule.allow.some(
        (entry) => entry.file === file && (!entry.item || entry.item === fn.name),
      );
      if (allowed) continue;
      const hits = rule.members.filter((member) => member.test(fn.body));
      if (hits.length >= (rule.min ?? 1)) {
        out.push({
          rule: rule.name,
          file,
          item: fn.name,
          line: lineNumberAt(lineStarts, fn.start),
          hint: rule.hint,
        });
      }
    }
  }
  return out;
}

function assemblyViolations(sources) {
  return sources.flatMap((source) =>
    assemblyViolationsIn(source.file, readFileSync(source.full, "utf8")),
  );
}

/// V8：装配点不含业务。src/daemon/ 只许装配、协议投影与跨域编排；监督
/// 状态机、slot 组装、运行态记录、进程派生都必须在 L3 / L0（#230）。
const COMPOSE_RULES = [
  { name: "监督状态机", pattern: /\bState::/ },
  { name: "slot 内部状态", pattern: /\bmachine\./ },
  { name: "slot 组装", pattern: /\bSlot::new\(/ },
  { name: "依赖分层", pattern: /\bstart_layers\(/ },
  { name: "运行态记录", pattern: /\bruntime::(?:save|remove)\(/ },
  { name: "进程派生", pattern: /\bCommand::spawn\(|\bStdio::/ },
  { name: "强杀槽", pattern: /\.signal_force\(/ },
];

function composeViolationsIn(file, text) {
  if (!file.startsWith("src/daemon/")) return [];
  const masked = stripComments(text);
  const lineStarts = [0];
  for (let index = 0; index < masked.length; index += 1) {
    if (masked[index] === "\n") lineStarts.push(index + 1);
  }
  const tests = testRegions(masked);
  const inTests = (offset) => tests.some(([start, end]) => offset >= start && offset <= end);
  const out = [];
  for (const rule of COMPOSE_RULES) {
    const re = new RegExp(rule.pattern.source, "g");
    for (let match = re.exec(masked); match; match = re.exec(masked)) {
      if (inTests(match.index)) continue;
      out.push({
        file,
        line: lineNumberAt(lineStarts, match.index),
        rule: rule.name,
        hint: "装配点只装配；监督逻辑放 supervise/ops（#230）。",
      });
    }
  }
  return out;
}

function composeViolations(sources) {
  return sources.flatMap((source) =>
    composeViolationsIn(source.file, readFileSync(source.full, "utf8")),
  );
}

const SELFTEST_CASES = [
  {
    name: "生产里的 detail 分支",
    source: 'fn build(error: &ProblemError) -> bool {\n    error.detail().contains("x")\n}\n',
    expected: ["按 detail 分支"],
  },
  {
    name: "生产里的 detail 比较（blob 的字段）",
    source: 'fn build(error: &blob::Error) -> bool {\n    error.detail == "x"\n}\n',
    expected: ["按 detail 比较"],
  },
  {
    name: "生产里的 message 分支",
    source: 'fn build(message: &str) -> bool {\n    message.contains("svcd")\n}\n',
    expected: ["按 message 分支"],
  },
  {
    name: "生产里的 message 比较",
    source: 'fn build(message: &str) -> bool {\n    message != "ok"\n}\n',
    expected: ["按 message 比较"],
  },
  {
    name: "mod tests 里的断言不算",
    source:
      'fn build() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        assert!(error.detail().contains("x"));\n    }\n}\n',
    expected: [],
  },
  {
    name: "单个 #[cfg(test)] 条目不算",
    source:
      '#[cfg(test)]\nfn helper(error: &ProblemError) -> bool {\n    error.detail().contains("x")\n}\n',
    expected: [],
  },
  {
    name: "字符串与注释不算",
    source:
      'fn build() {\n    // error.detail().contains("x")\n    let note = "error.detail().contains";\n    let _ = note;\n}\n',
    expected: [],
  },
  {
    name: "整个文件标了 #![cfg(test)]",
    source:
      '#![cfg(test)]\n\nfn helper(error: &ProblemError) -> bool {\n    error.detail().contains("x")\n}\n',
    expected: [],
  },
  {
    name: "测试之后的生产代码仍然算",
    source:
      '#[cfg(test)]\nmod tests {\n    fn t() { assert!(error.detail().contains("x")); }\n}\n\nfn build(message: &str) -> bool {\n    message.contains("x")\n}\n',
    expected: ["按 message 分支"],
  },
];

const SELFTEST_IDENTITY = [
  {
    name: "files 命中优先于首段回退",
    file: "src/manage/ops/scan.rs",
    files: [["src/manage/ops/scan.rs", "plugin"]],
    expected: "plugin",
  },
  {
    name: "未命中回退路径首段",
    file: "src/manage/ops/new.rs",
    files: [],
    expected: "manage",
  },
  {
    name: "Windows 分隔符先归一",
    file: normalizeFileKey("src\\manage\\ops\\scan.rs"),
    files: [["src/manage/ops/scan.rs", "plugin"]],
    expected: "plugin",
  },
];

const SELFTEST_FILE_MAP = [
  {
    name: "过期 files 键被报出",
    files: [["src/gone.rs", "kernel"]],
    exists: () => false,
    expected: { missing: ["src/gone.rs"], unknown: [] },
  },
  {
    name: "files 指向不存在的模块被报出",
    files: [["src/main.rs", "nope"]],
    exists: () => true,
    expected: { missing: [], unknown: [{ file: "src/main.rs", name: "nope" }] },
  },
  {
    name: "健康的 files 表零违规",
    files: [["src/main.rs", "api"]],
    exists: () => true,
    modules: ["api"],
    expected: { missing: [], unknown: [] },
  },
];

const SELFTEST_SEAM = [
  {
    name: "名单外的文件自己开管道算违规",
    file: "src/server/http/threads.rs",
    source: "let stderr = Stdio::piped();\n",
    expected: ["stdio 的调用方直接开管道"],
  },
  {
    name: "已登记的直接调用方不算",
    file: "src/manage/ops/config.rs",
    source: "let stderr = Stdio::piped();\n",
    expected: [],
  },
  {
    name: "stdio seam 自己不算",
    file: "src/wire/contract/stdio/platform/unix.rs",
    source: "let stderr = Stdio::piped();\n",
    expected: [],
  },
];

const SELFTEST_ASSEMBLY = [
  {
    name: "同一函数里第二次组装算违规",
    file: "src/face/other.rs",
    source:
      'fn render(metadata: &Metadata) -> Vec<(String, String)> {\n    let tag = webui::etag(modified, 1);\n    webui::web_headers(&tag)\n}\n',
    expected: ["静态资源条件请求投影"],
  },
  {
    name: "允许的组装点不算",
    file: "src/face/webui.rs",
    source:
      'fn asset_validators(metadata: &Metadata) -> bool {\n    let tag = etag(modified, 1);\n    let _ = web_headers(&tag);\n    not_modified(None, &tag)\n}\n',
    expected: [],
  },
  {
    name: "同一文件两个函数各出现一次不算",
    file: "src/face/other.rs",
    source:
      'fn build(metadata: &Metadata) -> String {\n    etag(modified, 1)\n}\nfn headers(tag: &str) -> usize {\n    web_headers(tag).len()\n}\n',
    expected: [],
  },
  {
    name: "名单外的锚定算违规",
    file: "src/daemon.rs",
    source: 'fn persist(now: Now, ms: u64) -> u64 {\n    now.epoch_of(ms)\n}\n',
    expected: ["单调毫秒 → epoch 秒的锚定"],
  },
  {
    name: "锚定留在 projection 里不算",
    file: "src/kernel/projection.rs",
    source: 'fn anchored_secs(now: Now, ms: u64) -> Option<u64> {\n    Some(now.epoch_of(ms))\n}\n',
    expected: [],
  },
  {
    name: "长相相近的标识符不算（normalize_etag）",
    file: "src/platform/storage/upload.rs",
    source: 'fn compare(etag: &str) -> bool {\n    normalize_etag(etag) == etag\n}\n',
    expected: [],
  },
  {
    name: "测试里的第二次组装不算",
    file: "src/face/other.rs",
    source:
      '#[cfg(test)]\nmod tests {\n    fn t() {\n        let tag = etag(m, 1);\n        let _ = web_headers(&tag);\n    }\n}\n',
    expected: [],
  },
  {
    name: "pre-body 认领顺序在 claim 外算违规",
    file: "src/daemon/assembly.rs",
    source:
      "fn wire(routes: &Routes) {\n    let _ = routes.plugin_ui(&head);\n    let _ = routes.package_upload(&head);\n}\n",
    expected: ["pre-body 认领顺序"],
  },
  {
    name: "pre-body 认领顺序留在 claim 里不算",
    file: "src/face/prebody.rs",
    source:
      "fn claim(&self) {\n    let _ = self.plugin_ui(&head);\n    let _ = self.package_upload(&head);\n}\n",
    expected: [],
  },
  {
    name: "插件 UI 静态准入在允许点外算违规",
    file: "src/face/fallback.rs",
    source:
      'fn plugin_ui(id: &str) {\n    if !valid_name(id) { return; }\n    let _ = web_assets(paths, id);\n}\n',
    expected: ["插件 UI 静态准入"],
  },
  {
    name: "插件 UI 静态准入留在 web_assets_for 里不算",
    file: "src/face/plugin_ui.rs",
    source:
      'fn web_assets_for(id: &str) {\n    if !valid_name(id) { return None; }\n    web_assets(paths, id)\n}\n',
    expected: [],
  },
  {
    name: "插件 UI token 准入在允许点外算违规",
    file: "src/face/other.rs",
    source:
      'fn admit(id: &str) {\n    let _ = web_assets_for(paths, id);\n    let _ = valid_token(lan, ui);\n}\n',
    expected: ["插件 UI token 准入"],
  },
  {
    name: "插件 UI token 准入留在 static_assets 里不算",
    file: "src/face/plugin_ui.rs",
    source:
      'fn static_assets(id: &str) {\n    let _ = web_assets_for(paths, id);\n    let _ = valid_token(lan, ui);\n}\n',
    expected: [],
  },
  {
    name: "插件 UI 资产解析在允许点外算违规",
    file: "src/face/other.rs",
    source:
      'fn asset(sub: &str) {\n    let r = relative_path(sub, entry);\n    let _ = web_file(root, &r);\n    if wants_html(headers) {}\n}\n',
    expected: ["插件 UI 资产解析"],
  },
  {
    name: "插件 UI 资产解析留在 resolve_asset 里不算",
    file: "src/face/webui.rs",
    source:
      'fn resolve_asset(sub: &str) {\n    let r = relative_path(sub, entry);\n    let _ = web_file(root, &r);\n    if wants_html(headers) {}\n}\n',
    expected: [],
  },
  {
    name: "install 收尾在收尾函数外再拼一次算违规",
    file: "src/daemon/plugins.rs",
    source:
      'fn install_again(host: &mut Supervisor, service: Service, clock: &Clock) {\n    let mut poll = host.hook_pump();\n    host.install(service, false);\n    host.bump_catalog();\n    let _ = host.status_for_id("demo", clock.now());\n}\n',
    expected: ["插件安装收尾"],
  },
  {
    name: "收尾函数里的 install 组装不算",
    file: "src/daemon/plugins.rs",
    source:
      'fn finish_install(host: &mut Supervisor, service: Service, clock: &Clock) {\n    host.install(service, false);\n    host.bump_catalog();\n    let _ = host.status_for_id("demo", clock.now());\n}\n',
    expected: [],
  },
  {
    name: "complete 端点的 parts 解析在名单外算违规",
    file: "src/face/other.rs",
    source:
      'fn decode(body: &[u8]) -> Option<usize> {\n    let value: Value = serde_json::from_slice(body).unwrap();\n    value.get("parts").and_then(|parts| parts.as_array()).map(|parts| parts.len())\n}\n',
    expected: ["complete 端点的 parts 请求体解析"],
  },
  {
    name: "parse_parts 的 parts 解析不算",
    file: "src/face/blob_routes.rs",
    source:
      'fn parse_parts(body: &[u8], required: bool) -> Option<usize> {\n    let value: Value = serde_json::from_slice(body).ok()?;\n    let parts = value.get("parts")?.as_array()?;\n    let _ = required;\n    Some(parts.len())\n}\n',
    expected: [],
  },
  {
    name: "导入成功响应在第二个函数里组装算违规",
    file: "src/face/import_copy.rs",
    source:
      'fn render(status: &Value, warnings: Vec<String>) -> Value {\n    let id = status.get("summary").and_then(|summary| summary.get("id"));\n    json!({"configApplied": true, "id": id, "warnings": warnings})\n}\n',
    expected: ["导入成功响应的组装"],
  },
  {
    name: "允许的导入成功组装点不算",
    file: "src/face/install.rs",
    source:
      'fn import_success(summary: Value, warnings: Vec<String>) -> Vec<u8> {\n    let id = summary.get("id");\n    json!({"configApplied": true, "id": id, "warnings": warnings})\n}\n',
    expected: [],
  },
];

const SELFTEST_COMPOSE = [
  {
    name: "装配点里的监督状态机",
    file: "src/daemon/driver.rs",
    source: "fn project() {\n    let _ = State::Running;\n}\n",
    expected: ["监督状态机"],
  },
  {
    name: "装配点里的 slot 组装",
    file: "src/daemon/plugins.rs",
    source: "fn build() {\n    let _ = Slot::new(id, exec);\n}\n",
    expected: ["slot 组装"],
  },
  {
    name: "装配点里的进程派生",
    file: "src/daemon/bootstrap.rs",
    source: "fn spawn() {\n    Command::spawn(cmd);\n}\n",
    expected: ["进程派生"],
  },
  {
    name: "装配点里的正常编排不算",
    file: "src/daemon/driver.rs",
    source: "fn start(host: &mut Supervisor) {\n    host.start_service(\"x\", clock);\n}\n",
    expected: [],
  },
  {
    name: "测试文件里的状态机不算",
    file: "src/daemon/tests.rs",
    source: "#![cfg(test)]\n\nfn t() {\n    let _ = State::Running;\n}\n",
    expected: [],
  },
  {
    name: "L3 里的状态机不算",
    file: "src/supervise/ops/host.rs",
    source: "fn t() {\n    let _ = State::Running;\n}\n",
    expected: [],
  },
];

function selftest() {
  let total = 0;
  let failures = 0;
  const check = (name, found, expected) => {
    total += 1;
    const ok = JSON.stringify(found) === JSON.stringify(expected);
    console.log(`selftest: ${ok ? "ok" : "FAILED"}  ${name}`);
    if (!ok) {
      failures += 1;
      console.log(`  expected ${JSON.stringify(expected)}, got ${JSON.stringify(found)}`);
    }
  };

  for (const item of SELFTEST_CASES) {
    const found = [...new Set(categoryViolationsIn("<selftest>", item.source).map((entry) => entry.rule))].sort();
    check(item.name, found, [...new Set(item.expected)].sort());
  }

  for (const item of SELFTEST_IDENTITY) {
    const fileModules = new Map(item.files);
    check(item.name, resolveFileModule(item.file, fileModules), item.expected);
  }

  for (const item of SELFTEST_FILE_MAP) {
    const modules = new Set(item.modules ?? ["api", "kernel"]);
    check(item.name, partitionFileModules(new Map(item.files), modules, item.exists), item.expected);
  }

  for (const item of SELFTEST_SEAM) {
    const found = [...new Set(seamViolationsIn(item.file, item.source).map((entry) => entry.rule))].sort();
    check(item.name, found, [...new Set(item.expected)].sort());
  }

  for (const item of SELFTEST_ASSEMBLY) {
    const found = [...new Set(assemblyViolationsIn(item.file, item.source).map((entry) => entry.rule))].sort();
    check(item.name, found, [...new Set(item.expected)].sort());
  }

  for (const item of SELFTEST_COMPOSE) {
    const found = [...new Set(composeViolationsIn(item.file, item.source).map((entry) => entry.rule))].sort();
    check(item.name, found, [...new Set(item.expected)].sort());
  }

  console.log(`selftest: ${total - failures}/${total} 通过`);
  return failures;
}

function report({ map, modules, sources, edges, unknown, stale, missingFiles, badFileModules, unmapped, layers, cycles, suffixes, seam, category, assembly, compose }) {
  const lines = [];
  lines.push("dep-lint · 观测基线（只读，不改代码）");
  lines.push(`模块 ${sources.length} 个源文件，引用边 ${edges.length} 条，映射 ${modules.size} 项`);
  const byLayer = new Map();
  for (const entry of modules.values()) byLayer.set(entry.layer, (byLayer.get(entry.layer) ?? 0) + 1);
  lines.push(
    "层分布：" +
      [...byLayer.entries()]
        .sort((a, b) => a[0].localeCompare(b[0]))
        .map(([layer, count]) => `${layer} ${count}`)
        .join(" · "),
  );
  if (unknown.length > 0) lines.push(`⚠ 未映射的文件：${unknown.join(", ")}`);
  if (stale.length > 0) lines.push(`⚠ 映射里不存在的模块：${stale.join(", ")}`);

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

  lines.push("");
  const mapCount =
    stale.length + unknown.length + missingFiles.length + badFileModules.length + unmapped.length;
  lines.push(`[V4] 模块映射：映射缺失或过期（${mapCount}）`);
  for (const name of stale) lines.push(`  ${name}  映射里有，src/ 里没有`);
  for (const file of unknown) lines.push(`  ${file}  src/ 里有，映射里没有`);
  for (const file of missingFiles) lines.push(`  ${file}  files 里登记了，磁盘上没有`);
  for (const item of badFileModules) lines.push(`  ${item.file}  files 指向不存在的模块 ${item.name}`);
  for (const file of unmapped) lines.push(`  ${file}  没有 files 映射（新文件必须在 arch-map.files 里登记）`);
  if (mapCount === 0) lines.push("  （无）");

  lines.push("");
  lines.push(`[V5] seam 不变量（${seam.length}）`);
  for (const item of seam) lines.push(`  ${item.file}:${item.line}  ${item.rule}  —— ${item.hint}`);
  if (seam.length === 0) lines.push("  （无）");

  lines.push("");
  lines.push(`[V6] 失败类别：detail / message 不参与分支（${category.length}）`);
  for (const item of category) lines.push(`  ${item.file}:${item.line}  ${item.rule}  —— ${item.hint}`);
  if (category.length === 0) lines.push("  （无）");

  lines.push("");
  lines.push(`[V7] 单点组装：同一份组合只拼一次（${assembly.length}）`);
  for (const item of assembly) {
    lines.push(`  ${item.file}:${item.line}  ${item.item}  ${item.rule}  —— ${item.hint}`);
  }
  if (assembly.length === 0) lines.push("  （无）");

  lines.push("");
  lines.push(`[V8] 装配点不含业务（${compose.length}）`);
  for (const item of compose) {
    lines.push(`  ${item.file}:${item.line}  ${item.rule}  —— ${item.hint}`);
  }
  if (compose.length === 0) lines.push("  （无）");

  const total =
    layers.length +
    cycles.length +
    suffixes.errors.length +
    stale.length +
    unknown.length +
    missingFiles.length +
    badFileModules.length +
    unmapped.length +
    seam.length +
    category.length +
    assembly.length +
    compose.length;
  lines.push("");
  lines.push(
    `合计 ${total} 条违规（层方向 ${layers.length} · 循环分量 ${cycles.length} · 后缀 ${suffixes.errors.length} · 映射 ${mapCount} · seam ${seam.length} · 失败类别 ${category.length} · 单点组装 ${assembly.length} · 装配点业务 ${compose.length}）`,
  );
  return { text: lines.join("\n"), total };
}

const args = parseArgs(process.argv.slice(2));
if (args.selftest) {
  process.exit(selftest() === 0 ? 0 : 1);
}
const { map, rank, modules, fileModules } = loadMap(args.map);
const { sources, edges, unknown } = scanSources(args.src, modules, fileModules);
const unmapped = sources.filter((source) => !source.explicit).map((source) => source.file);
const { missing: missingFiles, unknown: badFileModules } = partitionFileModules(
  fileModules,
  modules,
  (file) => existsSync(join(REPO, file)),
);
const mapped = new Set(sources.map((source) => source.module));
const stale = [...modules.keys()].filter((name) => !mapped.has(name)).sort();
const nodes = [...new Set(sources.map((source) => source.module))];
const layers = layerViolations(edges, modules, rank);
const cycles = cycleViolations(nodes, edges);
const suffixes = suffixViolations(sources, map);
const seam = seamViolations(sources);
const category = categoryViolations(sources);
const assembly = assemblyViolations(sources);
const compose = composeViolations(sources);
const { text, total } = report({ map, modules, sources, edges, unknown, stale, missingFiles, badFileModules, unmapped, layers, cycles, suffixes, seam, category, assembly, compose });

if (args.json) {
  console.log(JSON.stringify({ total, layers, cycles, suffixes, seam, category, assembly, compose, unknown, stale, missingFiles, badFileModules, unmapped, files: sources.length, edges: edges.length }, null, 2));
} else {
  console.log(text);
}
process.exit(total > 0 && args.fail ? 1 : 0);
