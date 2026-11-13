#!/usr/bin/env node
// Compatibility and performance harness for svcd's HTTP, MCP, WebSocket and
// SSE surfaces. Run from the repo root:
//
//   node tools/perf/run.mjs --compat
//   node tools/perf/run.mjs --perf
//   node tools/perf/run.mjs --all --out target/perf-results.json
//
// The harness is intentionally outside cargo test and CI: it spawns a release
// daemon, a Python echo plugin and real network clients.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync, copyFileSync, openSync, closeSync } from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';
import WebSocket from 'ws';
import EventSource from 'eventsource';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';

const ROOT = path.resolve(fileURLToPath(new URL('../..', import.meta.url)));
const WIN = process.platform === 'win32';
const EXE = WIN ? '.exe' : '';
const SVCD = path.join(ROOT, 'target', 'release', 'svcd' + EXE);
const KEY = path.join(ROOT, 'samples', 'keys', 'sample-2026.key');
const PUB = path.join(ROOT, 'samples', 'keys', 'sample-2026.pub');
const KEY_ID = 'sample-2026';
const KEEP_ALIVE_AGENT = new http.Agent({ keepAlive: true, maxSockets: 128 });

function log(message) {
  console.log('[perf] ' + message);
}

function runSync(file, args, env, cwd) {
  return execFileSync(file, args, {
    cwd: cwd || ROOT,
    env: env || process.env,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  });
}

function ensureAlias(name) {
  const target = path.join(path.dirname(SVCD), name + EXE);
  copyFileSync(SVCD, target);
  return target;
}

function ensureKey() {
  if (!existsSync(KEY) || !existsSync(PUB)) {
    runSync(process.execPath, [path.join(ROOT, 'tools', 'gen_sample_key.mjs')]);
  }
  assert.ok(existsSync(KEY), 'missing sample key: ' + KEY);
}

function httpRequest(address, method, urlPath, headers, body, timeoutMs, keepAlive) {
  return new Promise(function (resolve, reject) {
    const url = new URL('http://' + address);
    const mergedHeaders = Object.assign({}, headers || {});
    if (!keepAlive) {
      mergedHeaders.Connection = 'close';
    } else if (!mergedHeaders.Connection) {
      mergedHeaders.Connection = 'keep-alive';
    }
    if (body && mergedHeaders['Content-Length'] === undefined && mergedHeaders['content-length'] === undefined) {
      mergedHeaders['Content-Length'] = Buffer.byteLength(body);
    }
    const options = {
      hostname: url.hostname,
      port: url.port,
      method: method,
      path: urlPath,
      headers: mergedHeaders,
      agent: keepAlive ? KEEP_ALIVE_AGENT : false,
    };
    const request = http.request(options, function (response) {
      const chunks = [];
      response.on('data', function (chunk) { chunks.push(chunk); });
      response.on('end', function () {
        resolve({
          status: response.statusCode,
          headers: response.headers,
          body: Buffer.concat(chunks),
        });
      });
    });
    request.setTimeout(timeoutMs || 5000, function () {
      request.destroy(new Error('request timeout'));
    });
    request.on('error', reject);
    if (body) request.write(body);
    request.end();
  });
}

function parseJson(response) {
  return JSON.parse(response.body.toString('utf8'));
}

async function waitForHttp(configDir, timeoutMs) {
  const deadline = Date.now() + (timeoutMs || 15000);
  while (Date.now() < deadline) {
    const portFile = path.join(configDir, 'http.port');
    if (existsSync(portFile)) {
      const address = readFileSync(portFile, 'utf8').trim();
      const connected = await new Promise(function (resolve) {
        const socket = net.connect(Number(address.split(':').pop()), '127.0.0.1');
        socket.once('connect', function () { socket.destroy(); resolve(true); });
        socket.once('error', function () { resolve(false); });
      });
      if (connected) return address;
    }
    await new Promise(function (resolve) { setTimeout(resolve, 25); });
  }
  throw new Error('daemon HTTP never became ready');
}

function shellQuote(value) {
  return "'" + String(value).replace(/'/g, "'\\''") + "'";
}

async function startDaemon(root) {
  const env = Object.assign({}, process.env, {
    SVCD_CONFIG_DIR: path.join(root, 'config'),
    SVCD_DATA_DIR: path.join(root, 'data'),
    SVCD_LOG_DIR: path.join(root, 'logs'),
    SVCD_TRUST_DIR: path.join(root, 'config', 'trust.d'),
    SVCD_HTTP_PORT: '0',
    SVCD_TEST_UNPRIVILEGED: '1',
  });
  mkdirSync(env.SVCD_CONFIG_DIR, { recursive: true });
  mkdirSync(env.SVCD_TRUST_DIR, { recursive: true });
  mkdirSync(env.SVCD_DATA_DIR, { recursive: true });
  mkdirSync(env.SVCD_LOG_DIR, { recursive: true });
  copyFileSync(PUB, path.join(env.SVCD_TRUST_DIR, KEY_ID + '.pub'));
  // Release builds reject `svc lan token rotate` for non-root; seed the token
  // file before start instead. /mcp reads it even while LAN is disabled.
  const token = randomBytes(32).toString('hex');
  writeFileSync(path.join(env.SVCD_CONFIG_DIR, 'lan-token'), token);
  const logPath = path.join(root, 'daemon.stderr');
  const logFd = openSync(logPath, 'a');
  const child = spawn(SVCD, ['run', '--foreground'], {
    env: env,
    cwd: ROOT,
    stdio: ['ignore', 'ignore', logFd],
    windowsHide: true,
  });
  child.on('exit', function () { try { closeSync(logFd); } catch (_) {} });
  const address = await waitForHttp(env.SVCD_CONFIG_DIR);
  return { child: child, env: env, address: address, logPath: logPath, token: token };
}

async function stopDaemon(daemon) {
  try {
    runSync(SVCD, ['shutdown'], daemon.env);
  } catch (_) {}
  await new Promise(function (resolve) {
    const timer = setTimeout(function () { try { daemon.child.kill(); } catch (_) {} resolve(); }, 5000);
    daemon.child.once('exit', function () { clearTimeout(timer); resolve(); });
  });
}

function writeFixture(root) {
  const fixture = path.join(root, 'plugin');
  mkdirSync(path.join(fixture, 'payload', 'bin'), { recursive: true });
  mkdirSync(path.join(fixture, 'payload', 'web'), { recursive: true });
  const agent = [
    '#!/usr/bin/env python3',
    'import json, sys, threading, time',
    'lock = threading.Lock()',
    'event = 0',
    'def send(message):',
    '    global event',
    '    line = json.dumps(message, separators=(",", ":"))',
    '    with lock:',
    '        sys.stdout.write(line + "\\n")',
    '        sys.stdout.flush()',
    'for line in sys.stdin:',
    '    line = line.strip()',
    '    if not line:',
    '        continue',
    '    try:',
    '        request = json.loads(line)',
    '    except Exception:',
    '        continue',
    '    request_id = request.get("id")',
    '    method = request.get("method")',
    '    if method == "svcd.ping":',
    '        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})',
    '    elif method == "tools/call":',
    '        event += 1',
    '        send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": {"event": event}}})',
    '        send({"jsonrpc": "2.0", "id": request_id, "result": {"content": [{"type": "text", "text": "ok"}], "isError": False}})',
    '    elif request_id is not None:',
    '        send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "method not found"}})',
  ].join('\n') + '\n';
  writeFileSync(path.join(fixture, 'payload', 'bin', 'agent.py'), agent);
  const manifest = {
    schema: 1,
    name: 'perf',
    version: '1.0.0',
    desc: 'perf compatibility fixture',
    protocol: 1,
    exec: { path: WIN ? 'payload/bin/agent.cmd' : 'payload/bin/agent.sh', ready: 'notify' },
    capabilities: [{ name: 'echo', desc: 'echo', inputSchema: { type: 'object' } }],
    ui: { type: 'web', entry: 'index.html' },
  };
  writeFileSync(path.join(fixture, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  writeFileSync(path.join(fixture, 'payload', 'web', 'index.html'), '<!doctype html><html><body>perf</body></html>\n');
  if (WIN) {
    writeFileSync(path.join(fixture, 'payload', 'bin', 'agent.cmd'), '@echo off\r\npython "%~dp0agent.py"\r\n');
  } else {
    const script = '#!/bin/sh\nexec python3 "$(dirname "$0")/agent.py"\n';
    writeFileSync(path.join(fixture, 'payload', 'bin', 'agent.sh'), script);
    execFileSync('chmod', ['+x', path.join(fixture, 'payload', 'bin', 'agent.sh')]);
  }
  return fixture;
}

function buildAndInstall(root, daemon) {
  const fixture = writeFixture(root);
  const pluginCli = ensureAlias('plugin');
  const spk = path.join(root, 'perf.spk');
  const bundleOut = runSync(pluginCli, ['bundle', fixture, '--key', KEY, '--key-id', KEY_ID, '-o', spk], daemon.env);
  assert.ok(existsSync(spk), 'bundle did not produce ' + spk + ': ' + bundleOut);
  const packageBytes = readFileSync(spk);
  return { spk: spk, packageBytes: packageBytes };
}

async function installAndStart(daemon, packageBytes) {
  const installStart = performance.now();
  const installed = await httpRequest(daemon.address, 'POST', '/api/v1/plugins', { 'content-type': 'application/octet-stream' }, packageBytes, 15000);
  assert.equal(installed.status, 200, installed.body.toString());
  const installMs = performance.now() - installStart;
  const startStart = performance.now();
  const started = await httpRequest(daemon.address, 'POST', '/api/v1/services/perf/start', { 'content-length': '0' }, null, 15000);
  assert.equal(started.status, 200, started.body.toString());
  const startMs = performance.now() - startStart;
  const status = await httpRequest(daemon.address, 'GET', '/api/v1/services/perf', {}, null);
  assert.equal(status.status, 200, status.body.toString());
  return { status: parseJson(status), install_ms: installMs, start_ms: startMs };
}

function tokenFor(daemon) {
  const svc = ensureAlias('svc');
  const output = runSync(svc, ['lan', 'token', 'rotate'], daemon.env);
  const match = output.match(/token: (\S+)/);
  assert.ok(match, 'no token in: ' + output);
  return match[1];
}

function mcpPayload(method, params, id, name) {
  const request = {
    jsonrpc: '2.0',
    id: id,
    method: method,
    params: Object.assign({}, params || {}, {
      _meta: {
        'io.modelcontextprotocol/protocolVersion': '2026-07-28',
        'io.modelcontextprotocol/clientCapabilities': {},
      },
    }),
  };
  const headers = {
    'content-type': 'application/json',
    'MCP-Protocol-Version': '2026-07-28',
    'Mcp-Method': method,
  };
  if (name) headers['Mcp-Name'] = name;
  return { request: request, headers: headers };
}

async function mcpPost(daemon, token, method, params, id, name, keepAlive) {
  const payload = mcpPayload(method, params, id, name);
  payload.headers.Authorization = 'Bearer ' + token;
  const response = await httpRequest(daemon.address, 'POST', '/mcp', payload.headers, JSON.stringify(payload.request), 10000, keepAlive);
  return { status: response.status, json: parseJson(response) };
}

function wsOpen(url, headers) {
  return new Promise(function (resolve, reject) {
    const socket = new WebSocket(url, headers ? { headers: headers } : undefined);
    const timer = setTimeout(function () { socket.terminate(); reject(new Error('ws open timeout')); }, 5000);
    socket.once('open', function () { clearTimeout(timer); resolve(socket); });
    socket.once('error', function (error) { clearTimeout(timer); reject(error); });
    socket.once('unexpected-response', function (_request, response) {
      clearTimeout(timer);
      reject(new Error('ws handshake HTTP ' + response.statusCode));
    });
  });
}

function wsCollector(socket) {
  const messages = [];
  const waiters = [];
  socket.on('message', function (data) {
    let value;
    try { value = JSON.parse(data.toString()); } catch (_) { value = { raw: data.toString() }; }
    messages.push(value);
    for (const waiter of waiters.slice()) {
      if (waiter.predicate(value)) {
        waiters.splice(waiters.indexOf(waiter), 1);
        clearTimeout(waiter.timer);
        waiter.resolve(value);
      }
    }
  });
  return {
    messages: messages,
    waitFor: function (predicate, timeoutMs) {
      for (const value of messages) {
        if (predicate(value)) return Promise.resolve(value);
      }
      return new Promise(function (resolve, reject) {
        const timer = setTimeout(function () {
          const index = waiters.findIndex(function (item) { return item.timer === timer; });
          if (index >= 0) waiters.splice(index, 1);
          reject(new Error('ws message timeout'));
        }, timeoutMs || 5000);
        waiters.push({ predicate: predicate, resolve: resolve, timer: timer });
      });
    },
  };
}

async function sseRaw(address, lastEventId, trigger) {
  const url = new URL('http://' + address + '/plugin-ui/perf/events');
  return new Promise(function (resolve, reject) {
    const headers = { Accept: 'text/event-stream', Connection: 'close' };
    if (lastEventId !== undefined && lastEventId !== null) headers['Last-Event-ID'] = String(lastEventId);
    const request = http.request({ hostname: url.hostname, port: url.port, path: url.pathname, headers: headers }, function (response) {
      let buffer = '';
      response.setEncoding('utf8');
      response.on('data', function (chunk) {
        buffer += chunk;
        const blocks = buffer.split('\n\n');
        if (blocks.length < 2) return;
        const event = blocks[0];
        const idMatch = event.match(/^id: (\d+)$/m);
        const dataMatch = event.match(/^data: (.+)$/m);
        if (dataMatch) {
          response.destroy();
          resolve({ id: idMatch ? Number(idMatch[1]) : null, data: JSON.parse(dataMatch[1]) });
        }
      });
      if (trigger) trigger().catch(reject);
    });
    request.on('error', reject);
    request.setTimeout(5000, function () { request.destroy(new Error('sse timeout')); });
    request.end();
  });
}

function stats(samples) {
  if (samples.length === 0) return { n: 0, min: 0, avg: 0, p50: 0, p95: 0, p99: 0, max: 0 };
  const sorted = samples.slice().sort(function (a, b) { return a - b; });
  function pick(percent) {
    const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil((percent / 100) * sorted.length) - 1));
    return sorted[index];
  }
  const sum = sorted.reduce(function (a, b) { return a + b; }, 0);
  return {
    n: samples.length,
    min: sorted[0],
    avg: sum / sorted.length,
    p50: pick(50),
    p95: pick(95),
    p99: pick(99),
    max: sorted[sorted.length - 1],
  };
}

function processInfo(pid) {
  if (!pid) return null;
  try {
    const script = 'Get-Process -Id ' + pid + ' | Select-Object Id,CPU,WorkingSet64,PrivateMemorySize64 | ConvertTo-Json -Compress';
    const output = execFileSync('powershell', ['-NoProfile', '-Command', script], { encoding: 'utf8', windowsHide: true });
    return JSON.parse(output);
  } catch (_) {
    return null;
  }
}

function rpcBody(name, id) {
  return JSON.stringify({ jsonrpc: '2.0', id: id || 1, method: 'tools/call', params: { name: name, arguments: {} } });
}

async function uiRpc(daemon, name, id) {
  return httpRequest(daemon.address, 'POST', '/plugin-ui/perf/rpc', { 'content-type': 'application/json' }, rpcBody(name, id), 10000);
}

async function runCompat(daemon, token) {
  const checks = [];
  async function check(name, fn) {
    try {
      const detail = await fn();
      checks.push({ name: name, ok: true, detail: detail || '' });
      log('PASS ' + name);
    } catch (error) {
      checks.push({ name: name, ok: false, detail: error.message });
      log('FAIL ' + name + ': ' + error.message);
    }
  }

  await check('HTTP GET /api/v1/services: 200, JSON, Content-Length, Connection close', async function () {
    const response = await httpRequest(daemon.address, 'GET', '/api/v1/services', {}, null);
    assert.equal(response.status, 200);
    assert.match(String(response.headers['content-type']), /application\/json/);
    assert.equal(Number(response.headers['content-length']), response.body.length);
    assert.equal(String(response.headers['connection']).toLowerCase(), 'close');
    const body = parseJson(response);
    assert.ok(Array.isArray(body.services));
    return body.services.length + ' services';
  });


  await check('HTTP keep-alive serves two requests on one connection', async function () {
    const first = await httpRequest(daemon.address, 'GET', '/api/v1/services', {}, null, 5000, true);
    assert.equal(first.status, 200);
    assert.equal(String(first.headers.connection).toLowerCase(), 'keep-alive');
    const second = await httpRequest(daemon.address, 'GET', '/api/v1/services', {}, null, 5000, true);
    assert.equal(second.status, 200);
    assert.equal(String(second.headers.connection).toLowerCase(), 'keep-alive');
    return 'two responses on one keep-alive connection';
  });

  await check('HTTP 405 carries Allow and JSON error', async function () {
    const response = await httpRequest(daemon.address, 'POST', '/', {}, Buffer.from(''));
    assert.equal(response.status, 405);
    assert.equal(String(response.headers.allow), 'GET');
    assert.equal(parseJson(response).error.code, 'method_not_allowed');
  });

  await check('HTTP malformed request is rejected', async function () {
    let data = '';
    await new Promise(function (resolve, reject) {
      const socket = net.connect(Number(daemon.address.split(':').pop()), '127.0.0.1');
      socket.on('connect', function () { socket.write('GARBAGE\r\n\r\n'); });
      socket.on('data', function (chunk) { data += chunk.toString(); });
      socket.on('end', resolve);
      socket.on('error', reject);
      setTimeout(function () { socket.destroy(); resolve(); }, 3000);
    });
    assert.ok(data.startsWith('HTTP/1.1 400') || data === '', 'unexpected: ' + data.slice(0, 80));
  });


  await check('HTTP chunked request body is rejected', async function () {
    let data = '';
    await new Promise(function (resolve, reject) {
      const socket = net.connect(Number(daemon.address.split(':').pop()), '127.0.0.1');
      const payload = 'POST /api/v1/services/perf/start HTTP/1.1\r\nHost: ' + daemon.address + '\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n';
      socket.on('connect', function () { socket.write(payload); });
      socket.on('data', function (chunk) { data += chunk.toString(); });
      socket.on('end', resolve);
      socket.on('error', reject);
      setTimeout(function () { socket.destroy(); resolve(); }, 3000);
    });
    assert.ok(data.startsWith('HTTP/1.1 4'), 'unexpected: ' + data.slice(0, 80));
  });

  await check('MCP modern server/discover', async function () {
    const result = await mcpPost(daemon, token, 'server/discover', {}, 1);
    assert.equal(result.status, 200);
    assert.equal(result.json.result.resultType, 'complete');
    assert.deepEqual(result.json.result.supportedVersions, ['2026-07-28']);
    return 'versions=' + result.json.result.supportedVersions.join(',');
  });

  await check('MCP modern tools/list includes perf.echo', async function () {
    const result = await mcpPost(daemon, token, 'tools/list', {}, 2);
    assert.equal(result.status, 200);
    const names = result.json.result.tools.map(function (tool) { return tool.name; });
    assert.ok(names.includes('perf.echo'), names.join(','));
    return names.join(',');
  });

  await check('MCP modern tools/call returns result', async function () {
    const result = await mcpPost(daemon, token, 'tools/call', { name: 'perf.echo', arguments: {} }, 3, 'perf.echo');
    assert.equal(result.status, 200);
    assert.equal(result.json.result.isError, false);
    assert.equal(result.json.result.resultType, 'complete');
    return result.json.result.content[0].text;
  });

  await check('MCP modern rejects missing bearer (401)', async function () {
    const payload = mcpPayload('tools/list', {}, 4, null);
    const response = await httpRequest(daemon.address, 'POST', '/mcp', payload.headers, JSON.stringify(payload.request));
    assert.equal(response.status, 401);
  });

  await check('MCP modern rejects wrong bearer (403)', async function () {
    const payload = mcpPayload('tools/list', {}, 5, null);
    payload.headers.Authorization = 'Bearer wrong';
    const response = await httpRequest(daemon.address, 'POST', '/mcp', payload.headers, JSON.stringify(payload.request));
    assert.equal(response.status, 403);
  });

  await check('MCP modern rejects missing _meta (-32602)', async function () {
    const payload = mcpPayload('tools/list', {}, 6, null);
    payload.request.params = {};
    payload.headers.Authorization = 'Bearer ' + token;
    const response = await httpRequest(daemon.address, 'POST', '/mcp', payload.headers, JSON.stringify(payload.request));
    assert.equal(response.status, 400);
    assert.equal(parseJson(response).error.code, -32602);
  });

  await check('MCP stdio official SDK: initialize, list, call', async function () {
    const mcp = ensureAlias('mcp');
    const transport = new StdioClientTransport({
      command: mcp,
      args: [],
      env: Object.assign({}, process.env, daemon.env),
      stderr: 'pipe',
    });
    const client = new Client({ name: 'perf-compat', version: '1.0.0' });
    try {
      await client.connect(transport);
      const listed = await client.listTools();
      assert.ok(listed.tools.some(function (tool) { return tool.name === 'perf.echo'; }));
      const called = await client.callTool({ name: 'perf.echo', arguments: {} });
      assert.equal(called.isError, false);
      return listed.tools.length + ' tools';
    } finally {
      await client.close().catch(function () {});
    }
  });

  await check('WebSocket tools/call + notification', async function () {
    const socket = await wsOpen('ws://' + daemon.address + '/plugin-ui/perf/ws');
    try {
      const collector = wsCollector(socket);
      socket.send(JSON.stringify({ jsonrpc: '2.0', id: 21, method: 'tools/call', params: { name: 'perf.echo', arguments: {} } }));
      const response = await collector.waitFor(function (value) { return value.id === 21; });
      assert.equal(response.result.content[0].text, 'ok');
      const notification = await collector.waitFor(function (value) { return value.method === 'notifications/message'; });
      assert.ok(notification.params.data.event >= 1);
      return 'response + notification';
    } finally {
      socket.close();
    }
  });

  await check('WebSocket ping/pong and close', async function () {
    const socket = await wsOpen('ws://' + daemon.address + '/plugin-ui/perf/ws');
    const pong = new Promise(function (resolve) { socket.once('pong', resolve); });
    socket.ping('compat');
    await Promise.race([pong, new Promise(function (_, reject) { setTimeout(function () { reject(new Error('pong timeout')); }, 3000); })]);
    const closed = new Promise(function (resolve) { socket.once('close', resolve); });
    socket.close(1000);
    await closed;
  });

  await check('WebSocket fragmented text is reassembled', async function () {
    const socket = await wsOpen('ws://' + daemon.address + '/plugin-ui/perf/ws');
    try {
      const collector = wsCollector(socket);
      const message = JSON.stringify({ jsonrpc: '2.0', id: 22, method: 'tools/call', params: { name: 'perf.echo', arguments: {} } });
      socket.send(message.slice(0, 10), { fin: false });
      socket.send(message.slice(10), { fin: true });
      const response = await collector.waitFor(function (value) { return value.id === 22; });
      assert.equal(response.result.content[0].text, 'ok');
      return 'reassembled';
    } finally {
      socket.close();
    }
  });

  await check('WebSocket cross-origin is rejected', async function () {
    try {
      const socket = await wsOpen('ws://' + daemon.address + '/plugin-ui/perf/ws', { Origin: 'http://evil.example' });
      socket.terminate();
      throw new Error('cross-origin handshake unexpectedly succeeded');
    } catch (error) {
      assert.match(error.message, /403/);
    }
  });

  await check('WebSocket >1 MiB closes with 1009', async function () {
    const socket = await wsOpen('ws://' + daemon.address + '/plugin-ui/perf/ws');
    const closed = new Promise(function (resolve) { socket.once('close', function (code) { resolve(code); }); });
    socket.send('x'.repeat(1024 * 1024 + 1));
    const code = await Promise.race([closed, new Promise(function (_, reject) { setTimeout(function () { reject(new Error('close timeout')); }, 5000); })]);
    assert.equal(code, 1009);
  });

  await check('SSE EventSource receives notifications', async function () {
    const source = new EventSource('http://' + daemon.address + '/plugin-ui/perf/events');
    try {
      await new Promise(function (resolve, reject) {
        const timer = setTimeout(function () { reject(new Error('sse open timeout')); }, 5000);
        source.onopen = function () { clearTimeout(timer); resolve(); };
        source.onerror = function () { clearTimeout(timer); reject(new Error('sse error')); };
      });
      const message = new Promise(function (resolve, reject) {
        const timer = setTimeout(function () { reject(new Error('sse event timeout')); }, 5000);
        source.addEventListener('message', function (event) {
          clearTimeout(timer);
          resolve(event);
        });
      });
      await uiRpc(daemon, 'perf.echo', 31);
      const event = await message;
      assert.ok(event.data.includes('notifications/message'));
      return 'id=' + event.lastEventId;
    } finally {
      source.close();
    }
  });

  await check('SSE Last-Event-ID replays a missed event', async function () {
    const first = await sseRaw(daemon.address, null, function () { return uiRpc(daemon, 'perf.echo', 41); });
    assert.ok(first.id > 0);
    const secondCall = await uiRpc(daemon, 'perf.echo', 42);
    assert.equal(secondCall.status, 200);
    const replayed = await sseRaw(daemon.address, first.id, null);
    assert.ok(replayed.id > first.id, 'expected replay after ' + first.id + ', got ' + replayed.id);
    return 'first=' + first.id + ' replay=' + replayed.id;
  });

  return checks;
}
async function benchHttp(daemon, pathName, total, concurrency) {
  const samples = [];
  let errors = 0;
  let next = 0;
  const start = performance.now();
  async function worker() {
    while (true) {
      const index = next++;
      if (index >= total) return;
      const begin = performance.now();
      try {
        const response = await httpRequest(daemon.address, 'GET', pathName, {}, null, 10000, true);
        if (response.status !== 200) errors++;
      } catch (_) {
        errors++;
      }
      samples.push(performance.now() - begin);
    }
  }
  await Promise.all(Array.from({ length: concurrency }, worker));
  const elapsed = performance.now() - start;
  return { stats: stats(samples), errors: errors, elapsed_ms: elapsed, rps: total / (elapsed / 1000) };
}

async function benchMcp(daemon, token, method, name, total, concurrency) {
  const samples = [];
  let errors = 0;
  let next = 0;
  const start = performance.now();
  async function worker() {
    while (true) {
      const index = next++;
      if (index >= total) return;
      const begin = performance.now();
      try {
        const params = method === 'tools/call' ? { name: name, arguments: {} } : {};
        const response = await mcpPost(daemon, token, method, params, 1000 + index, method === 'tools/call' ? name : null, true);
        if (response.status !== 200 || !response.json.result) errors++;
      } catch (_) {
        errors++;
      }
      samples.push(performance.now() - begin);
    }
  }
  await Promise.all(Array.from({ length: concurrency }, worker));
  const elapsed = performance.now() - start;
  return { stats: stats(samples), errors: errors, elapsed_ms: elapsed, rps: total / (elapsed / 1000) };
}

async function benchWs(daemon, connections, perConnection) {
  const samples = [];
  let errors = 0;
  const start = performance.now();
  await Promise.all(Array.from({ length: connections }, async function (_, connection) {
    let socket;
    try {
      socket = await wsOpen('ws://' + daemon.address + '/plugin-ui/perf/ws');
      const collector = wsCollector(socket);
      for (let index = 0; index < perConnection; index++) {
        const id = 10000 + connection * perConnection + index;
        const begin = performance.now();
        socket.send(JSON.stringify({ jsonrpc: '2.0', id: id, method: 'tools/call', params: { name: 'perf.echo', arguments: {} } }));
        const response = await collector.waitFor(function (value) { return value.id === id; }, 10000);
        if (!response.result) errors++;
        samples.push(performance.now() - begin);
      }
    } catch (_) {
      errors++;
    } finally {
      if (socket) socket.close();
    }
  }));
  const elapsed = performance.now() - start;
  return { stats: stats(samples), errors: errors, elapsed_ms: elapsed, rps: (connections * perConnection) / (elapsed / 1000) };
}

async function benchSse(daemon, total) {
  const source = new EventSource('http://' + daemon.address + '/plugin-ui/perf/events');
  let count = 0;
  const start = performance.now();
  try {
    await new Promise(function (resolve, reject) {
      const timer = setTimeout(function () { reject(new Error('sse open timeout')); }, 5000);
      source.onopen = function () { clearTimeout(timer); resolve(); };
      source.onerror = function () { clearTimeout(timer); reject(new Error('sse error')); };
    });
    source.addEventListener('message', function () { count++; });
    for (let index = 0; index < total; index++) {
      await uiRpc(daemon, 'perf.echo', 20000 + index);
    }
    const deadline = Date.now() + 5000;
    while (count < total && Date.now() < deadline) {
      await new Promise(function (resolve) { setTimeout(resolve, 5); });
    }
  } finally {
    source.close();
  }
  const elapsed = performance.now() - start;
  return { events: count, total: total, elapsed_ms: elapsed, events_per_sec: count / (elapsed / 1000), dropped: total - count };
}

async function benchMixed(daemon, token, durationSeconds) {
  const end = Date.now() + durationSeconds * 1000;
  const latencies = { http: [], mcp: [], ws: [] };
  const ops = { http: 0, mcp: 0, ws: 0, sse: 0 };
  let errors = 0;
  const source = new EventSource('http://' + daemon.address + '/plugin-ui/perf/events');
  await new Promise(function (resolve, reject) {
    const timer = setTimeout(function () { reject(new Error('sse open timeout')); }, 5000);
    source.onopen = function () { clearTimeout(timer); resolve(); };
    source.onerror = function () { clearTimeout(timer); reject(new Error('sse error')); };
  });
  source.addEventListener('message', function () { ops.sse++; });

  async function httpWorker() {
    while (Date.now() < end) {
      const begin = performance.now();
      try {
        const response = await httpRequest(daemon.address, 'GET', '/api/v1/services', {}, null, 10000, true);
        if (response.status !== 200) errors++; else ops.http++;
      } catch (_) { errors++; }
      latencies.http.push(performance.now() - begin);
    }
  }
  async function mcpWorker() {
    let id = 30000;
    while (Date.now() < end) {
      const begin = performance.now();
      try {
        const response = await mcpPost(daemon, token, 'tools/list', {}, id++, null, true);
        if (response.status !== 200 || !response.json.result) errors++; else ops.mcp++;
      } catch (_) { errors++; }
      latencies.mcp.push(performance.now() - begin);
    }
  }
  async function wsWorker() {
    let socket;
    try {
      socket = await wsOpen('ws://' + daemon.address + '/plugin-ui/perf/ws');
      const collector = wsCollector(socket);
      let id = 40000;
      while (Date.now() < end) {
        const begin = performance.now();
        try {
          const requestId = id++;
          socket.send(JSON.stringify({ jsonrpc: '2.0', id: requestId, method: 'tools/call', params: { name: 'perf.echo', arguments: {} } }));
          const response = await collector.waitFor(function (value) { return value.id === requestId; }, 10000);
          if (!response.result) errors++; else ops.ws++;
        } catch (_) { errors++; }
        latencies.ws.push(performance.now() - begin);
      }
    } catch (_) { errors++; } finally {
      if (socket) socket.close();
    }
  }

  await Promise.all([
    httpWorker(), httpWorker(), httpWorker(), httpWorker(),
    mcpWorker(), mcpWorker(), mcpWorker(), mcpWorker(),
    wsWorker(), wsWorker(),
  ]);
  source.close();
  const elapsed = performance.now() - (end - durationSeconds * 1000);
  return {
    duration_seconds: durationSeconds,
    ops: ops,
    errors: errors,
    total_ops: ops.http + ops.mcp + ops.ws,
    rps: (ops.http + ops.mcp + ops.ws) / durationSeconds,
    sse_events_per_sec: ops.sse / durationSeconds,
    latencies: {
      http: stats(latencies.http),
      mcp: stats(latencies.mcp),
      ws: stats(latencies.ws),
    },
  };
}

function sleep(ms) {
  return new Promise(function (resolve) { setTimeout(resolve, ms); });
}

// Sum WorkingSet/CPU over the daemon process tree (plugin wrappers, Python
// children and their conhost helpers included). Windows-only; returns null
// elsewhere.
function processTree(pid) {
  if (!pid || !WIN) return null;
  const lines = [
    '$ErrorActionPreference = "SilentlyContinue"',
    '$root = ' + pid,
    '$all = Get-CimInstance Win32_Process',
    '$ids = New-Object System.Collections.Generic.HashSet[int]',
    '[void]$ids.Add([int]$root)',
    '$front = @([int]$root)',
    'while ($front.Count -gt 0) {',
    '  $next = @()',
    '  foreach ($process in $all) {',
    '    $parent = [int]$process.ParentProcessId',
    '    $child = [int]$process.ProcessId',
    '    if ($ids.Contains($parent) -and -not $ids.Contains($child)) { [void]$ids.Add($child); $next += $child }',
    '  }',
    '  $front = $next',
    '}',
    '$idList = @($ids)',
    '$procs = @(Get-Process -Id $idList -ErrorAction SilentlyContinue)',
    '$working = ($procs | Measure-Object WorkingSet64 -Sum).Sum',
    '$private = ($procs | Measure-Object PrivateMemorySize64 -Sum).Sum',
    '$cpu = ($procs | Measure-Object CPU -Sum).Sum',
    '[pscustomobject]@{ count = $procs.Count; working_set = [int64]$working; private = [int64]$private; cpu = [double]$cpu } | ConvertTo-Json -Compress',
  ];
  try {
    const output = execFileSync('powershell', ['-NoProfile', '-Command', lines.join('\n')], { encoding: 'utf8', windowsHide: true });
    return JSON.parse(output);
  } catch (_) {
    return null;
  }
}

function mb(bytes) {
  return bytes / (1024 * 1024);
}

function writeScriptFixture(root) {
  const fixture = path.join(root, 'plugin');
  mkdirSync(path.join(fixture, 'payload', 'bin'), { recursive: true });
  const exec = WIN ? 'payload/bin/agent.cmd' : 'payload/bin/agent.sh';
  if (WIN) {
    const script = [
      '@echo off',
      ':loop',
      'set "line="',
      'set /p line=',
      'if errorlevel 1 goto end',
      'echo {"jsonrpc":"2.0","id":1,"result":{"ok":true}}',
      'goto loop',
      ':end',
    ].join('\r\n') + '\r\n';
    writeFileSync(path.join(fixture, exec), script);
  } else {
    const script = '#!/bin/sh\nwhile IFS= read -r line; do\n  printf \'{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n\'\ndone\n';
    writeFileSync(path.join(fixture, exec), script);
    execFileSync('chmod', ['+x', path.join(fixture, exec)]);
  }
  const manifest = {
    schema: 1,
    name: 'perf',
    version: '1.0.0',
    desc: 'minimal script plugin for scale tests',
    protocol: 1,
    exec: { path: exec, ready: 'notify' },
    capabilities: [],
  };
  writeFileSync(path.join(fixture, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  return fixture;
}

async function measureScale() {
  const counts = [0, 5, 10, 30];
  const results = [];
  const pluginCli = ensureAlias('plugin');
  for (const count of counts) {
    const root = path.join(ROOT, 'target', 'scale-run-' + Date.now() + '-' + count);
    mkdirSync(root, { recursive: true });
    log('scale: starting ' + count + ' plugin(s) in ' + root);
    const daemon = await startDaemon(root);
    try {
      const fixture = writeScriptFixture(root);
      const packages = [];
      for (let index = 0; index < count; index++) {
        const id = 'p' + index;
        const spk = path.join(root, id + '.spk');
        runSync(pluginCli, ['bundle', fixture, '--id', id, '--key', KEY, '--key-id', KEY_ID, '-o', spk], daemon.env);
        packages.push({ id: id, bytes: readFileSync(spk) });
      }
      const startAll = performance.now();
      for (const item of packages) {
        const installed = await httpRequest(daemon.address, 'POST', '/api/v1/plugins', { 'content-type': 'application/octet-stream' }, item.bytes, 15000);
        assert.equal(installed.status, 200, installed.body.toString());
        const started = await httpRequest(daemon.address, 'POST', '/api/v1/services/' + item.id + '/start', { 'content-length': '0' }, null, 20000);
        assert.equal(started.status, 200, started.body.toString());
      }
      const startAllMs = performance.now() - startAll;
      await sleep(2000);
      const daemonBefore = processInfo(daemon.child.pid);
      const before = processTree(daemon.child.pid);
      await sleep(5000);
      const daemonAfter = processInfo(daemon.child.pid);
      const after = processTree(daemon.child.pid);
      const cpuSeconds = before && after ? Math.max(0, after.cpu - before.cpu) : 0;
      const daemonCpuSeconds = daemonBefore && daemonAfter ? Math.max(0, daemonAfter.CPU - daemonBefore.CPU) : 0;
      const pluginCpuSeconds = Math.max(0, cpuSeconds - daemonCpuSeconds);
      const cpuPercent = cpuSeconds / 5 * 100;
      const daemonCpuPercent = daemonCpuSeconds / 5 * 100;
      const pluginCpuPercent = pluginCpuSeconds / 5 * 100;
      const daemonRss = daemonAfter ? daemonAfter.WorkingSet64 : 0;
      results.push({
        plugins: count,
        root: root,
        processes: after ? after.count : 0,
        daemon_rss_bytes: daemonRss,
        plugin_rss_bytes: Math.max(0, (after ? after.working_set : 0) - daemonRss),
        total_rss_bytes: after ? after.working_set : 0,
        total_private_bytes: after ? after.private : 0,
        cpu_seconds_5s: cpuSeconds,
        daemon_cpu_percent_one_core: daemonCpuPercent,
        plugin_cpu_percent_one_core: pluginCpuPercent,
        cpu_percent_one_core: cpuPercent,
        cpu_percent_machine: cpuPercent / os.cpus().length,
        start_all_ms: startAllMs,
      });
      log('scale: ' + count + ' plugins, daemon RSS=' + mb(daemonRss).toFixed(1) + ' MB, plugin RSS=' + mb(Math.max(0, (after ? after.working_set : 0) - daemonRss)).toFixed(1) + ' MB, daemon CPU=' + daemonCpuPercent.toFixed(2) + '%');
    } finally {
      await stopDaemon(daemon);
    }
  }
  const baseline = results.length > 0 ? results[0].total_rss_bytes : 0;
  for (const item of results) {
    item.plugin_rss_bytes = Math.max(0, item.total_rss_bytes - baseline);
    item.per_plugin_rss_bytes = item.plugins > 0 ? item.plugin_rss_bytes / item.plugins : 0;
  }
  return results;
}

function renderScaleReport(results) {
  const lines = [];
  lines.push('# Plugin scale resource report');
  lines.push('');
  lines.push('Generated: ' + new Date().toISOString());
  lines.push('');
  lines.push('- OS: ' + os.type() + ' ' + os.release());
  lines.push('- CPU: ' + os.cpus()[0].model + ' x' + os.cpus().length);
  lines.push('- Plugin: minimal cmd/sh script (no Python)');
  lines.push('- Window: 5s idle sample after 2s settle; daemon is sampled directly, plugin RSS is total tree minus daemon.');
  lines.push('');
  lines.push('| Plugins | Processes | Daemon RSS MB | Plugin RSS MB | Total RSS MB | Per plugin MB | Daemon CPU % | Plugin CPU % | Total CPU % | Start all ms |');
  lines.push('| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |');
  for (const item of results) {
    lines.push('| ' + item.plugins + ' | ' + item.processes + ' | ' + mb(item.daemon_rss_bytes).toFixed(2) + ' | ' + mb(item.plugin_rss_bytes).toFixed(2) + ' | ' + mb(item.total_rss_bytes).toFixed(2) + ' | ' + mb(item.per_plugin_rss_bytes).toFixed(2) + ' | ' + item.daemon_cpu_percent_one_core.toFixed(2) + ' | ' + item.plugin_cpu_percent_one_core.toFixed(2) + ' | ' + item.cpu_percent_one_core.toFixed(2) + ' | ' + item.start_all_ms.toFixed(0) + ' |');
  }
  lines.push('');
  lines.push('## Notes');
  lines.push('');
  lines.push('- Daemon RSS/CPU comes from Get-Process by daemon pid; plugin values are the process tree minus the daemon.');
  lines.push('- CPU is the process-tree CPU seconds delta over 5s, normalized to one core.');
  lines.push('- The minimal script has no business logic and is idle; values show daemon overhead and per-plugin process cost, not production capacity.');
  lines.push('');
  return lines.join('\n');
}

async function runPerf(daemon, token, resources) {
  const perf = {};
  perf.http = {};
  for (const concurrency of [1, 8, 32]) {
    const total = concurrency === 1 ? 300 : 1000;
    perf.http['concurrency_' + concurrency] = await benchHttp(daemon, '/api/v1/services', total, concurrency);
  }
  perf.mcp = {};
  perf.mcp.list = {};
  perf.mcp.call = {};
  for (const concurrency of [1, 8, 32]) {
    const total = concurrency === 1 ? 300 : 1000;
    perf.mcp.list['concurrency_' + concurrency] = await benchMcp(daemon, token, 'tools/list', null, total, concurrency);
    perf.mcp.call['concurrency_' + concurrency] = await benchMcp(daemon, token, 'tools/call', 'perf.echo', total, concurrency);
  }
  perf.ws = {};
  for (const concurrency of [1, 8, 32]) {
    const perConnection = concurrency === 1 ? 300 : 100;
    perf.ws['connections_' + concurrency] = await benchWs(daemon, concurrency, perConnection);
  }
  perf.sse = await benchSse(daemon, 300);
  perf.mixed = await benchMixed(daemon, token, 10);
  if (resources) {
    const after = processInfo(resources.daemonPid);
    const pluginAfter = processInfo(resources.pluginPid);
    perf.resources = {
      daemon: { before: resources.daemonBefore, after: after },
      plugin: { before: resources.pluginBefore, after: pluginAfter },
    };
  }
  return perf;
}

function percent(value) {
  if (value === null || value === undefined) return 'n/a';
  return Number(value).toFixed(2);
}

function renderReport(results) {
  const lines = [];
  lines.push('# Core compatibility and performance report');
  lines.push('');
  lines.push('Generated: ' + results.generated_at);
  lines.push('');
  lines.push('## Environment');
  lines.push('');
  lines.push('- OS: ' + results.environment.os);
  lines.push('- CPU: ' + results.environment.cpu);
  lines.push('- Node: ' + results.environment.node);
  lines.push('- svcd: release build, ' + results.environment.binary_bytes + ' bytes');
  lines.push('- Plugin: Python NDJSON echo agent (hardware-independent overhead included)');
  lines.push('- HTTP port: ephemeral; loopback only; no TLS');
  lines.push('');
  lines.push('## Startup and install');
  lines.push('');
  lines.push('| Metric | ms |');
  lines.push('| --- | ---: |');
  lines.push('| daemon to HTTP ready | ' + percent(results.timings.daemon_ms) + ' |');
  lines.push('| plugin bundle | ' + percent(results.timings.bundle_ms) + ' |');
  lines.push('| plugin install | ' + percent(results.timings.install_ms) + ' |');
  lines.push('| plugin start | ' + percent(results.timings.start_ms) + ' |');
  lines.push('');
  if (results.compat) {
    lines.push('## Compatibility');
    lines.push('');
    lines.push('| Check | Result | Detail |');
    lines.push('| --- | --- | --- |');
    for (const check of results.compat) {
      lines.push('| ' + check.name + ' | ' + (check.ok ? 'PASS' : 'FAIL') + ' | ' + String(check.detail).replace(/\|/g, '\\|') + ' |');
    }
    lines.push('');
  }
  if (results.perf) {
    const perf = results.perf;
    lines.push('## HTTP performance');
    lines.push('');
    lines.push('| Concurrency | RPS | p50 ms | p95 ms | p99 ms | errors |');
    lines.push('| ---: | ---: | ---: | ---: | ---: | ---: |');
    for (const key of Object.keys(perf.http)) {
      const value = perf.http[key];
      lines.push('| ' + key.replace('concurrency_', '') + ' | ' + percent(value.rps) + ' | ' + percent(value.stats.p50) + ' | ' + percent(value.stats.p95) + ' | ' + percent(value.stats.p99) + ' | ' + value.errors + ' |');
    }
    lines.push('');
    for (const method of ['list', 'call']) {
      lines.push('## MCP ' + method + ' performance (Streamable HTTP)');
      lines.push('');
      lines.push('| Concurrency | RPS | p50 ms | p95 ms | p99 ms | errors |');
      lines.push('| ---: | ---: | ---: | ---: | ---: | ---: |');
      for (const key of Object.keys(perf.mcp[method])) {
        const value = perf.mcp[method][key];
        lines.push('| ' + key.replace('concurrency_', '') + ' | ' + percent(value.rps) + ' | ' + percent(value.stats.p50) + ' | ' + percent(value.stats.p95) + ' | ' + percent(value.stats.p99) + ' | ' + value.errors + ' |');
      }
      lines.push('');
    }
    lines.push('## WebSocket performance');
    lines.push('');
    lines.push('| Connections | RPS | p50 ms | p95 ms | p99 ms | errors |');
    lines.push('| ---: | ---: | ---: | ---: | ---: | ---: |');
    for (const key of Object.keys(perf.ws)) {
      const value = perf.ws[key];
      lines.push('| ' + key.replace('connections_', '') + ' | ' + percent(value.rps) + ' | ' + percent(value.stats.p50) + ' | ' + percent(value.stats.p95) + ' | ' + percent(value.stats.p99) + ' | ' + value.errors + ' |');
    }
    lines.push('');
    lines.push('## SSE performance');
    lines.push('');
    lines.push('| Events | Total | Events/sec | Dropped |');
    lines.push('| ---: | ---: | ---: | ---: |');
    lines.push('| ' + perf.sse.events + ' | ' + perf.sse.total + ' | ' + percent(perf.sse.events_per_sec) + ' | ' + perf.sse.dropped + ' |');
    lines.push('');
    lines.push('## Mixed workload (10s)');
    lines.push('');
    lines.push('| Metric | Value |');
    lines.push('| --- | ---: |');
    lines.push('| HTTP ops | ' + perf.mixed.ops.http + ' |');
    lines.push('| MCP ops | ' + perf.mixed.ops.mcp + ' |');
    lines.push('| WS ops | ' + perf.mixed.ops.ws + ' |');
    lines.push('| SSE events | ' + perf.mixed.ops.sse + ' |');
    lines.push('| Aggregate RPS | ' + percent(perf.mixed.rps) + ' |');
    lines.push('| Errors | ' + perf.mixed.errors + ' |');
    lines.push('');
    if (perf.resources) {
      lines.push('## Resources');
      lines.push('');
      lines.push('| Process | CPU before | CPU after | Working set before | Working set after |');
      lines.push('| --- | ---: | ---: | ---: | ---: |');
      for (const name of ['daemon', 'plugin']) {
        const before = perf.resources[name].before;
        const after = perf.resources[name].after;
        lines.push('| ' + name + ' | ' + (before ? percent(before.CPU) : 'n/a') + ' | ' + (after ? percent(after.CPU) : 'n/a') + ' | ' + (before ? before.WorkingSet64 : 'n/a') + ' | ' + (after ? after.WorkingSet64 : 'n/a') + ' |');
      }
      lines.push('');
    }
  }
  lines.push('## Notes');
  lines.push('');
  lines.push('- Compatibility HTTP checks use one-shot connections; performance HTTP/MCP workers use HTTP/1.1 keep-alive.');
  lines.push('- MCP Streamable HTTP is modern-only (2026-07-28), one JSON response per POST.');
  lines.push('- The Python plugin adds JSON encode/decode and interpreter scheduling overhead to MCP/WS/SSE call paths.');
  lines.push('- Numbers are from one Windows machine, not a production benchmark.');
  lines.push('');
  return lines.join('\n');
}

function environmentInfo() {
  const cpus = os.cpus();
  return {
    os: os.type() + ' ' + os.release() + ' (' + process.platform + ')',
    cpu: cpus.length > 0 ? cpus[0].model + ' x' + cpus.length : 'unknown',
    memory_bytes: os.totalmem(),
    node: process.version,
    binary_bytes: existsSync(SVCD) ? statSync(SVCD).size : 0,
  };
}

async function main() {
  const args = process.argv.slice(2);
  const scaleOnly = args.includes('--scale');
  const compatOnly = args.includes('--compat');
  const perfOnly = args.includes('--perf');
  const runCompatMode = compatOnly || (!scaleOnly && !compatOnly && !perfOnly);
  const runPerfMode = perfOnly || (!scaleOnly && !compatOnly && !perfOnly);
  const outIndex = args.indexOf('--out');
  const outPath = outIndex >= 0 ? path.resolve(args[outIndex + 1]) : path.join(ROOT, 'target', 'perf-results.json');
  const reportPath = path.join(ROOT, 'target', 'perf-report.md');
  if (!existsSync(SVCD)) throw new Error('missing release binary: ' + SVCD + ' (run cargo build --release)');
  ensureKey();
  ensureAlias('plugin');
  ensureAlias('svc');
  ensureAlias('mcp');
  if (scaleOnly) {
    const results = await measureScale();
    const scalePath = path.join(ROOT, 'target', 'scale-results.json');
    const scaleReport = path.join(ROOT, 'target', 'scale-report.md');
    writeFileSync(scalePath, JSON.stringify({ generated_at: new Date().toISOString(), environment: environmentInfo(), results: results }, null, 2) + '\n');
    writeFileSync(scaleReport, renderScaleReport(results));
    log('scale results ' + scalePath);
    log('scale report ' + scaleReport);
    return;
  }
  const root = path.join(ROOT, 'target', 'perf-run-' + Date.now());
  mkdirSync(root, { recursive: true });
  log('workdir ' + root);
  const daemonStart = performance.now();
  const daemon = await startDaemon(root);
  const daemonMs = performance.now() - daemonStart;
  let token;
  let status;
  let bundle;
  try {
    const bundleStart = performance.now();
    bundle = buildAndInstall(root, daemon);
    const bundleMs = performance.now() - bundleStart;
    const installResult = await installAndStart(daemon, bundle.packageBytes);
    status = installResult.status;
    const installMs = installResult.install_ms;
    const startMs = installResult.start_ms;
    token = daemon.token;
    const pluginPid = status.summary && status.summary.pid ? status.summary.pid : null;
    const daemonPid = daemon.child.pid;
    const daemonBefore = processInfo(daemonPid);
    const pluginBefore = processInfo(pluginPid);
    const results = {
      generated_at: new Date().toISOString(),
      environment: environmentInfo(),
      workdir: root,
      timings: { daemon_ms: daemonMs, bundle_ms: bundleMs, install_ms: installMs, start_ms: startMs },
      compat: null,
      perf: null,
    };
    if (runCompatMode) results.compat = await runCompat(daemon, token);
    if (runPerfMode) results.perf = await runPerf(daemon, token, {
      daemonPid: daemonPid,
      pluginPid: pluginPid,
      daemonBefore: daemonBefore,
      pluginBefore: pluginBefore,
    });
    mkdirSync(path.dirname(outPath), { recursive: true });
    writeFileSync(outPath, JSON.stringify(results, null, 2) + '\n');
    writeFileSync(reportPath, renderReport(results));
    log('results ' + outPath);
    log('report ' + reportPath);
    if (results.compat) {
      const failed = results.compat.filter(function (item) { return !item.ok; });
      log('compatibility: ' + (results.compat.length - failed.length) + '/' + results.compat.length + ' passed');
      if (failed.length > 0) {
        for (const item of failed) log('FAIL ' + item.name + ': ' + item.detail);
      }
    }
    if (results.perf) {
      log('HTTP c1 rps=' + percent(results.perf.http.concurrency_1.rps));
      log('mixed rps=' + percent(results.perf.mixed.rps) + ' errors=' + results.perf.mixed.errors);
    }
  } finally {
    await stopDaemon(daemon);
    KEEP_ALIVE_AGENT.destroy();
  }
}

main().catch(function (error) {
  console.error('[perf] fatal: ' + error.stack);
  process.exit(1);
});
