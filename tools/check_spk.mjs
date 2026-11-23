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
// Injection test for the manifest rules: every rule breaks one manifest that
// should be rejected, and protocol 3 with capabilities must pass (#265).
function selfTest() {
  const data = new Map();
  const entry = (name, bytes, mode) => {
    const bytes_ = Buffer.from(bytes);
    const item = { name, size: bytes_.length, mode, data: bytes_ };
    data.set(name, item);
    return { path: name, size: bytes_.length, mode, sha256: createHash('sha256').update(bytes_).digest('hex') };
  };
  const base = (over) => {
    const files = [
      entry('payload/bin/demo', '#!/bin/sh\nexit 0\n', 0o755),
      entry('payload/web/index.html', '<!doctype html>', 0o644),
    ];
    return Object.assign(
      {
        schema: 1,
        id: 'demo',
        name: '演示插件',
        version: '1.0.0',
        platform: 'x86_64-unknown-linux-gnu',
        protocol: 1,
        exec: { path: 'payload/bin/demo' },
        files,
      },
      over,
    );
  };
  const cases = [
    ['schema', base({ schema: 2 }), 'schema must be 1'],
    ['id', base({ id: 'Bad Id' }), 'bad id'],
    ['reserved id', base({ id: 'svc' }), 'reserved'],
    ['display name', base({ name: 'a\u0007b' }), 'bad display name'],
    ['version', base({ version: '1.0' }), 'bad version'],
    ['platform', base({ platform: 'linux' }), 'bad platform'],
    ['protocol', base({ protocol: 4 }), 'protocol must be'],
    ['exec', base({ exec: { path: 'payload/bin/missing' } }), 'exec.path not in files[]'],
    ['capability', base({ capabilities: [{ name: 'start', inputSchema: {} }] }), 'reserved for the lifecycle tool'],
    ['outputSchema', base({ capabilities: [{ name: 'echo', inputSchema: {}, outputSchema: 1 }] }), 'bad outputSchema'],
    ['icon', base({ icons: { gif: 'payload/web/index.html' } }), 'unknown format'],
    ['ui entry', base({ ui: { type: 'web', entry: 'nope.html' } }), 'ui entry not in files'],
  ];
  const failures = [];
  const good = manifestErrors(base({ capabilities: [{ name: 'echo', inputSchema: {}, outputSchema: {} }] }), data);
  if (good.length) failures.push('the good manifest was rejected: ' + good.join('; '));
  for (const [label, manifest, needle] of cases) {
    const found = manifestErrors(manifest, data).join(' | ');
    if (!found.includes(needle)) failures.push(label + ': expected "' + needle + '", got "' + found + '"');
  }
  const p3 = manifestErrors(
    base({ protocol: 3, exec: { path: 'payload/bin/demo', ready: 'notify' }, capabilities: [{ name: 'echo', inputSchema: {} }] }),
    data,
  );
  if (p3.length) failures.push('protocol 3 with capabilities was rejected: ' + p3.join('; '));
  if (failures.length) {
    console.error('self-test FAILED:');
    for (const failure of failures) console.error('  - ' + failure);
    process.exit(1);
  }
  console.log('self-test: ok (' + (cases.length + 2) + ' cases)');
}

if (!spk) {
  console.error('usage: node tools/check_spk.mjs <file.spk> [--extract dir] [--key pub.pem]');
  process.exit(2);
}

if (spk === '--self-test') {
  selfTest();
  process.exit(0);
}
if (!keyPath) keyPath = resolve('samples/keys/sample-2026.pub');

function cstr(b, start, len) {
  const s = b.subarray(start, start + len);
  const z = s.indexOf(0);
  return s.subarray(0, z === -1 ? len : z).toString('utf8');
}

/**
 * Every manifest rule, as a pure function of the manifest and the entries by
 * name. `--self-test` runs this against a good manifest and one mutant per
 * rule, so a broken rule fails here instead of silently accepting a package.
 */
function manifestErrors(manifest, byName) {
  const errors = [];
  function must(cond, msg) { if (!cond) errors.push(msg); }
  function must(cond, msg) { if (!cond) errors.push(msg); }

  must(manifest.schema === 1, 'schema must be 1');
  // `id` is the identity: directory name, log file, /plugin-ui/ segment and the
  // `<id>.<cap>` tool prefix. `name` is only a display name and may be Chinese.
  must(typeof manifest.id === 'string' && /^[a-z0-9][a-z0-9_-]{0,63}$/.test(manifest.id), 'bad id ([a-z0-9][a-z0-9_-]{0,63})');
  must(manifest.id !== 'svc', 'the built-in svc id is reserved');
  must(
    typeof manifest.name === 'string' && manifest.name.length > 0 && Array.from(manifest.name).length <= 128 && !/[\u0000-\u001f\u007f]/.test(manifest.name),
    'bad display name (non-empty, <=128 characters, no control characters)'
  );
  must(typeof manifest.version === 'string' && /^[0-9]+\.[0-9]+\.[0-9]+$/.test(manifest.version), 'bad version');
  must(typeof manifest.platform === 'string' && /^[a-z0-9_]+(-[a-z0-9_]+){2,3}$/.test(manifest.platform), 'bad platform (expected <arch>-<vendor>-<os>-<abi>)');
  must([1, 2, 3].includes(manifest.protocol), 'protocol must be 1, 2 or 3');
  must(Array.isArray(manifest.files) && manifest.files.length > 0, 'files[] required');

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

  if (manifest.interpreter !== undefined) {
    must(
      manifest.interpreter && typeof manifest.interpreter.name === 'string' && /^[A-Za-z0-9._-]+$/.test(manifest.interpreter.name),
      'bad interpreter name'
    );
    must(
      manifest.interpreter.version === undefined || typeof manifest.interpreter.version === 'string',
      'bad interpreter version'
    );
  }
  const lifecycle = manifest.lifecycle || {};
  for (const action of ['start', 'stop', 'status']) {
    const hook = lifecycle[action];
    if (!hook) continue;
    must(typeof hook.path === 'string' && seen.has(hook.path), 'lifecycle.' + action + ' not in files[]');
  }
  const kernelModule = [...seen].some(function (p) { return /\.ko$/i.test(p); });
  if (manifest.compat !== undefined) {
    const devices = manifest.compat && manifest.compat.devices;
    must(Array.isArray(devices) && devices.length > 0, 'compat.devices must be a non-empty array');
    if (Array.isArray(devices)) {
      must(devices.length <= 64, 'compat.devices lists more than 64 ids');
      const unique = new Set(devices);
      must(unique.size === devices.length, 'duplicate compat device id');
      for (const d of devices) {
        must(typeof d === 'string' && /^[a-z0-9][a-z0-9._-]{0,63}$/.test(d), 'bad compat device id: ' + d);
      }
    }
  }
  if (kernelModule) {
    must(manifest.compat && Array.isArray(manifest.compat.devices) && manifest.compat.devices.length > 0,
      'kernel modules require compat.devices');
  }
  // Package icons: one payload file per format, magic-checked, <=1 MiB each and
  // <=2 MiB in total.
  const iconFormats = { png: Buffer.from([0x89, 0x50, 0x4e, 0x47]), ico: Buffer.from([0, 0, 1, 0]), icns: Buffer.from('icns') };
  if (manifest.icons !== undefined) {
    const keys = Object.keys(manifest.icons);
    must(keys.length > 0, 'icons must declare at least one of png, ico or icns');
    let total = 0;
    for (const key of keys) {
      const magic = iconFormats[key];
      must(!!magic, 'icons has an unknown format: ' + key);
      const path = manifest.icons[key];
      must(typeof path === 'string' && seen.has(path), 'icons.' + key + ' is not in files[]: ' + path);
      const entry = byName.get(path);
      if (!entry || !magic) continue;
      must(entry.size <= 1024 * 1024, 'icons.' + key + ' is larger than 1 MiB');
      total += entry.size;
      must(entry.data.subarray(0, magic.length).equals(magic), 'icons.' + key + ' is not a ' + key + ' file');
    }
    must(total <= 2 * 1024 * 1024, 'icons total larger than 2 MiB');
  }
  const capNames = new Set();
  for (const c of manifest.capabilities || []) {
    must(typeof c.name === 'string' && c.name.length > 0, 'bad capability name');
    must(!String(c.name).startsWith('svc.'), 'capability may not use the svc. prefix: ' + c.name);
    must(c.name !== 'start', 'capability name "start" is reserved for the lifecycle tool');
    must(!capNames.has(c.name), 'duplicate capability: ' + c.name);
    capNames.add(c.name);
    must(c.inputSchema && typeof c.inputSchema === 'object', 'missing inputSchema: ' + c.name);
    must(c.outputSchema === undefined || (c.outputSchema && typeof c.outputSchema === 'object'), 'bad outputSchema: ' + c.name);
  }
  must(manifest.exec && seen.has(manifest.exec.path), 'exec.path not in files[]');
  if (manifest.ui && manifest.ui.type === 'web') {
    const entry = manifest.ui.entry || 'index.html';
    must(seen.has('payload/web/' + entry), 'ui entry not in files[]: payload/web/' + entry);
  }
  for (const k of Object.keys(manifest.hooks || {})) {
    must(manifest.hooks[k].startsWith('hooks/'), 'hook not under hooks/: ' + manifest.hooks[k]);
    must(seen.has(manifest.hooks[k]), 'hook not in files[]: ' + manifest.hooks[k]);
    const entry = byName.get(manifest.hooks[k]);
    if (entry && entry.data.subarray(0, 2).toString() === '#!' && entry.data.includes(0x0d)) {
      errors.push('hook must use LF line endings: ' + manifest.hooks[k]);
    }
  }
  return errors;
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
const byName = new Map();
for (const e of entries.slice(2)) byName.set(e.name, e);
const errors = manifestErrors(manifest, byName);

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
console.log('id: ' + manifest.id + '  name: ' + manifest.name);
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
