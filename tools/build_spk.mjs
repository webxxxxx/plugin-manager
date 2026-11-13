#!/usr/bin/env node
// Prototype .spk packer: strict ustar + canonical manifest + Ed25519 signature.
// Usage: node tools/build_spk.mjs [name]
import { readFileSync, writeFileSync, readdirSync, statSync, mkdirSync, rmSync, cpSync, existsSync } from 'node:fs';
import { createHash, createPrivateKey, sign } from 'node:crypto';
import { join, relative, sep, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const name = process.argv[2] || 'wireguard';
const SRC = join(ROOT, 'samples', name);
const OUT = join(ROOT, 'samples', 'spk');
const KEY = join(ROOT, 'samples', 'keys', 'sample-2026.key');
const KEY_ID = 'sample-2026';

const src = JSON.parse(readFileSync(join(SRC, 'manifest.json'), 'utf8'));
if (src.files) throw new Error('manifest.json must not contain files (the packer computes them)');

function stable(v) {
  if (Array.isArray(v)) return '[' + v.map(stable).join(',') + ']';
  if (v && typeof v === 'object') {
    return '{' + Object.keys(v).sort().map(function (k) {
      return JSON.stringify(k) + ':' + stable(v[k]);
    }).join(',') + '}';
  }
  return JSON.stringify(v);
}

function walk(dir, base, out) {
  for (const entry of readdirSync(dir).sort()) {
    const p = join(dir, entry);
    const st = statSync(p);
    if (st.isDirectory()) walk(p, base, out);
    else if (st.isFile()) out.push({ path: relative(base, p).split(sep).join('/'), abs: p });
    else throw new Error('unsupported entry: ' + p);
  }
  return out;
}

const execSet = new Set();
execSet.add(src.exec.path);
for (const k of Object.keys(src.hooks || {})) execSet.add(src.hooks[k]);
for (const action of Object.keys(src.lifecycle || {})) {
  if (src.lifecycle[action] && src.lifecycle[action].path) execSet.add(src.lifecycle[action].path);
}

const all = [];
for (const d of ['payload', 'config', 'hooks']) {
  const p = join(SRC, d);
  if (existsSync(p)) walk(p, SRC, all);
}
all.sort(function (a, b) { return a.path < b.path ? -1 : a.path > b.path ? 1 : 0; });

// A shebang hook with CRLF fails to spawn on Unix (the kernel looks for
// "/bin/sh\r"). Reject it at build time with a readable error.
const unixTarget = !String(src.platform || '').includes('windows');
if (unixTarget) {
  for (const hookPath of Object.values(src.hooks || {})) {
    const entry = all.find(function (e) { return e.path === hookPath; });
    if (entry && readFileSync(entry.abs).includes(0x0d)) {
      throw new Error('hook must use LF line endings: ' + hookPath);
    }
  }
}

const files = all.map(function (e) {
  const data = readFileSync(e.abs);
  return {
    path: e.path,
    size: data.length,
    sha256: createHash('sha256').update(data).digest('hex'),
    mode: execSet.has(e.path) ? 0o755 : 0o644
  };
});

const manifest = Object.assign({}, src, { files: files });
const manifestBytes = Buffer.from(stable(manifest), 'utf8');
const key = createPrivateKey(readFileSync(KEY));
const sig = sign(null, manifestBytes, key).toString('base64');
const signature = { alg: 'ed25519', keyId: KEY_ID, sig: sig };
const signatureBytes = Buffer.from(JSON.stringify(signature) + '\n', 'utf8');

const platform = String(src.platform || '');
if (!/^[a-z0-9_]+(-[a-z0-9_]+){2,3}$/.test(platform)) throw new Error('manifest.json needs a target-triple platform');
const arch = platform.split('-')[0];
const head = name + '-' + src.version + '-' + arch;
const stage = join(OUT, head);
rmSync(stage, { recursive: true, force: true });
mkdirSync(stage, { recursive: true });
for (const e of all) {
  const dst = join(stage, e.path.split('/').join(sep));
  mkdirSync(dirname(dst), { recursive: true });
  cpSync(e.abs, dst);
}
writeFileSync(join(stage, 'manifest.json'), manifestBytes);
writeFileSync(join(stage, 'signature.json'), signatureBytes);

function oct(n, width) {
  return n.toString(8).padStart(width - 1, '0') + '\u0000';
}

function header(nm, size, mode) {
  const b = Buffer.alloc(512);
  b.write(nm, 0, 100, 'utf8');
  b.write(oct(mode, 8), 100, 8, 'ascii');
  b.write(oct(0, 8), 108, 8, 'ascii');
  b.write(oct(0, 8), 116, 8, 'ascii');
  b.write(oct(size, 12), 124, 12, 'ascii');
  b.write(oct(0, 12), 136, 12, 'ascii');
  b.write('        ', 148, 8, 'ascii');
  b.write('0', 156, 1, 'ascii');
  b.write('ustar', 257, 5, 'ascii');
  b.write('\u0000', 262, 1, 'ascii');
  b.write('00', 263, 2, 'ascii');
  let sum = 0;
  for (let i = 0; i < 512; i++) sum += b[i];
  b.write(sum.toString(8).padStart(6, '0') + '\u0000 ', 148, 8, 'ascii');
  return b;
}

const chunks = [];
function add(nm, data, mode) {
  chunks.push(header(nm, data.length, mode));
  chunks.push(data);
  const pad = (512 - (data.length % 512)) % 512;
  if (pad) chunks.push(Buffer.alloc(pad));
}
add('manifest.json', manifestBytes, 0o644);
add('signature.json', signatureBytes, 0o644);
for (const e of all) add(e.path, readFileSync(e.abs), execSet.has(e.path) ? 0o755 : 0o644);
chunks.push(Buffer.alloc(1024));

const spk = join(OUT, head + '.spk');
writeFileSync(spk, Buffer.concat(chunks));

console.log('built ' + spk);
console.log('  files: ' + files.length + ', manifest: ' + manifestBytes.length + ' B, spk: ' + statSync(spk).size + ' B');
console.log('  staged: ' + stage);
