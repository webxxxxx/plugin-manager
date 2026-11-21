//! Shared HTTP integration-test checks.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::fixtures::*;
use super::harness::*;
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};

pub(crate) fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// A stalled download must hold one long-lived max_streams slot, keep the
/// server loop free, and release the slot when the client disconnects. Both
/// ends of the connection stay on the server loop; the client simply stops
/// reading after the head.
pub(crate) fn assert_slow_download_holds_the_cap(address: &str, path: &str, content: &[u8]) {
    let mut stalled = TcpStream::connect(address).expect("stalled connect");
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("stalled timeout");
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, address
    );
    stalled
        .write_all(request.as_bytes())
        .expect("stalled request");
    let (status, _) = read_response_head(&mut stalled);
    assert_eq!(status, 200);

    // The loop still answers an ordinary request while the transfer is stalled.
    let version = raw_request(address, "GET", "/api/v1/version", "", b"");
    assert_eq!(version.status, 200, "a slow reader must not block the loop");

    // The stalled transfer owns the only long-lived slot.
    let busy = raw_request(address, "GET", path, "", b"");
    assert_eq!(busy.status, 503, "{:?}", busy.body);
    assert_eq!(
        raw_header_values(&busy, "retry-after"),
        vec!["1"],
        "the busy response keeps the stable Retry-After contract"
    );
    assert_eq!(json_body(&busy)["error"]["code"], "busy");

    // A HEAD download refused by the same budget carries GET's head without a
    // body: the JSON envelope must not stay on the wire.
    let get_busy_length = header_value(&busy.headers, "content-length")
        .expect("GET 503 advertises its error body")
        .to_string();
    let head_busy = raw_request(address, "HEAD", path, "", b"");
    assert_eq!(head_busy.status, 503);
    assert!(
        head_busy.body.is_empty(),
        "HEAD 503 carried a body: {}",
        String::from_utf8_lossy(&head_busy.body)
    );
    assert_eq!(
        header_value(&head_busy.headers, "content-length"),
        Some(get_busy_length.as_str()),
        "HEAD 503 keeps the GET Content-Length"
    );
    assert_eq!(raw_header_values(&head_busy, "retry-after"), vec!["1"]);

    // Releasing the stalled connection releases the slot.
    drop(stalled);
    let mut released = None;
    for _ in 0..200 {
        let response = raw_request(address, "GET", path, "", b"");
        if response.status == 200 {
            released = Some(response);
            break;
        }
        assert_eq!(response.status, 503, "{:?}", response.body);
        std::thread::sleep(Duration::from_millis(10));
    }
    let released = released.expect("the stream cap must be released after a disconnect");
    assert_eq!(released.body.len(), content.len());
    assert_eq!(released.body, content);
}

/// Larger than any plausible loopback socket buffer, so the transfer is still
/// in progress while the client refuses to read.
pub(crate) fn large_download_body() -> Vec<u8> {
    vec![0x5a; 32 * 1024 * 1024]
}

pub(crate) fn has_staging(dir: &Path) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".upload-") || name.starts_with(".part-") {
                return true;
            }
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                stack.push(entry.path());
            }
        }
    }
    false
}

pub(crate) fn wait_until<F, D>(sandbox: &Sandbox, what: &str, ready: F, details: D)
where
    F: FnMut() -> bool,
    D: FnMut() -> String,
{
    wait_until_within(sandbox, what, Duration::from_secs(3), ready, details)
}

/// The bounded form for a wait that legitimately outlives the default three
/// seconds (a lifecycle hook, for example). It keeps the daemon-log tail on
/// timeout instead of a bare sleep loop.
pub(crate) fn wait_until_within<F, D>(
    sandbox: &Sandbox,
    what: &str,
    timeout: Duration,
    mut ready: F,
    mut details: D,
) where
    F: FnMut() -> bool,
    D: FnMut() -> String,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let extra = details();
    panic!("{what}\ndaemon log:\n{}\n{extra}", sandbox.daemon_log());
}

pub(crate) fn wait_for_staging(sandbox: &Sandbox, dir: &Path) {
    wait_until(
        sandbox,
        &format!("upload staging file did not appear under {}", dir.display()),
        || has_staging(dir),
        String::new,
    );
}

pub(crate) fn wait_for_no_staging(sandbox: &Sandbox, dir: &Path) {
    wait_until(
        sandbox,
        &format!(
            "upload staging file was not removed under {}",
            dir.display()
        ),
        || !has_staging(dir),
        String::new,
    );
}

/// A package install/import stages into a hidden directory directly under the
/// plugin tree, not into the blob staging area.
pub(crate) fn has_package_staging(plugins: &Path) -> bool {
    fs::read_dir(plugins)
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".staging-"))
        })
        .unwrap_or(false)
}

pub(crate) fn wait_for_package_staging_with<D>(sandbox: &Sandbox, plugins: &Path, details: D)
where
    D: FnMut() -> String,
{
    wait_until(
        sandbox,
        &format!(
            "package staging directory did not appear under {}",
            plugins.display()
        ),
        || has_package_staging(plugins),
        details,
    );
}

#[cfg_attr(windows, allow(dead_code))] // only the Unix thread-budget test drives it
pub(crate) fn wait_for_package_staging(sandbox: &Sandbox, plugins: &Path) {
    wait_for_package_staging_with(sandbox, plugins, String::new);
}

pub(crate) fn wait_for_no_package_staging(sandbox: &Sandbox, plugins: &Path) {
    wait_until(
        sandbox,
        &format!(
            "package staging directory was not removed under {}",
            plugins.display()
        ),
        || !has_package_staging(plugins),
        String::new,
    );
}

/// Open an upload and send only the head plus a first chunk. The server has a
/// valid content-length but the body is deliberately left unfinished.
pub(crate) fn start_stalled_upload(
    address: &str,
    method: &str,
    path: &str,
    declared: usize,
    initial: &[u8],
) -> TcpStream {
    let mut stream = TcpStream::connect(address).expect("stalled upload connect");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("stalled upload write timeout");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("stalled upload read timeout");
    let head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        method, path, address, declared
    );
    stream.write_all(head.as_bytes()).expect("upload head");
    stream.write_all(initial).expect("upload initial body");
    stream
}

pub(crate) fn second_upload_status(address: &str) -> (u16, Vec<(String, String)>) {
    let mut stream = TcpStream::connect(address).expect("second upload connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("second upload timeout");
    let head = format!(
        "POST /api/v1/blobs HTTP/1.1\r\nHost: {}\r\nContent-Length: 1\r\nConnection: close\r\n\r\n",
        address
    );
    stream
        .write_all(head.as_bytes())
        .expect("second upload head");
    read_response_head(&mut stream)
}

pub(crate) fn signed_demo_package(sandbox: &Sandbox, seed: u8) -> Vec<u8> {
    let key = SigningKey::from_bytes(&[seed; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    plugin_bundle(&key, "demo", "1.0.0", Some(b"good template\n"), false)
}

pub(crate) const TEST_PASSWORD: &str = "test-password";
pub(crate) const TEST_PASSWORD_2: &str = "new-test-password";

pub(crate) fn set_password(sandbox: &Sandbox, password: &str) {
    let changed = sandbox.run_svc(&["lan", "passwd", password]);
    assert_eq!(
        changed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
}

pub(crate) fn enable_auth(sandbox: &mut Sandbox) -> (String, String) {
    set_password(sandbox, TEST_PASSWORD);
    sandbox.shutdown();
    sandbox.start_daemon_with_bind("0.0.0.0:0");
    let address = sandbox.wait_for_http();
    (address, TEST_PASSWORD.to_string())
}

pub(crate) fn plugin_dir(sandbox: &Sandbox, id: &str) -> PathBuf {
    sandbox.data.join("plugins").join(id)
}

pub(crate) fn lan_status_field(text: &str, prefix: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("missing {prefix:?} in the control reply: {text:?}"))
        .trim()
        .to_string()
}

pub(crate) fn plugin_ui_base(address: &str, id: &str) -> String {
    plugin_ui_base_with(address, id, &[])
}

pub(crate) fn plugin_ui_base_with(address: &str, id: &str, headers: &[(&str, &str)]) -> String {
    let response = request_with(
        address,
        "POST",
        &format!("/api/v1/plugins/{}/ui-token", id),
        "",
        b"",
        headers,
    );
    assert_eq!(response.status, 200, "{}", response.body);
    response.body["url"]
        .as_str()
        .expect("plugin ui url")
        .to_string()
}

pub(crate) fn rpc_body(id: u64, name: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": {}}
    }))
    .expect("rpc body")
}

// ---------------------------------------------------------------------------
// S4 thread-budget regression (Linux only).
//
// /proc is the practical Unix way to read another process's thread count; the
// behavior tests stay platform-neutral, so this supplemental check is gated to
// Linux. #126 merged the plugin stdio reactor into the supervisor loop, so the
// default S4 baseline is four: the main/supervisor thread, the server loop and
// two blocking-pool workers. Keep-alive connections and SSE streams are driven
// by the server loop, so none of them may add a thread each.
// ---------------------------------------------------------------------------

/// The default S4 topology: supervisor (which also polls plugin stdio), server
/// loop and the two blocking workers the thread test pins with
/// `blocking_threads = 2`. Update this pin in the slice that changes the
/// baseline.
#[cfg(target_os = "linux")]
pub(crate) const IDLE_DAEMON_THREADS: usize = 4;

#[cfg(target_os = "linux")]
pub(crate) fn daemon_threads(pid: u32) -> usize {
    let status = fs::read_to_string(format!("/proc/{}/status", pid)).expect("daemon status");
    status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .and_then(|value| value.trim().parse().ok())
        .expect("daemon status has a Threads line")
}

#[cfg(target_os = "linux")]
pub(crate) fn daemon_thread_names(pid: u32) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(format!("/proc/{}/task", pid))
        .expect("daemon tasks")
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path().join("comm")).ok())
        .map(|name| name.trim().to_string())
        .collect();
    names.sort();
    names
}

#[cfg(target_os = "linux")]
pub(crate) fn stable_thread_count(pid: u32) -> usize {
    let mut count = daemon_threads(pid);
    let mut stable = 0;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(20));
        let next = daemon_threads(pid);
        if next == count {
            stable += 1;
            if stable == 5 {
                return count;
            }
        } else {
            count = next;
            stable = 0;
        }
    }
    count
}

#[cfg(target_os = "linux")]
pub(crate) fn open_keep_alive(address: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).expect("keep-alive connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("keep-alive timeout");
    let request = format!(
        "GET /api/v1/version HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    stream
        .write_all(request.as_bytes())
        .expect("keep-alive request");
    let response = read_one_response(&mut stream);
    assert_eq!(response.status, 200, "{}", response.body.len());
    stream
}

// ---------------------------------------------------------------------------
// #108: a live daemon must survive the smallest svcd.conf the parser takes.
//
// limits::tests proves the clamp arithmetic for these keys; this proves the
// running daemon actually uses the clamped values and stays up while each
// surface is exercised. Two configs share one entry point because the raw floor
// of a few keys is a *disabled* feature rather than a smaller one:
//
//   floor  - every key at the smallest value the parser accepts (0 where zero
//            is meaningful, 1 elsewhere). Every surface must answer with a
//            defined status (413 / 431 / 429 / 503) instead of dying.
//   usable - the same floors except the keys whose raw floor cannot carry one
//            live stream plus the request that feeds it:
//            max_connections / max_connections_per_ip 1 -> 2,
//            max_subscriptions / max_subscribers_per_plugin 0 -> 1,
//            default_timeout 0 -> 1000 ms, because a plugin round trip has to
//            finish inside the call budget before a stream can emit anything.
//            Every buffer and stack size stays at the floor.
//
// Each probe names the svcd.conf key it stresses; a failure prints that key,
// the exact request, the response and the daemon's own output.
// ---------------------------------------------------------------------------

/// A raw HTTP exchange that reports a dead socket instead of panicking, so a
/// failed probe can still name the svcd.conf key and the request it sent.
pub(crate) struct Exchange {
    pub(crate) status: u16,
    pub(crate) head: String,
    pub(crate) body: Vec<u8>,
    pub(crate) error: Option<String>,
}

impl Exchange {
    pub(crate) fn describe(&self) -> String {
        match &self.error {
            Some(error) => format!("no HTTP response: {error}"),
            None => {
                let body = String::from_utf8_lossy(&self.body);
                let body: String = body.chars().take(400).collect();
                format!("status {}\n{}body: {}", self.status, self.head, body)
            }
        }
    }

    pub(crate) fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

pub(crate) fn parse_exchange(raw: &[u8]) -> Exchange {
    let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Exchange {
            status: 0,
            head: String::new(),
            body: Vec::new(),
            error: Some(format!(
                "incomplete response {:?}",
                String::from_utf8_lossy(raw)
            )),
        };
    };
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    Exchange {
        status,
        head,
        body: raw[split + 4..].to_vec(),
        error: None,
    }
}

/// Write one request and read the response status line first, then the
/// declared body. A server that rejects an oversized request may answer and
/// close while the rest of that request is still in flight, which turns the
/// close into a reset; a client that only calls read_to_end can lose the
/// already-written status line to it. The status line is what the regression
/// asserts, so read it before any reset can matter.
pub(crate) fn exchange(address: &str, head: &str, body: &[u8]) -> Exchange {
    let mut stream = match TcpStream::connect(address) {
        Ok(stream) => stream,
        Err(error) => {
            return Exchange {
                status: 0,
                head: String::new(),
                body: Vec::new(),
                error: Some(format!("connect {address}: {error}")),
            }
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    if let Err(error) = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
    {
        return Exchange {
            status: 0,
            head: String::new(),
            body: Vec::new(),
            error: Some(format!("write: {error}")),
        };
    }

    let mut raw = Vec::new();
    let mut failure = None;
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                if raw.is_empty() {
                    failure = Some("connection closed before a response head".to_string());
                }
                break;
            }
            Ok(_) => {
                raw.push(byte[0]);
                if raw.ends_with(b"\r\n\r\n") {
                    break;
                }
                if raw.len() > 64 * 1024 {
                    failure = Some("response head is too large".to_string());
                    break;
                }
            }
            Err(error) => {
                if raw.is_empty() {
                    failure = Some(format!("read head: {error}"));
                }
                break;
            }
        }
    }
    if failure.is_none() && !raw.is_empty() {
        let text = String::from_utf8_lossy(&raw).to_string();
        let length = text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("content-length") {
                return None;
            }
            value.trim().parse::<usize>().ok()
        });
        if let Some(length) = length {
            let mut declared = vec![0u8; length.min(1024 * 1024)];
            let mut filled = 0;
            while filled < declared.len() {
                match stream.read(&mut declared[filled..]) {
                    Ok(0) => break,
                    Ok(read) => filled += read,
                    // A reset after the head must not hide the status line.
                    Err(_) => break,
                }
            }
            raw.extend_from_slice(&declared[..filled]);
        }
    }
    // Every probe sends Connection: close, so draining to EOF guarantees the
    // server already dropped this connection and freed its (one) admission
    // slot before the next probe connects. A reset here is irrelevant: the
    // status line is already captured.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut sink = [0u8; 512];
    while let Ok(read) = stream.read(&mut sink) {
        if read == 0 {
            break;
        }
    }

    let mut exchange = parse_exchange(&raw);
    // The read failure is the more specific diagnosis: with an empty buffer
    // `parse_exchange` can only say "incomplete response", hiding the reset or
    // read error that actually happened.
    exchange.error = failure.or(exchange.error);
    if exchange.error.is_some() {
        exchange.error = Some(format!(
            "{} (received {} bytes)",
            exchange.error.unwrap_or_default(),
            raw.len()
        ));
    }
    exchange
}

pub(crate) fn read_status_line(stream: &mut TcpStream) -> Result<u16, String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if head.len() > 8192 {
            return Err("response head is too long".to_string());
        }
        match stream.read(&mut byte) {
            Ok(0) => return Err("connection closed before a status line".to_string()),
            Ok(_) => {}
            Err(error) => return Err(format!("read: {error}")),
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line {:?}", text))
}

pub(crate) fn expect_status(
    sandbox: &mut Sandbox,
    key: &str,
    request: &str,
    allowed: &[u16],
    response: &Exchange,
) {
    if response.error.is_none() && allowed.contains(&response.status) {
        return;
    }
    regression_failure(
        sandbox,
        key,
        request,
        &format!("status in {allowed:?}"),
        &response.describe(),
    );
}

pub(crate) fn expect_error_code(
    sandbox: &mut Sandbox,
    key: &str,
    request: &str,
    code: &str,
    response: &Exchange,
) {
    if String::from_utf8_lossy(&response.body).contains(code) {
        return;
    }
    regression_failure(
        sandbox,
        key,
        request,
        &format!("an error with code {:?}", code),
        &response.describe(),
    );
}

pub(crate) fn expect_status_code(
    sandbox: &mut Sandbox,
    key: &str,
    request: &str,
    allowed: &[u16],
    status: u16,
    head: &str,
) {
    if allowed.contains(&status) {
        return;
    }
    regression_failure(
        sandbox,
        key,
        request,
        &format!("status in {allowed:?}"),
        &format!("status {status}\n{head}"),
    );
}

pub(crate) fn assert_daemon_alive(sandbox: &mut Sandbox, phase: &str) {
    let state = daemon_state(sandbox);
    if !state.starts_with("running") {
        regression_failure(
            sandbox,
            "liveness",
            &format!("daemon of the {phase} config"),
            "the process still running",
            &state,
        );
    }
    let mut panics: Vec<String> = Vec::new();
    if let Some(line) = sandbox
        .daemon_log()
        .lines()
        .find(|line| line.contains("panicked"))
    {
        panics.push(format!("stderr: {line}"));
    }
    if let Ok(entries) = fs::read_dir(&sandbox.logs) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(text) = fs::read_to_string(&path) {
                if let Some(line) = text.lines().find(|line| line.contains("panicked")) {
                    panics.push(format!("{}: {line}", path.display()));
                }
            }
        }
    }
    if !panics.is_empty() {
        regression_failure(
            sandbox,
            "liveness",
            &format!("daemon of the {phase} config"),
            "no panicked marker in stderr or the log files",
            &panics.join("\n"),
        );
    }
}
