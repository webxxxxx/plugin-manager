#!/usr/bin/env node
// Minify the embedded console sources into `src/assets/static/`. This is a
// manual step: edit the readable copies under `src/assets/console/`, run this
// script, and commit both sides. `build.rs` embeds the generated files.
//
//   node tools/minify-static.mjs           # rewrite the static files
//   node tools/minify-static.mjs --check   # fail if they are stale
//
// The minifier is deliberately dependency-free. CSS is compacted; JavaScript
// loses comments, indentation and every optional space (strings are copied
// verbatim), without renaming identifiers.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");

const FILES = [
  ["src/assets/console/index.html", "src/assets/static/index.html"],
  ["src/assets/console/favicon.svg", "src/assets/static/favicon.svg"],
];

function minifyCss(css) {
  return css
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\s+/g, " ")
    .replace(/\s*([{}:;,])\s*/g, "$1")
    .replace(/;}/g, "}")
    .trim();
}

const WORD = /[A-Za-z0-9_$]/;

// A space is load-bearing only between two word characters (two identifiers,
// a keyword and a name) or between two characters that would otherwise form a
// different token (`++`, `--`, a comment start).
function needsSpace(previous, next) {
  if (!previous || !next) return false;
  if (WORD.test(previous) && WORD.test(next)) return true;
  if ((previous === "+" && next === "+") || (previous === "-" && next === "-")) return true;
  return previous === "/" && next === "/";
}

// JavaScript is whitespace-insensitive except inside strings. This scanner
// keeps single quotes, double quotes and template literals verbatim; the page
// has no block comments or inline line comments, so comment removal is
// line-oriented and safe.
function minifyJs(js) {
  const kept = [];
  for (const line of js.replace(/\r\n?/g, "\n").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("//")) continue;
    kept.push(trimmed);
  }

  const code = kept.join("\n");
  let out = "";
  let quote = null;
  let escaped = false;
  let regex = false;
  let inClass = false;
  for (let index = 0; index < code.length; index += 1) {
    const char = code[index];
    if (regex) {
      out += char;
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === "[") inClass = true;
      else if (char === "]") inClass = false;
      else if (char === "/" && !inClass) regex = false;
      continue;
    }
    if (quote) {
      out += char;
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === quote) quote = null;
      continue;
    }
    if (char === "'" || char === '"' || char === "`") {
      quote = char;
      out += char;
      continue;
    }
    if (char === "/") {
      // A slash is a regex opener after an operator, a delimiter or `(`; after
      // a value it is division. This page only has the former.
      const previous = out.replace(/\s+$/, "").slice(-1);
      if (!previous || "([{:;,=!&|?+-*%^~<>".includes(previous)) {
        regex = true;
        inClass = false;
        out += char;
        continue;
      }
    }
    if (/\s/.test(char)) {
      let next = index + 1;
      while (next < code.length && /\s/.test(code[next])) next += 1;
      if (needsSpace(out[out.length - 1] || "", code[next] || "")) out += " ";
      index = next - 1;
      continue;
    }
    out += char;
  }
  return out;
}

function minifyHtml(source) {
  let html = source.replace(/\r\n?/g, "\n");
  const style = /<style>([\s\S]*?)<\/style>/.exec(html);
  const script = /<script>([\s\S]*?)<\/script>/.exec(html);
  if (!style) throw new Error("index.html has no <style> block");
  if (!script) throw new Error("index.html has no <script> block");
  html = html.replace(style[0], () => "<style>" + minifyCss(style[1]) + "</style>");
  html = html.replace(script[0], () => "<script>" + minifyJs(script[1]) + "</script>");
  html = html
    .replace(/<!--[\s\S]*?-->/g, "")
    .replace(/>\s+</g, "><")
    .replace(/[ \t]+\n/g, "\n")
    .replace(/\n{2,}/g, "\n")
    .trim();
  return html + "\n";
}

function minifySvg(source) {
  return (
    source
      .replace(/<\?xml[^>]*\?>/g, "")
      .replace(/<!--[\s\S]*?-->/g, "")
      .replace(/>\s+</g, "><")
      .replace(/\s{2,}/g, " ")
      .trim() + "\n"
  );
}

function build(relative) {
  const source = readFileSync(join(ROOT, relative), "utf8");
  return relative.endsWith(".svg") ? minifySvg(source) : minifyHtml(source);
}

const check = process.argv.includes("--check");
let stale = 0;
for (const [relative, output] of FILES) {
  const minified = build(relative);
  const before = readFileSync(join(ROOT, relative), "utf8").length;
  const target = join(ROOT, output);
  if (check) {
    const current = existsSync(target)
      ? readFileSync(target, "utf8").replace(/\r\n/g, "\n")
      : "";
    if (current !== minified) {
      console.error(`minify-static: ${output} is stale; run node tools/minify-static.mjs`);
      stale += 1;
    } else {
      console.log(`minify-static: ${output} is current`);
    }
  } else {
    writeFileSync(target, minified);
    const after = minified.length;
    const percent = ((1 - after / before) * 100).toFixed(1);
    console.log(`minify-static: ${relative} -> ${output} ${before} -> ${after} chars (-${percent}%)`);
  }
}
if (stale > 0) process.exit(1);
