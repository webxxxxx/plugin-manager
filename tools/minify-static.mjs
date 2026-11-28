#!/usr/bin/env node
// Minify the two embedded console sources. This is a manual step: edit the
// readable `index.html` / `favicon.svg`, run this script, and commit both the
// source and the generated `.min` file. `build.rs` embeds the `.min` copy.
//
//   node tools/minify-static.mjs           # rewrite the .min files
//   node tools/minify-static.mjs --check   # fail if they are stale
//
// The minifier is deliberately dependency-free and conservative: it strips
// comments, indentation and blank lines (and compacts CSS), but it does not
// rename identifiers or rewrite JavaScript tokens.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");

const FILES = [
  ["src/assets/static/index.html", "src/assets/static/index.min.html"],
  ["src/assets/static/favicon.svg", "src/assets/static/favicon.min.svg"],
];

function minifyCss(css) {
  return css
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\s+/g, " ")
    .replace(/\s*([{}:;,])\s*/g, "$1")
    .replace(/;}/g, "}")
    .trim();
}

// JavaScript is line-oriented here: every comment occupies its own line, so
// removing those lines and the indentation is safe. Token-level rewriting
// would need a parser and buy little for a page this size.
function minifyJs(js) {
  const kept = [];
  for (const line of js.replace(/\r\n?/g, "\n").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("//")) continue;
    kept.push(trimmed);
  }
  return kept.join("\n");
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
    console.log(`minify-static: ${relative} ${before} -> ${after} bytes (-${percent}%)`);
  }
}
if (stale > 0) process.exit(1);
