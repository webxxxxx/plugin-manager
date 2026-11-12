#!/usr/bin/env node
// Prototype .spk checker: verifies the strict ustar layout, signature, and manifest pins.
// Usage: node tools/check_spk.mjs <file.spk> [--extract dir] [--key pub.pem]
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { createHash, createPublicKey, verify } from 'node:crypto';
import { join, dirname, resolve, sep } from 'node:path';

const args = process.argv.slice(2);
const spk = args[0];
let extractDir = null;
let keyPath = null;
for (let i = 1; i < args.length; i++) {
  if (args[i] === '--extract') extractDir = args[++i];
  else if (args[i] === '--key') keyPath = args[++i];
}
if (!spk) {
  console.error('usage: node tools/check_spk.mjs <file.spk> [--extract dir] [--key pub.pem]');
  process.exit(2);
}
if (!keyPath) keyPath = resolve('samples/keys/sample-2026.pub');

function cstr(b, start, len) {
  const s = b.subarray(start, start + len);
  const z = s.indexOf(0);
  return s.subarray(0, z === -1 ? len : z).toString('utf8');
}

const buf = readFileSync(spk);
let off = 0;
const entries = [];
while (true) {
  if (off + 512 > buf.length) throw new Error('truncated tar at byte ' + off);
  const h = buf.subarray(off, off + 512);
  let zero = true;
  for (let i = 0; i < 512; i++) if (h[i] !== 0) { zero = false; break; }
  if (zero) break;
  const nm = cstr(h, 0, 100);
  const mode = parseInt(cstr(h, 100, 8).trim() || '0', 8);
  const size = parseInt(cstr(h, 124, 12).trim() || '0', 8);
  const type = String.fromCharCode(h[156]);
  const stored = parseInt(cstr(h, 148, 8).trim() || '0', 8);
  const copy = Buffer.from(h);
  for (let i = 148; i < 156; i++) copy[i] = 32;
  let sum = 0;
  for (let i = 0; i < 512; i++) sum += copy[i];
  if (sum !== stored) throw new Error('bad checksum for ' + nm);
  if (type !== '0') throw new Error('unsupported tar type ' + type + ' for ' + nm);
  if (nm.startsWith('/') || nm.indexOf(String.fromCharCode(92)) !== -1) throw new Error('unsafe path: ' + nm);
  const parts = nm.split('/');
  if (parts.some(p => p === '..' || p === '.' || p === '')) throw new Error('unsafe path: ' + nm);
  const data = buf.subarray(off + 512, off + 512 + size);
  if (data.length !== size) throw new Error('truncated data for ' + nm);
  entries.push({ name: nm, mode: mode, size: size, data: data });
  off += 512 + Math.ceil(size / 512) * 512;
}

if (entries.length < 2) throw new Error('missing manifest/signature');
if (entries[0].name !== 'manifest.json') throw new Error('manifest.json must be the first entry');
if (entries[1].name !== 'signature.json') throw new Error('signature.json must be the second entry');

const manifestBytes = entries[0].data;
const sig = JSON.parse(entries[1].data.toString('utf8'));
const pub = createPublicKey(readFileSync(keyPath));
const sigOk = verify(null, manifestBytes, pub, Buffer.from(sig.sig, 'base64'));
if (!sigOk) throw new Error('signature verification FAILED');

const manifest = JSON.parse(manifestBytes.toString('utf8'));
const errors = [];
function must(cond, msg) { if (!cond) errors.push(msg); }

must(manifest.schema === 1, 'schema must be 1');
must(typeof manifest.name === 'string' && /^[a-z0-9][a-z0-9_-]{0,63}$/.test(manifest.name), 'bad name');
must(typeof manifest.version === 'string' && /^[0-9]+\.[0-9]+\.[0-9]+/.test(manifest.version), 'bad version');
must(typeof manifest.platform === 'string' && /^[a-z0-9_]+(-[a-z0-9_]+){2,3}$/.test(manifest.platform), 'bad platform (expected <arch>-<vendor>-<os>-<abi>)');
must(manifest.protocol === 1, 'protocol must be 1');
must(Array.isArray(manifest.files) && manifest.files.length > 0, 'files[] required');

const byName = new Map();
for (const e of entries.slice(2)) byName.set(e.name, e);
const seen = new Set();
for (const f of manifest.files) {
  must(!seen.has(f.path), 'duplicate file: ' + f.path);
  seen.add(f.path);
  const e = byName.get(f.path);
  must(!!e, 'missing file: ' + f.path);
  if (!e) continue;
  must(createHash('sha256').update(e.data).digest('hex') === f.sha256, 'hash mismatch: ' + f.path);
  must(e.size === f.size, 'size mismatch: ' + f.path);
  must(e.mode === f.mode, 'mode mismatch: ' + f.path);
}
for (const n of byName.keys()) must(seen.has(n), 'extra file not in manifest: ' + n);

const capNames = new Set();
for (const c of manifest.capabilities || []) {
  must(typeof c.name === 'string' && /^[a-z0-9][a-z0-9_-]{0,63}$/.test(c.name), 'bad capability name');
  must(!String(c.name).startsWith('svc.'), 'capability may not use the svc. prefix: ' + c.name);
  must(!capNames.has(c.name), 'duplicate capability: ' + c.name);
  capNames.add(c.name);
  must(c.inputSchema && typeof c.inputSchema === 'object', 'missing inputSchema: ' + c.name);
}
must(manifest.exec && seen.has(manifest.exec.path), 'exec.path not in files[]');
if (manifest.ui && manifest.ui.type === 'web') {
  const entry = manifest.ui.entry || 'index.html';
  must(seen.has('payload/web/' + entry), 'ui entry not in files[]: payload/web/' + entry);
}
for (const k of Object.keys(manifest.hooks || {})) {
  must(manifest.hooks[k].startsWith('hooks/'), 'hook not under hooks/: ' + manifest.hooks[k]);
  must(seen.has(manifest.hooks[k]), 'hook not in files[]: ' + manifest.hooks[k]);
}

function stable(v) {
  if (Array.isArray(v)) return '[' + v.map(stable).join(',') + ']';
  if (v && typeof v === 'object') {
    return '{' + Object.keys(v).sort().map(function (k) {
      return JSON.stringify(k) + ':' + stable(v[k]);
    }).join(',') + '}';
  }
  return JSON.stringify(v);
}
const canonical = stable(manifest) === manifestBytes.toString('utf8');

if (extractDir) {
  for (const e of entries.slice(2)) {
    const dst = join(extractDir, e.name.split('/').join(sep));
    mkdirSync(dirname(dst), { recursive: true });
    writeFileSync(dst, e.data);
  }
  writeFileSync(join(extractDir, 'manifest.json'), manifestBytes);
  writeFileSync(join(extractDir, 'signature.json'), entries[1].data);
}

console.log('spk: ' + resolve(spk));
console.log('entries: ' + entries.length + ' (manifest + signature + ' + (entries.length - 2) + ' files)');
console.log('name: ' + manifest.name);
console.log('version: ' + manifest.version + '  platform: ' + manifest.platform + '  arch: ' + String(manifest.platform).split('-')[0] + '  protocol: ' + manifest.protocol);
console.log('capabilities: ' + (manifest.capabilities || []).map(c => c.name).join(', '));
console.log('signature: OK (ed25519, keyId=' + sig.keyId + ')');
console.log('canonical manifest: ' + (canonical ? 'yes' : 'no (valid, but not reproducible)'));
if (extractDir) console.log('extracted: ' + resolve(extractDir));
if (errors.length) {
  console.error('FAIL:');
  for (const e of errors) console.error('  - ' + e);
  process.exit(1);
}
console.log('RESULT: OK');
