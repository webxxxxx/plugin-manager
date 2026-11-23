//! Shared HTTP integration-test fixtures.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::path::Path;
use std::time::{Duration, Instant};

use super::harness::*;
use super::support::target;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

/// One modern MCP request over Streamable HTTP. `name` mirrors
/// `params.name` into `Mcp-Name` for tools/call.
pub(crate) fn mcp_request(
    address: &str,
    token: &str,
    method: &str,
    params: Value,
    name: Option<&str>,
) -> Response {
    let mut params = params;
    if !params.is_object() {
        params = json!({});
    }
    params["_meta"] = modern_meta();
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("serialize mcp request");
    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {}", token)),
        ("MCP-Protocol-Version".to_string(), "2026-07-28".to_string()),
        ("Mcp-Method".to_string(), method.to_string()),
    ];
    if let Some(name) = name {
        headers.push(("Mcp-Name".to_string(), name.to_string()));
    }
    let headers: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    request_with(address, "POST", "/mcp", "application/json", &body, &headers)
}

pub(crate) fn tool_names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn script() -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        (
            "agent.cmd",
            b"@echo off\r\n:loop\r\nset \"line=\"\r\nset /p line=\r\nif errorlevel 1 goto end\r\necho {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\ngoto loop\r\n:end\r\n".to_vec(),
        )
    } else {
        (
            "agent.sh",
            b"#!/bin/sh\nwhile IFS= read -r line; do\n  printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\ndone\n".to_vec(),
        )
    }
}

/// A notify agent that announces itself on stderr once, so its service log is
/// known to be live before a clear removes it (#262).
fn chatty_script() -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        (
            "chatty.cmd",
            b"@echo off\r\necho chatty ready 1>&2\r\n:loop\r\nset \"line=\"\r\nset /p line=\r\nif errorlevel 1 goto end\r\necho {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\ngoto loop\r\n:end\r\n".to_vec(),
        )
    } else {
        (
            "chatty.sh",
            b"#!/bin/sh\nprintf 'chatty ready\\n' >&2\nwhile IFS= read -r line; do\n  printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\ndone\n".to_vec(),
        )
    }
}

/// The chatty agent as a signed package.
pub(crate) fn chatty_plugin_bundle(key: &SigningKey, id: &str) -> Vec<u8> {
    let (name, data) = chatty_script();
    let exec_path = format!("payload/bin/{}", name);
    let files = vec![(exec_path.clone(), data, 0o755)];
    let manifest = json!({
        "schema": 1,
        "id": id,
        "name": id,
        "version": "1.0.0",
        "desc": "chatty plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
    });
    signed_package(key, manifest, &files)
}

pub(crate) fn plugin_package(key: &SigningKey) -> Vec<u8> {
    plugin_bundle(key, "demo", "1.0.0", None, false)
}

pub(crate) fn plugin_bundle(
    key: &SigningKey,
    id: &str,
    version: &str,
    config: Option<&[u8]>,
    check: bool,
) -> Vec<u8> {
    let (manifest, files) = plugin_parts(id, version, config, check);
    signed_package(key, manifest, &files)
}

/// A signed package with one extra large payload file. Used by the export
/// tests so the download is still in flight while the client stalls.
pub(crate) fn plugin_bundle_with_payload(
    key: &SigningKey,
    id: &str,
    version: &str,
    payload: &[u8],
) -> Vec<u8> {
    let (manifest, mut files) = plugin_parts(id, version, None, false);
    files.push(("payload/big.bin".to_string(), payload.to_vec(), 0o644));
    files.sort_by(|left, right| left.0.cmp(&right.0));
    signed_package(key, manifest, &files)
}

/// Same payload as `plugin_bundle`, but without the `signature.json` entry.
pub(crate) fn unsigned_plugin_bundle(id: &str) -> Vec<u8> {
    let (manifest, files) = plugin_parts(id, "1.0.0", None, false);
    unsigned_package(manifest, &files)
}

pub(crate) fn plugin_parts(
    id: &str,
    version: &str,
    config: Option<&[u8]>,
    check: bool,
) -> (Value, Vec<(String, Vec<u8>, u32)>) {
    let (name, data) = script();
    let exec_path = format!("payload/bin/{}", name);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![(exec_path.clone(), data, 0o755)];
    if let Some(config) = config {
        files.push(("config/config".to_string(), config.to_vec(), 0o644));
    }
    let check_path = if check {
        let (path, body) = if cfg!(windows) {
            (
                "check/check.cmd",
                b"@echo off\r\nfindstr /C:\"good\" \"%SVCD_CONFIG%\" >nul\r\nexit /b %errorlevel%\r\n".to_vec(),
            )
        } else {
            (
                "check/check.sh",
                b"#!/bin/sh\ngrep -q good \"$SVCD_CONFIG\"\n".to_vec(),
            )
        };
        files.push((path.to_string(), body, 0o755));
        Some(path)
    } else {
        None
    };
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut manifest = json!({
        "schema": 1,
        "id": id,
        "name": id,
        "version": version,
        "desc": "http plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
    });
    if config.is_some() {
        manifest["config"] = json!("config/config");
    }
    if let Some(path) = check_path {
        manifest["configCheck"] = json!({"path": path});
    }
    (manifest, files)
}

pub(crate) fn attach_files(mut manifest: Value, files: &[(String, Vec<u8>, u32)]) -> Vec<u8> {
    manifest["files"] = json!(files
        .iter()
        .map(|(path, data, mode)| json!({
            "path": path,
            "size": data.len(),
            "sha256": hex_sha256(data),
            "mode": mode
        }))
        .collect::<Vec<Value>>());
    serde_json::to_vec(&manifest).expect("manifest")
}

/// Attach \`files[]\`, sign the manifest and wrap everything in the ustar layout
/// the installer verifies. Shared by the plain and web-UI fixtures.
pub(crate) fn signed_package(
    key: &SigningKey,
    manifest: Value,
    files: &[(String, Vec<u8>, u32)],
) -> Vec<u8> {
    let manifest = attach_files(manifest, files);
    let signature =
        json!({"alg":"ed25519","keyId":"test","sig":BASE64.encode(key.sign(&manifest).to_bytes())});
    let signature = serde_json::to_vec(&signature).expect("signature");
    let mut tar = Vec::new();
    tar.extend_from_slice(&header("manifest.json", manifest.len(), 0o644));
    tar.extend_from_slice(&manifest);
    tar.extend(std::iter::repeat_n(0, padding(manifest.len())));
    tar.extend_from_slice(&header("signature.json", signature.len(), 0o644));
    tar.extend_from_slice(&signature);
    tar.extend(std::iter::repeat_n(0, padding(signature.len())));
    for (path, data, mode) in files {
        tar.extend_from_slice(&header(path, data.len(), *mode));
        tar.extend_from_slice(data);
        tar.extend(std::iter::repeat_n(0, padding(data.len())));
    }
    tar.extend(std::iter::repeat_n(0, 1024));
    tar
}

/// A package with no `signature.json` entry at all; only the insecure install
/// path accepts it.
pub(crate) fn unsigned_package(manifest: Value, files: &[(String, Vec<u8>, u32)]) -> Vec<u8> {
    let manifest = attach_files(manifest, files);
    let mut tar = Vec::new();
    tar.extend_from_slice(&header("manifest.json", manifest.len(), 0o644));
    tar.extend_from_slice(&manifest);
    tar.extend(std::iter::repeat_n(0, padding(manifest.len())));
    for (path, data, mode) in files {
        tar.extend_from_slice(&header(path, data.len(), *mode));
        tar.extend_from_slice(data);
        tar.extend(std::iter::repeat_n(0, padding(data.len())));
    }
    tar.extend(std::iter::repeat_n(0, 1024));
    tar
}

/// A `.spkb` backup wrapping an unsigned inner package.
pub(crate) fn unsigned_backup(id: &str) -> Vec<u8> {
    unsigned_backup_with_schema(id, 1)
}

pub(crate) fn unsigned_backup_with_schema(id: &str, schema: u32) -> Vec<u8> {
    let spk = unsigned_plugin_bundle(id);
    let backup = serde_json::to_vec(&json!({"schema": schema, "id": id, "version": "1.0.0"}))
        .expect("backup");
    let mut tar = Vec::new();
    for (path, data, mode) in [
        ("backup.json", backup.as_slice(), 0o644u32),
        ("plugin.spk", spk.as_slice(), 0o644u32),
    ] {
        tar.extend_from_slice(&header(path, data.len(), mode));
        tar.extend_from_slice(data);
        tar.extend(std::iter::repeat_n(0, padding(data.len())));
    }
    tar.extend(std::iter::repeat_n(0, 1024));
    tar
}

/// A plugin-side NDJSON agent that answers \`svcd.ping\` and \`tools/call\` with a
/// real tool result, echoing the request id. The sh variant extracts the id with
/// sed; the PowerShell variant uses a regex.
pub(crate) fn web_agent() -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        (
            "agent.ps1",
            br#"$ErrorActionPreference = 'Stop'
while ($null -ne ($line = [Console]::In.ReadLine())) {
  $id = 1
  if ($line -match '"id"\s*:\s*(\d+)') { $id = [int]$Matches[1] }
  [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}')
  [Console]::Out.Flush()
}
"#
            .to_vec(),
        )
    } else {
        (
            "agent.sh",
            br#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -z "$id" ]; then id=1; fi
  printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id"
done
"#
            .to_vec(),
        )
    }
}

/// A plugin with \`payload/web/\` assets and one declared capability. \`ui\`
/// controls whether \`manifest.ui\` declares a browser UI at all.
pub(crate) fn web_plugin_bundle(key: &SigningKey, id: &str, ui: bool) -> Vec<u8> {
    let (name, data) = web_agent();
    let exec_path = format!("payload/bin/{}", name);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), data, 0o755),
        (
            "config/config".to_string(),
            b"initial config\n".to_vec(),
            0o644,
        ),
        (
            "payload/web/index.html".to_string(),
            b"<!doctype html>\n<html><head><title>demo ui</title></head><body>plugin page</body></html>"
                .to_vec(),
            0o644,
        ),
        (
            "payload/web/app.js".to_string(),
            b"export const answer = 42;\n".to_vec(),
            0o644,
        ),
        ("payload/web/large.html".to_string(), large_web_page(), 0o644),
    ];
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut manifest = json!({
        "schema": 1,
        "id": id,
        "name": id,
        "version": "1.0.0",
        "desc": "web ui plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "capabilities": [
            {"name": "status", "desc": "sample status", "inputSchema": {"type": "object"}}
        ],
    });
    manifest["config"] = json!("config/config");
    if ui {
        manifest["ui"] = json!({"type": "web", "entry": "index.html"});
    }
    signed_package(key, manifest, &files)
}

/// A page larger than the default stream buffer. Serving it end to end
/// proves the stream handler chunks its memory body instead of growing the
/// output buffer to the whole response.
pub(crate) fn large_web_page() -> Vec<u8> {
    let mut html =
        Vec::from(&b"<!doctype html>\n<html><head><title>large</title></head><body>\n"[..]);
    for line in 0..400 {
        html.extend_from_slice(
            format!(
                "<p>row {}: the plugin UI stream stays inside the configured buffer</p>\n",
                line
            )
            .as_bytes(),
        );
    }
    html.extend_from_slice(b"</body></html>\n");
    html
}

/// A web UI plugin whose agent emits a notification before every response.
pub(crate) fn web_event_agent() -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        (
            "event-agent.ps1",
            br#"$ErrorActionPreference = 'Stop'
while ($null -ne ($line = [Console]::In.ReadLine())) {
  $id = 1
  if ($line -match '"id"\s*:\s*(\d+)') { $id = [int]$Matches[1] }
  $token = $null
  if ($line -match '"_meta"\s*:\s*\{[^}]*"progressToken"\s*:\s*"([^"]+)"') { $token = $Matches[1] }
  if ($line -match '"name"\s*:\s*"slow"') {
    Start-Sleep -Seconds 5
  }
  if ($null -ne $token) {
    [Console]::Out.WriteLine('{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"' + $token + '"}}')
  }
  [Console]::Out.WriteLine('{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"pong-event"}}')
  [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}')
  [Console]::Out.Flush()
}
"#
            .to_vec(),
        )
    } else {
        (
            "event-agent.sh",
            br#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -z "$id" ]; then id=1; fi
  meta=$(printf '%s\n' "$line" | sed -n 's/.*"_meta":{\([^}]*\)}.*/\1/p')
  case "$line" in
    *'"name":"slow"'*) sleep 5 ;;
  esac
  token=$(printf '%s\n' "$meta" | sed -n 's/.*"progressToken":"\([^"]*\)".*/\1/p')
  if [ -n "$token" ]; then
    printf '{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"%s"}}\n' "$token"
  fi
  printf '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"pong-event"}}\n'
  printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id"
done
"#
            .to_vec(),
        )
    }
}

pub(crate) fn web_event_plugin_bundle(key: &SigningKey, id: &str) -> Vec<u8> {
    let (name, data) = web_event_agent();
    let exec_path = format!("payload/bin/{}", name);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), data, 0o755),
        (
            "payload/web/index.html".to_string(),
            b"<!doctype html>\n<html><body>events</body></html>".to_vec(),
            0o644,
        ),
    ];
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let manifest = json!({
        "schema": 1,
        "id": id,
        "name": id,
        "version": "1.0.0",
        "desc": "event ui plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "capabilities": [
            {"name": "status", "desc": "sample status", "inputSchema": {"type": "object"}},
            {"name": "slow", "desc": "slow status", "inputSchema": {"type": "object"}}
        ],
        "ui": {"type": "web", "entry": "index.html"},
    });
    signed_package(key, manifest, &files)
}

pub(crate) fn daemon_with_web_event_plugin(key_byte: u8) -> (Sandbox, String) {
    daemon_with_web_event_plugin_conf(key_byte, "")
}

/// [`daemon_with_web_event_plugin`] with extra document lines, so a test can pin
/// a setting whose value would otherwise follow the host's derived shape.
pub(crate) fn daemon_with_web_event_plugin_conf(key_byte: u8, extra: &str) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    // This UI test holds 16 keep-alive connections, 8 SSE streams and 16
    // concurrent page requests, all from 127.0.0.1, and its probes retry. State
    // the per-address cap instead of taking the shape's share: the point of the
    // test is threads, and a small host's 64-connection cap would refuse the
    // page load for a reason this test is not about.
    fs::write(
        sandbox.config.join("svcd.conf"),
        format!("max_connections_per_ip = 128\n{extra}"),
    )
    .expect("write svcd.conf");
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = web_event_plugin_bundle(&key, "demo");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    (sandbox, address)
}

/// A signed plugin whose post-install hook writes `marker` and then sleeps for
/// five seconds. The hook runs on the supervisor loop, so it holds the
/// `plugin.install_staged` control round trip open long enough to disconnect.
#[cfg(unix)]
pub(crate) fn slow_hook_plugin_bundle(key: &SigningKey, marker: &Path) -> Vec<u8> {
    let exec_path = "payload/bin/agent.sh".to_string();
    let hook_path = "hooks/post-install.sh".to_string();
    let hook = format!("#!/bin/sh\n: > '{}'\nsleep 5\n", marker.display());
    let files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), b"#!/bin/sh\nexit 0\n".to_vec(), 0o755),
        (hook_path.clone(), hook.into_bytes(), 0o755),
    ];
    let manifest = json!({
        "schema": 1,
        "id": "slowhook",
        "name": "slowhook",
        "version": "1.0.0",
        "desc": "slow post-install hook",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "hooks": {"postInstall": hook_path},
    });
    signed_package(key, manifest, &files)
}

pub(crate) fn ws_key() -> String {
    BASE64.encode([0x42u8; 16])
}

/// Open a WebSocket handshake and return (stream, status, response head).
pub(crate) fn ws_handshake(
    address: &str,
    path: &str,
    extra: &[(&str, &str)],
) -> (TcpStream, u16, String) {
    try_ws_handshake(address, path, extra)
        .unwrap_or_else(|error| panic!("ws handshake {path}: {error}"))
}

/// The fallible form: an admission refusal is a transport failure, not a
/// protocol verdict, so the caller can retry it.
pub(crate) fn try_ws_handshake(
    address: &str,
    path: &str,
    extra: &[(&str, &str)],
) -> Result<(TcpStream, u16, String), String> {
    let mut stream = TcpStream::connect(address).map_err(|error| format!("connect: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("timeout: {error}"))?;
    let key = ws_key();
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n",
        path, address, key
    );
    for (name, value) in extra {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write: {error}"))?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream
            .read_exact(&mut byte)
            .map_err(|error| format!("read head: {error}"))?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line {:?}", head))?;
    Ok((stream, status, head))
}

/// A handshake that has to reach the handler: retry until the daemon admits the
/// connection instead of panicking on the admission refusal (#155). A readable
/// 503 is the same refusal as a reset one: the gate answers busy without a
/// handler, so it is retried too.
pub(crate) fn ws_handshake_admitted(
    sandbox: &mut Sandbox,
    address: &str,
    path: &str,
    extra: &[(&str, &str)],
) -> (TcpStream, u16, String) {
    let deadline = Instant::now() + ADMISSION_WAIT;
    loop {
        match try_ws_handshake(address, path, extra) {
            Ok((stream, 503, _head)) => {
                drop(stream);
                if Instant::now() >= deadline || !daemon_state(sandbox).starts_with("running") {
                    regression_failure(
                        sandbox,
                        "max_connections",
                        &format!("GET {path} (WebSocket handshake)"),
                        "an admitted handshake within the admission wait",
                        "the admission gate kept answering 503",
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(handshake) => return handshake,
            Err(error) => {
                if Instant::now() >= deadline || !daemon_state(sandbox).starts_with("running") {
                    regression_failure(
                        sandbox,
                        "max_connections",
                        &format!("GET {path} (WebSocket handshake)"),
                        "an answered handshake within the admission wait",
                        &error,
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

pub(crate) fn ws_send(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    ws_send_with(stream, opcode, payload, true);
}

pub(crate) fn ws_send_with(stream: &mut TcpStream, opcode: u8, payload: &[u8], fin: bool) {
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![if fin { 0x80 | opcode } else { opcode }];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream.write_all(&frame).expect("ws frame");
    stream.flush().expect("ws flush");
}

pub(crate) fn ws_read(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).expect("ws frame head");
    let opcode = header[0] & 0x0f;
    assert_eq!(header[1] & 0x80, 0, "server frames must be unmasked");
    let mut length = u64::from(header[1] & 0x7f);
    if length == 126 {
        let mut extended = [0u8; 2];
        stream.read_exact(&mut extended).expect("ws length");
        length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0u8; 8];
        stream.read_exact(&mut extended).expect("ws length");
        length = u64::from_be_bytes(extended);
    }
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload).expect("ws payload");
    (opcode, payload)
}

pub(crate) fn sse_connect(address: &str, path: &str) -> TcpStream {
    sse_connect_with_last_id(address, path, None)
}

pub(crate) fn sse_connect_with_last_id(
    address: &str,
    path: &str,
    last_id: Option<u64>,
) -> TcpStream {
    let mut stream = TcpStream::connect(address).expect("sse connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("sse timeout");
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n",
        path, address
    );
    if let Some(last_id) = last_id {
        request.push_str(&format!("Last-Event-ID: {}\r\n", last_id));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("sse request");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("sse response head");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    assert!(
        head.to_ascii_lowercase().contains("text/event-stream"),
        "{}",
        head
    );
    stream
}

/// Open an SSE request and return its status without waiting for the stream
/// to end. Used when a long-lived response may be refused or accepted.
pub(crate) fn sse_probe(address: &str, path: &str) -> (u16, TcpStream) {
    let mut stream = TcpStream::connect(address).expect("sse connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("sse timeout");
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\r\n",
        path, address
    );
    stream.write_all(request.as_bytes()).expect("sse request");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("sse response head");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("sse status");
    (status, stream)
}

pub(crate) fn sse_id(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("id: "))
        .and_then(|value| value.trim().parse::<u64>().ok())
}

pub(crate) fn sse_wait_for_data(stream: &mut TcpStream, needle: &str) -> String {
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).expect("sse data");
        seen.push(byte[0]);
        if seen.ends_with(b"\n\n") {
            let text = String::from_utf8_lossy(&seen).to_string();
            if text.contains(needle) {
                return text;
            }
            seen.clear();
        }
    }
}

/// A protocol-3 (raw) plugin whose agent echoes stdin to stdout verbatim.
/// `cat` is enough: the daemon writes `len | stream | payload` frames and the
/// plugin must return them unchanged.
#[cfg(unix)]
pub(crate) fn raw_plugin_bundle(key: &SigningKey, id: &str) -> Vec<u8> {
    let exec_path = "payload/bin/raw-agent.sh".to_string();
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), b"#!/bin/sh\nexec cat\n".to_vec(), 0o755),
        (
            "payload/web/index.html".to_string(),
            b"<!doctype html>\n<html><body>raw</body></html>".to_vec(),
            0o644,
        ),
    ];
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let manifest = json!({
        "schema": 1,
        "id": id,
        "name": id,
        "version": "1.0.0",
        "desc": "raw echo plugin",
        "platform": target(),
        "protocol": 3,
        "exec": {"path": exec_path, "args": [], "ready": "exec"},
        "ui": {"type": "web", "entry": "index.html"},
    });
    signed_package(key, manifest, &files)
}

/// A protocol-2 agent: one `u32 len | json` frame per message. Python on Unix,
/// PowerShell on Windows. Answers the ready ping and every call with a pong
/// result, so an install+call proves the length-prefixed transport end to end.
pub(crate) fn length_prefixed_agent() -> (&'static str, Vec<u8>, Value) {
    if cfg!(windows) {
        let script = r#"$ErrorActionPreference = 'Stop'
$in = [Console]::OpenStandardInput()
$out = [Console]::OpenStandardOutput()
$len = [byte[]]::new(4)
while ($true) {
  $got = 0
  while ($got -lt 4) {
    $r = $in.Read($len, $got, 4 - $got)
    if ($r -le 0) { exit 0 }
    $got += $r
  }
  $size = ([uint32]$len[0] -shl 24) -bor ([uint32]$len[1] -shl 16) -bor ([uint32]$len[2] -shl 8) -bor [uint32]$len[3]
  $body = New-Object byte[] $size
  $got = 0
  while ($got -lt $size) {
    $r = $in.Read($body, $got, $size - $got)
    if ($r -le 0) { exit 0 }
    $got += $r
  }
  $text = [Text.Encoding]::UTF8.GetString($body)
  $id = 1
  if ($text -match '"id"\s*:\s*(\d+)') { $id = [int]$Matches[1] }
  $reply = [Text.Encoding]::UTF8.GetBytes('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}')
  $n = [uint32]$reply.Length
  $head = [byte[]]@(
    (($n -shr 24) -band 255), (($n -shr 16) -band 255), (($n -shr 8) -band 255), ($n -band 255))
  $out.Write($head, 0, 4)
  $out.Write($reply, 0, $reply.Length)
  $out.Flush()
}
"#;
        (
            "agent.ps1",
            script.as_bytes().to_vec(),
            json!({"name": "powershell", "args": ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]}),
        )
    } else {
        let script = r#"#!/usr/bin/env python3
import json, struct, sys

inp, out = sys.stdin.buffer, sys.stdout.buffer
while True:
    head = inp.read(4)
    if len(head) < 4:
        break
    size = struct.unpack(">I", head)[0]
    body = inp.read(size)
    if len(body) < size:
        break
    try:
        message = json.loads(body)
    except ValueError:
        continue
    reply = json.dumps({
        "jsonrpc": "2.0",
        "id": message.get("id", 1),
        "result": {"content": [{"type": "text", "text": "pong"}], "isError": False},
    }).encode()
    out.write(struct.pack(">I", len(reply)))
    out.write(reply)
    out.flush()
"#;
        (
            "agent.py",
            script.as_bytes().to_vec(),
            json!({"name": "python3", "version": ">=3.8"}),
        )
    }
}

/// Install and start a protocol-2 plugin; a 200 proves the length-prefixed
/// channel carried the ready ping (#260).
pub(crate) fn daemon_with_protocol_two_plugin(key_byte: u8, id: &str) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("");
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let (name, agent, interpreter) = length_prefixed_agent();
    let exec_path = format!("payload/bin/{}", name);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), agent, 0o755),
        (
            "payload/web/index.html".to_string(),
            b"<!doctype html>\n<html><body>protocol two</body></html>".to_vec(),
            0o644,
        ),
    ];
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let manifest = json!({
        "schema": 1,
        "id": id,
        "name": id,
        "version": "1.0.0",
        "desc": "protocol 2 agent",
        "platform": target(),
        "protocol": 2,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "interpreter": interpreter,
        "capabilities": [{"name": "echo", "desc": "echo", "inputSchema": {}}],
        "ui": {"type": "web", "entry": "index.html"},
    });
    let package = signed_package(&key, manifest, &files);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    let started = request(
        &address,
        "POST",
        &format!("/api/v1/services/{}/start", id),
        "",
        b"",
    );
    assert_eq!(started.status, 200, "{}", started.body);
    (sandbox, address)
}

/// A protocol-3 agent: JSON-RPC on stream 0 (ping, capability calls, one
/// notification) and raw echo on every other stream. Python on Unix,
/// PowerShell on Windows; both speak `u32 len | u32 stream`.
pub(crate) fn raw_json_agent(garbage: bool) -> (&'static str, Vec<u8>, Value) {
    if cfg!(windows) {
        let script = r#"$ErrorActionPreference = 'Stop'
$in = [Console]::OpenStandardInput()
$out = [Console]::OpenStandardOutput()
$garbage = __GARBAGE__
function Send-Frame([byte[]]$payload, [uint32]$stream) {
  $n = [uint32]$payload.Length
  $hdr = [byte[]]@(
    (($n -shr 24) -band 255), (($n -shr 16) -band 255), (($n -shr 8) -band 255), ($n -band 255),
    (($stream -shr 24) -band 255), (($stream -shr 16) -band 255), (($stream -shr 8) -band 255), ($stream -band 255))
  $out.Write($hdr, 0, 8)
  if ($payload.Length -gt 0) { $out.Write($payload, 0, $payload.Length) }
  $out.Flush()
}
function Read-Exact([int]$n) {
  $buf = New-Object byte[] $n
  $got = 0
  while ($got -lt $n) {
    $r = $in.Read($buf, $got, $n - $got)
    if ($r -le 0) { return $null }
    $got += $r
  }
  return ,$buf
}
while ($true) {
  $head = Read-Exact 8
  if ($null -eq $head) { break }
  $size = ([uint32]$head[0] -shl 24) -bor ([uint32]$head[1] -shl 16) -bor ([uint32]$head[2] -shl 8) -bor [uint32]$head[3]
  $stream = ([uint32]$head[4] -shl 24) -bor ([uint32]$head[5] -shl 16) -bor ([uint32]$head[6] -shl 8) -bor [uint32]$head[7]
  $body = Read-Exact ([int]$size)
  if ($null -eq $body) { break }
  if ($stream -eq 4294967295) {
    $text = [Text.Encoding]::UTF8.GetString($body)
    if ($text -match '"op"\s*:\s*"close"') {
      Send-Frame ([Text.Encoding]::UTF8.GetBytes('{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"stream-closed"}}')) 0
    }
    continue
  }
  if ($stream -eq 0) {
    $text = [Text.Encoding]::UTF8.GetString($body)
    if ($garbage -and ($text -notmatch '"method"\s*:\s*"svcd\.ping"')) {
      Send-Frame ([Text.Encoding]::UTF8.GetBytes('not json')) 0
      $garbage = $false
      continue
    }
    if ($text -match '"id"\s*:\s*(\d+)') {
      $id = [int]$Matches[1]
      Send-Frame ([Text.Encoding]::UTF8.GetBytes('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}')) 0
      Send-Frame ([Text.Encoding]::UTF8.GetBytes('{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"panel-event"}}')) 0
    }
  } else {
    Send-Frame $body $stream
    if ([Text.Encoding]::UTF8.GetString($body) -eq 'close-me') {
      Send-Frame ([Text.Encoding]::UTF8.GetBytes('{"op":"close","stream":' + $stream + '}')) 4294967295
    }
  }
}
"#;
        let text = script.replace("__GARBAGE__", if garbage { "$true" } else { "$false" });
        (
            "agent.ps1",
            text.into_bytes(),
            json!({"name": "powershell", "args": ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]}),
        )
    } else {
        let script = r#"#!/usr/bin/env python3
import json, struct, sys

inp, out = sys.stdin.buffer, sys.stdout.buffer
GARBAGE = __GARBAGE__
CONTROL = 0xFFFFFFFF


def send(stream, payload):
    out.write(struct.pack(">II", len(payload), stream))
    out.write(payload)
    out.flush()


def reply(message):
    rid = message.get("id")
    if rid is None:
        return
    send(0, json.dumps({
        "jsonrpc": "2.0",
        "id": rid,
        "result": {"content": [{"type": "text", "text": "pong"}], "isError": False},
    }).encode())
    send(0, json.dumps({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": {"level": "info", "data": "panel-event"},
    }).encode())


while True:
    head = inp.read(8)
    if len(head) < 8:
        break
    size, stream = struct.unpack(">II", head)
    body = inp.read(size)
    if len(body) < size:
        break
    if stream == CONTROL:
        try:
            message = json.loads(body)
        except ValueError:
            continue
        if message.get("op") == "close":
            send(0, json.dumps({
                "jsonrpc": "2.0",
                "method": "notifications/message",
                "params": {"level": "info", "data": "stream-closed"},
            }).encode())
        continue
    if stream == 0:
        try:
            message = json.loads(body)
        except ValueError:
            message = None
        if GARBAGE and (not message or message.get("method") != "svcd.ping"):
            send(0, b"not json")
            GARBAGE = False
            continue
        try:
            reply(message)
        except ValueError:
            pass
    else:
        send(stream, body)
        if body == b"close-me":
            send(CONTROL, json.dumps({"op": "close", "stream": stream}).encode())
"#;
        (
            "agent.py",
            script
                .replace("__GARBAGE__", if garbage { "True" } else { "False" })
                .into_bytes(),
            json!({"name": "python3", "version": ">=3.8"}),
        )
    }
}

/// A protocol-3 plugin that speaks JSON-RPC and raw at the same time (#265).
pub(crate) fn raw_json_plugin_bundle(key: &SigningKey, id: &str, garbage: bool) -> Vec<u8> {
    let (name, agent, interpreter) = raw_json_agent(garbage);
    let exec_path = format!("payload/bin/{}", name);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), agent, 0o755),
        (
            "payload/web/index.html".to_string(),
            b"<!doctype html>\n<html><body>raw json</body></html>".to_vec(),
            0o644,
        ),
    ];
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let manifest = json!({
        "schema": 1,
        "id": id,
        "name": id,
        "version": "1.0.0",
        "desc": "protocol 3 with JSON and raw",
        "platform": target(),
        "protocol": 3,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "interpreter": interpreter,
        "capabilities": [{"name": "echo", "desc": "echo", "inputSchema": {}}],
        "ui": {"type": "web", "entry": "index.html"},
    });
    signed_package(key, manifest, &files)
}

/// Install the mixed-channel plugin and leave it stopped. `garbage` makes the
/// first stream-0 answer non-JSON, which is how the protocol-violation test
/// drives a real instance into the stop path.
pub(crate) fn install_raw_json_plugin(key_byte: u8, id: &str, garbage: bool) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("");
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = raw_json_plugin_bundle(&key, id, garbage);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    (sandbox, address)
}

/// Install and start the mixed-channel plugin. A 200 here already proves the
/// ready ping crossed stream 0.
pub(crate) fn daemon_with_raw_json_plugin(key_byte: u8, id: &str) -> (Sandbox, String) {
    let (sandbox, address) = install_raw_json_plugin(key_byte, id, false);
    let started = request(
        &address,
        "POST",
        &format!("/api/v1/services/{}/start", id),
        "",
        b"",
    );
    assert_eq!(started.status, 200, "{}", started.body);
    (sandbox, address)
}

/// A running daemon with a protocol-3 plugin named `echo`.
#[cfg(unix)]
pub(crate) fn daemon_with_raw_plugin(key_byte: u8) -> (Sandbox, String) {
    daemon_with_raw_plugin_with(key_byte, "")
}

#[cfg(unix)]
pub(crate) fn daemon_with_raw_plugin_with(key_byte: u8, conf: &str) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with(conf);
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = raw_plugin_bundle(&key, "echo");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    let started = request(&address, "POST", "/api/v1/services/echo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    (sandbox, address)
}

/// Start a daemon and install the `demo` plugin (with a config template)
/// under a fresh trust key. Returns the sandbox and its HTTP address.
pub(crate) fn daemon_with_demo_plugin(key_byte: u8) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_bundle(&key, "demo", "1.0.0", Some(b"good template\n"), true);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    (sandbox, address)
}

/// A running daemon with a plugin whose payload is large enough that an
/// export cannot finish inside one socket buffer.
pub(crate) fn export_sandbox(key_byte: u8, extra: &str, payload: &[u8]) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_bundle_with_payload(&key, "demo", "1.0.0", payload);
    sandbox.allow_insecure_install_with(extra);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    (sandbox, address)
}

/// A running daemon with the demo web-UI plugin installed.
pub(crate) fn web_ui_sandbox(key_byte: u8) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = web_plugin_bundle(&key, "demo", true);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    (sandbox, address)
}

/// Like `web_ui_sandbox`, with extra `svcd.conf` keys for one test.
pub(crate) fn web_ui_sandbox_with_conf(key_byte: u8, extra: &str) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = web_plugin_bundle(&key, "demo", true);
    sandbox.allow_insecure_install_with(extra);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    (sandbox, address)
}

pub(crate) fn tar_names(bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut offset = 0;
    while offset + 512 <= bytes.len() {
        let header = &bytes[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let end = header[..100]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(100);
        names.push(String::from_utf8_lossy(&header[..end]).to_string());
        let size = std::str::from_utf8(&header[124..135])
            .ok()
            .and_then(|value| {
                usize::from_str_radix(value.trim_matches(char::from(0)).trim(), 8).ok()
            })
            .unwrap_or(0);
        offset += 512 + size + padding(size);
    }
    names
}

pub(crate) fn header(path: &str, size: usize, mode: u32) -> [u8; 512] {
    let mut header = [0u8; 512];
    header[..path.len()].copy_from_slice(path.as_bytes());
    let mode = format!("{:07o}", mode);
    header[100..100 + mode.len()].copy_from_slice(mode.as_bytes());
    let size = format!("{:011o}", size);
    header[124..124 + size.len()].copy_from_slice(size.as_bytes());
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header
}

pub(crate) fn padding(size: usize) -> usize {
    let remainder = size % 512;
    if remainder == 0 {
        0
    } else {
        512 - remainder
    }
}

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

pub(crate) fn pem_from_public(key: &[u8; 32]) -> Vec<u8> {
    const PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let mut der = PREFIX.to_vec();
    der.extend_from_slice(key);
    let body = BASE64.encode(der);
    let mut pem = String::from("-----BEGIN PUBLIC KEY-----\n");
    for chunk in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).expect("base64"));
        pem.push('\n');
    }
    pem.push_str("-----END PUBLIC KEY-----\n");
    pem.into_bytes()
}
