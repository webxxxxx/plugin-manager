//! Shared HTTP integration-test scenarios.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::checks::*;
use super::fixtures::*;
use super::harness::*;
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};

/// True when the probe never reached a handler: the admission gate answered
/// busy, or the connection was reset before the response head. A complete
/// non-2xx answer (a real 4xx/5xx from a handler) is *not* a refusal, so the
/// retry loops return it immediately and report it as a failure.
pub(crate) fn admission_refused(probe: &Exchange) -> bool {
    probe.status == 0 || probe.status == 503 || probe.error.is_some()
}

/// One request that must be answered by a handler, not by the admission gate.
/// Retries an admission refusal against [`ADMISSION_WAIT`]; a stopped daemon is
/// the bail-out, so a crash is reported instead of waited on.
pub(crate) fn exchange_admitted(
    sandbox: &mut Sandbox,
    address: &str,
    head: &str,
    body: &[u8],
) -> Exchange {
    let deadline = Instant::now() + ADMISSION_WAIT;
    loop {
        let probe = exchange(address, head, body);
        if !admission_refused(&probe)
            || Instant::now() >= deadline
            || !daemon_state(sandbox).starts_with("running")
        {
            return probe;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The floor admits exactly one of two concurrent connections, the usable
/// config both. The pair is opened before either socket sends, so it races the
/// slot release of whatever probe came before; retry the whole pair until it
/// shows the configured shape (#155). The refused socket carries no request,
/// so its 503 is not lost to the reset a pending request would cause.
pub(crate) fn assert_concurrent_connections(sandbox: &mut Sandbox, address: &str, streams: bool) {
    let request = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    let deadline = Instant::now() + ADMISSION_WAIT;
    let mut last = String::from("no attempt");
    loop {
        let mut first = connect_for_probe(
            sandbox,
            "max_connections",
            "first concurrent connection",
            address,
        );
        let mut second = connect_for_probe(
            sandbox,
            "max_connections",
            "second concurrent connection",
            address,
        );
        for stream in [&first, &second] {
            if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
                regression_failure(
                    sandbox,
                    "max_connections",
                    "a concurrent connection",
                    "a socket with a read timeout",
                    &format!("read timeout: {error}"),
                );
            }
        }
        // Let the accept loop admit (or refuse) both sockets before either
        // writes, so the refusal arrives with no unread request data.
        std::thread::sleep(Duration::from_millis(100));
        let mut accepted = true;
        if streams {
            for (name, stream) in [("first", &mut first), ("second", &mut second)] {
                if let Err(error) = stream.write_all(request.as_bytes()) {
                    // Both sockets can be refused before the pair is retried.
                    last = format!("{name} concurrent connection: write: {error}");
                    accepted = false;
                    continue;
                }
                match read_status_line(stream) {
                    Ok(200) => {}
                    // 503 is the admission gate answering; wait for a slot.
                    Ok(503) => {
                        last = format!("{name} concurrent connection: status 503");
                        accepted = false;
                    }
                    // Any other complete answer is a real failure, not a
                    // refusal: report it now instead of retrying it away.
                    Ok(status) => regression_failure(
                        sandbox,
                        "max_connections",
                        &format!("{name} concurrent connection"),
                        "status 200",
                        &format!("status {status}"),
                    ),
                    Err(error) => {
                        last = format!("{name} concurrent connection: {error}");
                        accepted = false;
                    }
                }
            }
            if accepted {
                return;
            }
        } else {
            match read_status_line(&mut second) {
                Ok(503) => {
                    // The second socket was refused at accept, so the first one
                    // holds the only slot. A write can still fail when both
                    // sockets were refused before the pair was retried.
                    if let Err(error) = first.write_all(request.as_bytes()) {
                        last = format!("first (admitted) concurrent connection: write: {error}");
                    } else {
                        match read_status_line(&mut first) {
                            Ok(200) => return,
                            // The first socket was refused too: a fresh pair
                            // may still see the released slot.
                            Ok(503) => {
                                last =
                                    "first (admitted) concurrent connection: status 503".to_string()
                            }
                            Ok(status) => regression_failure(
                                sandbox,
                                "max_connections",
                                "first (admitted) concurrent connection",
                                "status 200",
                                &format!("status {status}"),
                            ),
                            Err(error) => {
                                last = format!("first (admitted) concurrent connection: {error}")
                            }
                        }
                    }
                }
                // A complete non-503 answer means the floor admitted both (or
                // answered something else): a real failure, not a retry.
                Ok(status) => regression_failure(
                    sandbox,
                    "max_connections",
                    "second concurrent connection (refused at accept, no request sent)",
                    "status 503",
                    &format!("status {status}"),
                ),
                Err(error) => {
                    last = format!(
                        "second concurrent connection (refused at accept, no request sent): {error}"
                    )
                }
            }
        }
        if Instant::now() >= deadline || !daemon_state(sandbox).starts_with("running") {
            regression_failure(
                sandbox,
                "max_connections",
                if streams {
                    "two concurrent connections"
                } else {
                    "first (admitted) concurrent connection"
                },
                if streams {
                    "status 200 on both connections"
                } else {
                    "status 200 on the admitted connection after a 503 for the second"
                },
                &last,
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Connect one probe socket, reporting a dead listener through the config-key
/// channel instead of a bare panic (docs/testing.md §6).
pub(crate) fn connect_for_probe(
    sandbox: &mut Sandbox,
    key: &str,
    what: &str,
    address: &str,
) -> TcpStream {
    match TcpStream::connect(address) {
        Ok(stream) => stream,
        Err(error) => regression_failure(
            sandbox,
            key,
            what,
            "a connectable daemon",
            &format!("connect {address}: {error}"),
        ),
    }
}

/// One MCP request over the Streamable HTTP binding.
pub(crate) fn mcp_min_exchange(
    sandbox: &mut Sandbox,
    address: &str,
    password: &str,
    method: &str,
    params: Value,
    name: Option<&str>,
) -> Exchange {
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
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: {}\r\n",
        address, password, method
    );
    if let Some(name) = name {
        head.push_str(&format!("Mcp-Name: {}\r\n", name));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    exchange_admitted(sandbox, address, &head, &body)
}

/// Read SSE events until one contains `needle`, reporting what arrived
/// instead of panicking, so a failure can name the config key and the stream.
pub(crate) fn sse_next_event(
    stream: &mut TcpStream,
    needle: &str,
    timeout: Duration,
) -> Result<String, String> {
    let _ = stream.set_read_timeout(Some(timeout));
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(format!(
                    "stream closed before an event containing {:?}; saw {:?}",
                    needle,
                    String::from_utf8_lossy(&seen)
                ))
            }
            Ok(_) => {
                seen.push(byte[0]);
                if seen.ends_with(b"\n\n") {
                    let text = String::from_utf8_lossy(&seen).to_string();
                    if text.contains(needle) {
                        return Ok(text);
                    }
                    seen.clear();
                }
            }
            Err(error) => {
                return Err(format!(
                    "read: {error}; saw {:?}",
                    String::from_utf8_lossy(&seen)
                ))
            }
        }
    }
}

/// Open an SSE stream, retrying the 429 a previous subscriber can cause while
/// the reactor has not noticed its EOF yet.
pub(crate) fn sse_open_retrying(
    address: &str,
    path: &str,
    last_id: Option<u64>,
) -> Result<TcpStream, String> {
    let mut last = String::new();
    for _ in 0..80 {
        let mut stream =
            TcpStream::connect(address).map_err(|error| format!("connect: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| format!("timeout: {error}"))?;
        let mut request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n",
            path, address
        );
        if let Some(last_id) = last_id {
            request.push_str(&format!("Last-Event-ID: {}\r\n", last_id));
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
        let text = String::from_utf8_lossy(&head).to_string();
        let status = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        if status == 200 {
            return Ok(stream);
        }
        if status != 429 {
            return Err(format!("status {status}: {text}"));
        }
        last = text;
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(format!(
        "the previous subscriber was never released: {last}"
    ))
}

/// An SSE stream that has to reach the handler. `sse_open_retrying` already
/// retries the subscriber-release 429; this also retries the admission refusal
/// and reports through the config key that the caller's probe belongs to
/// instead of panicking (#155).
pub(crate) fn sse_open_admitted(
    sandbox: &mut Sandbox,
    key: &str,
    address: &str,
    path: &str,
    last_id: Option<u64>,
) -> TcpStream {
    let deadline = Instant::now() + ADMISSION_WAIT;
    loop {
        match sse_open_retrying(address, path, last_id) {
            Ok(stream) => return stream,
            Err(error) => {
                if Instant::now() >= deadline || !daemon_state(sandbox).starts_with("running") {
                    regression_failure(
                        sandbox,
                        key,
                        &format!("GET {path} (SSE)"),
                        "an admitted SSE stream within the admission wait",
                        &error,
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

pub(crate) fn run_min_config_phase(phase: &str, streams: bool) {
    let overrides = if streams {
        USABLE_OVERRIDES
    } else {
        NO_OVERRIDES
    };
    let mut sandbox = Sandbox::new();
    fs::write(sandbox.config.join("svcd.conf"), minimal_conf(overrides)).expect("write svcd.conf");

    let key = SigningKey::from_bytes(&[0x6d; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = web_event_plugin_bundle(&key, "demo");

    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let port_before =
        fs::read_to_string(sandbox.data.join("run").join("http.port")).expect("http.port");

    // wait_for_http drops a bare TCP connection. With max_connections = 1 the
    // daemon can still count it when the first real request lands, so the first
    // probe is retried against the bounded admission wait instead of failing.
    let warmup = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    let probe = exchange_admitted(&mut sandbox, &address, &warmup, b"");
    if probe.status != 200 {
        regression_failure(
            &mut sandbox,
            "max_connections",
            "GET /api/v1/services (the first admitted request)",
            "status 200 within the admission wait",
            &probe.describe(),
        );
    }

    // /mcp always authenticates with the configured password.
    set_password(&sandbox, MIN_CONFIG_PASSWORD);

    // One installed plugin, so the plugin-UI surfaces (WS/SSE) exist at all.
    let installed = exchange_admitted(
        &mut sandbox,
        &address,
        &request_head(
            &address,
            "POST",
            "/api/v1/plugins",
            "application/octet-stream",
            package.len(),
            &[],
        ),
        &package,
    );
    expect_status(
        &mut sandbox,
        "max_package_bytes",
        "POST /api/v1/plugins (signed demo package)",
        &[200],
        &installed,
    );
    let started = exchange_admitted(
        &mut sandbox,
        &address,
        &request_head(&address, "POST", "/api/v1/services/demo/start", "", 0, &[]),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_connections",
        "POST /api/v1/services/demo/start",
        &[200],
        &started,
    );

    // max_package_bytes clamps to 1 MiB. Package installs are a streaming
    // takeover, so the gate is this key (not max_body_bytes, which only bounds
    // the non-tar import body): an over-size Content-Length is refused before a
    // single body byte is read. The compile-time defaults are 8/64/512 MiB.
    let over_package = exchange_admitted(
        &mut sandbox,
        &address,
        &request_head(
            &address,
            "POST",
            "/api/v1/plugins",
            "application/octet-stream",
            1024 * 1024 + 1,
            &[],
        ),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_package_bytes",
        "POST /api/v1/plugins with Content-Length: 1048577 and no body",
        &[413],
        &over_package,
    );
    expect_error_code(
        &mut sandbox,
        "max_package_bytes",
        "POST /api/v1/plugins with Content-Length: 1048577 and no body",
        "payload_too_large",
        &over_package,
    );

    // max_json_body_bytes clamps to 4 KiB on the ordinary JSON routes.
    let over_json = exchange_admitted(
        &mut sandbox,
        &address,
        &request_head(
            &address,
            "POST",
            "/api/v1/services/demo/start",
            "application/json",
            4 * 1024 + 1,
            &[],
        ),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_json_body_bytes",
        "POST /api/v1/services/demo/start with Content-Length: 4097 and no body",
        &[413],
        &over_json,
    );

    // max_header_bytes clamps to 1 KiB: a request just under the clamp parses...
    let pad = "a".repeat(700);
    let under_header = exchange_admitted(
        &mut sandbox,
        &address,
        &request_head(
            &address,
            "GET",
            "/api/v1/services",
            "",
            0,
            &[("X-Pad", pad.as_str())],
        ),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_header_bytes",
        "GET /api/v1/services with a ~800-byte header block",
        &[200],
        &under_header,
    );

    // ...and one just over it is refused with the RFC 9110 §15.5.20 status.
    // 8/16/64 KiB (the compile-time defaults) would accept ~1.1 KiB, so this is
    // the clamp in action (#106).
    let just_over = "a".repeat(1000);
    let over_header = exchange_admitted(
        &mut sandbox,
        &address,
        &request_head(
            &address,
            "GET",
            "/api/v1/services",
            "",
            0,
            &[("X-Pad", just_over.as_str())],
        ),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_header_bytes",
        "GET /api/v1/services with a ~1.1 KiB header block (clamp is 1024)",
        &[431],
        &over_header,
    );
    expect_error_code(
        &mut sandbox,
        "max_header_bytes",
        "GET /api/v1/services with a ~1.1 KiB header block (clamp is 1024)",
        "headers_too_large",
        &over_header,
    );

    // A much larger header block is the abortive-close case: the daemon answers
    // and closes while the request is still arriving, so an abortive close can
    // discard the status line before the client reads it. The rejection path
    // now half-closes and drains, so the 431 and its error code must survive
    // this input (#106).
    let big = "a".repeat(4 * 1024);
    let huge_head = request_head(
        &address,
        "GET",
        "/api/v1/services",
        "",
        0,
        &[("X-Pad", big.as_str())],
    );
    let mut huge_header = exchange(&address, &huge_head, b"");
    // A readable 503 here is the admission gate answering, not the header
    // verdict: wait for a slot instead of reading it as a regression.
    let huge_deadline = Instant::now() + ADMISSION_WAIT;
    while huge_header.status == 503
        && Instant::now() < huge_deadline
        && daemon_state(&mut sandbox).starts_with("running")
    {
        std::thread::sleep(Duration::from_millis(25));
        huge_header = exchange(&address, &huge_head, b"");
    }
    if huge_header.status != 431 {
        regression_failure(
            &mut sandbox,
            "max_header_bytes",
            "GET /api/v1/services with a 4 KiB header block",
            "431 with a readable status line",
            &huge_header.describe(),
        );
    }
    expect_error_code(
        &mut sandbox,
        "max_header_bytes",
        "GET /api/v1/services with a 4 KiB header block",
        "headers_too_large",
        &huge_header,
    );
    assert_daemon_alive(&mut sandbox, phase);

    // Concurrent connections: the floor admits one connection and refuses the
    // second with 503 at accept time; the usable config admits two. Both hosts
    // enforce this: the poll loop counts its connection table before it admits
    // a socket.
    assert_concurrent_connections(&mut sandbox, &address, streams);

    // http_idle_timeout_secs clamps to 1 s: the poll loop's idle sweep closes a
    // keep-alive connection instead of idling for the compile-time default
    // (30/60/120 s).
    // The keep-alive probe needs an admitted slot too, so retry the handshake
    // against the admission wait.
    let idle_request = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    let idle_deadline = Instant::now() + ADMISSION_WAIT;
    let (mut idle, _kept) = loop {
        let mut stream = connect_for_probe(
            &mut sandbox,
            "http_idle_timeout_secs",
            "keep-alive GET /api/v1/services",
            &address,
        );
        if let Err(error) = stream.set_read_timeout(Some(Duration::from_millis(500))) {
            regression_failure(
                &mut sandbox,
                "http_idle_timeout_secs",
                "keep-alive GET /api/v1/services",
                "a socket with a read timeout",
                &format!("read timeout: {error}"),
            );
        }
        let kept = match stream.write_all(idle_request.as_bytes()) {
            Ok(()) => read_one_response(&mut stream),
            // A refused connection can be reset on write: not admitted yet.
            Err(_) => RawResponse {
                status: 0,
                body: Vec::new(),
                headers: Vec::new(),
            },
        };
        if kept.status == 200 {
            break (stream, kept);
        }
        if kept.status != 0 && kept.status != 503 {
            // A complete answer that is neither success nor the admission
            // gate is a real failure; do not retry it away.
            regression_failure(
                &mut sandbox,
                "http_idle_timeout_secs",
                "keep-alive GET /api/v1/services",
                "status 200 on the keep-alive connection",
                &format!("status {}", kept.status),
            );
        }
        if Instant::now() >= idle_deadline || !daemon_state(&mut sandbox).starts_with("running") {
            regression_failure(
                &mut sandbox,
                "http_idle_timeout_secs",
                "keep-alive GET /api/v1/services",
                "status 200 on the keep-alive connection",
                &format!("status {}", kept.status),
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let opened = Instant::now();
    let mut closed = false;
    let mut sink = [0u8; 128];
    while opened.elapsed() < Duration::from_secs(8) {
        match idle.read(&mut sink) {
            Ok(0) => {
                closed = true;
                break;
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    if !closed {
        regression_failure(
            &mut sandbox,
            "http_idle_timeout_secs",
            "keep-alive GET /api/v1/services left idle",
            "the daemon closes an idle connection within ~1 s",
            "still open after 8 s",
        );
    }

    // MCP over Streamable HTTP: tools/list and tools/call. The modern binding
    // has no initialize handshake, so that method must answer 404 -32601
    // rather than drop the connection.
    let listed = mcp_min_exchange(
        &mut sandbox,
        &address,
        MIN_CONFIG_PASSWORD,
        "tools/list",
        json!({}),
        None,
    );
    expect_status(
        &mut sandbox,
        "max_json_body_bytes",
        "POST /mcp tools/list",
        &[200],
        &listed,
    );
    let listed = listed.json().unwrap_or_else(|| {
        regression_failure(
            &mut sandbox,
            "max_json_body_bytes",
            "POST /mcp tools/list",
            "a JSON-RPC response",
            &listed.describe(),
        )
    });
    let names = tool_names(&listed["result"]["tools"]);
    if !names.contains(&"svc.health".to_string()) {
        regression_failure(
            &mut sandbox,
            "max_json_body_bytes",
            "POST /mcp tools/list",
            "svc.health in result.tools",
            &format!("tools {names:?}"),
        );
    }

    let called = mcp_min_exchange(
        &mut sandbox,
        &address,
        MIN_CONFIG_PASSWORD,
        "tools/call",
        json!({"name": "svc.health", "arguments": {}}),
        Some("svc.health"),
    );
    expect_status(
        &mut sandbox,
        "default_timeout",
        "POST /mcp tools/call svc.health (100 ms budget at the floor)",
        &[200],
        &called,
    );
    let called = called.json().unwrap_or_else(|| {
        regression_failure(
            &mut sandbox,
            "default_timeout",
            "POST /mcp tools/call svc.health",
            "a JSON-RPC response",
            &called.describe(),
        )
    });
    if called["result"].is_null() || called["result"]["isError"] == json!(true) {
        regression_failure(
            &mut sandbox,
            "default_timeout",
            "POST /mcp tools/call svc.health (100 ms budget at the floor)",
            "a successful result",
            &called.to_string(),
        );
    }

    let initialize = mcp_min_exchange(
        &mut sandbox,
        &address,
        MIN_CONFIG_PASSWORD,
        "initialize",
        json!({}),
        None,
    );
    expect_status(
        &mut sandbox,
        "max_json_body_bytes",
        "POST /mcp initialize (not spoken on the modern binding)",
        &[404],
        &initialize,
    );

    // The stdio bridge still speaks the legacy handshake; it reaches the same
    // daemon over the control channel, not over HTTP.
    let bridged = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "min-config", "version": "1"}
        }
    }));
    if bridged["result"]["serverInfo"]["name"] != json!("svcd") {
        regression_failure(
            &mut sandbox,
            "default_timeout",
            "stdio bridge initialize",
            "result.serverInfo.name = svcd",
            &bridged.to_string(),
        );
    }
    let bridged_list = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {"_meta": modern_meta()}
    }));
    let bridged_names = tool_names(&bridged_list["result"]["tools"]);
    if !bridged_names.contains(&"svc.health".to_string()) {
        regression_failure(
            &mut sandbox,
            "default_timeout",
            "stdio bridge tools/list",
            "svc.health in result.tools",
            &format!("tools {bridged_names:?}"),
        );
    }

    let base = plugin_ui_base(&address, "demo");
    let ws_path = format!("{}ws", base);
    let events_path = format!("{}events", base);

    if streams {
        let (mut ws, status, head) = ws_handshake_admitted(&mut sandbox, &address, &ws_path, &[]);
        expect_status_code(
            &mut sandbox,
            "max_subscribers_per_plugin",
            "GET <base>ws (Upgrade: websocket)",
            &[101],
            status,
            &head,
        );
        let call = json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {"name": "status", "arguments": {}}
        });
        ws_send(&mut ws, 0x1, call.to_string().as_bytes());
        let mut answered = false;
        for _ in 0..8 {
            let (opcode, payload) = ws_read(&mut ws);
            if opcode != 0x1 {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&payload) else {
                continue;
            };
            if value["id"] == json!(21) && value["result"]["content"][0]["text"] == json!("pong") {
                answered = true;
                break;
            }
        }
        if !answered {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "WebSocket tools/call status",
                "a JSON-RPC result frame with id 21",
                "no matching frame",
            );
        }

        // The plugin's own notification for the previous call is still queued,
        // so skip text frames until the pong arrives.
        ws_send(&mut ws, 0x9, b"min-config");
        let mut pong = None;
        for _ in 0..8 {
            let (opcode, payload) = ws_read(&mut ws);
            if opcode == 0xA {
                pong = Some(payload);
                break;
            }
        }
        if pong.as_deref() != Some(b"min-config".as_ref()) {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "WebSocket ping frame",
                "a pong frame echoing the payload",
                &format!("pong {pong:?}"),
            );
        }

        // ws_max_message_bytes clamps to 1 KiB: a larger message is closed with
        // 1009 instead of growing the buffer or dropping the socket silently.
        ws_send(&mut ws, 0x1, &vec![b'x'; 2048]);
        let mut close = None;
        for _ in 0..8 {
            let (opcode, payload) = ws_read(&mut ws);
            if opcode == 0x8 {
                close = Some(payload);
                break;
            }
        }
        let code = close
            .as_ref()
            .and_then(|payload| payload.get(0..2))
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
            .unwrap_or(0);
        if code != 1009 {
            regression_failure(
                &mut sandbox,
                "ws_max_message_bytes",
                "WebSocket text frame of 2048 bytes (clamp is 1024)",
                "a close frame with code 1009",
                &format!("close {close:?}"),
            );
        }
        drop(ws);

        // SSE: connect, receive one event, then reconnect with Last-Event-ID and
        // receive the event published while no subscriber was attached. The
        // subscriber slot and the connection slot are both released by the
        // server asynchronously, so the open waits them out instead of failing.
        let mut sse = sse_open_admitted(
            &mut sandbox,
            "max_subscribers_per_plugin",
            &address,
            &events_path,
            None,
        );
        let rpc_path = format!("{}rpc", base);
        let call = br#"{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"status","arguments":{}}}"#;
        let answered = exchange_admitted(
            &mut sandbox,
            &address,
            &request_head(
                &address,
                "POST",
                &rpc_path,
                "application/json",
                call.len(),
                &[],
            ),
            call,
        );
        expect_status(
            &mut sandbox,
            "max_connections_per_ip",
            "POST <base>rpc while the SSE stream holds the other connection",
            &[200],
            &answered,
        );
        if !String::from_utf8_lossy(&answered.body).contains("pong") {
            regression_failure(
                &mut sandbox,
                "default_timeout",
                "POST <base>rpc tools/call status (plugin round trip)",
                "a result containing pong",
                &answered.describe(),
            );
        }
        let event = sse_next_event(&mut sse, "pong-event", Duration::from_secs(5)).unwrap_or_else(
            |error| {
                regression_failure(
                    &mut sandbox,
                    "max_subscribers_per_plugin",
                    "GET <base>events (SSE) after one tools/call",
                    "a notifications/message event",
                    &error,
                )
            },
        );
        if !event.contains("notifications/message") {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "GET <base>events (SSE)",
                "a notifications/message event",
                &event,
            );
        }
        let last_id = sse_id(&event).unwrap_or_else(|| {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "GET <base>events (SSE)",
                "an id line on the event",
                &event,
            )
        });
        drop(sse);

        let later = br#"{"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"status","arguments":{}}}"#;
        let published = exchange_admitted(
            &mut sandbox,
            &address,
            &request_head(
                &address,
                "POST",
                &rpc_path,
                "application/json",
                later.len(),
                &[],
            ),
            later,
        );
        expect_status(
            &mut sandbox,
            "max_connections_per_ip",
            "POST <base>rpc with no SSE subscriber attached",
            &[200],
            &published,
        );

        let mut replay = sse_open_admitted(
            &mut sandbox,
            "max_subscribers_per_plugin",
            &address,
            &events_path,
            Some(last_id),
        );
        let replayed = sse_next_event(&mut replay, "pong-event", Duration::from_secs(5))
            .unwrap_or_else(|error| {
                regression_failure(
                    &mut sandbox,
                    "max_subscribers_per_plugin",
                    "GET <base>events with Last-Event-ID",
                    "the replayed event within 5 s",
                    &error,
                )
            });
        let replayed_id = sse_id(&replayed).unwrap_or_else(|| {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "GET <base>events with Last-Event-ID",
                "an id line on the replayed event",
                &replayed,
            )
        });
        if replayed_id <= last_id {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "GET <base>events with Last-Event-ID",
                &format!("a replayed event after id {last_id}"),
                &format!("replayed id {replayed_id}: {replayed}"),
            );
        }
        drop(replay);
    } else {
        // The floor disables UI subscribers outright: 0 is a feature switch,
        // and both stream surfaces must say so instead of dropping the socket.
        let upgrade = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            ws_path,
            address,
            ws_key()
        );
        // Both surfaces answer a rejection for the subscriber limit, so the
        // probe must reach the handler: an admission refusal is retried.
        let handshake = exchange_admitted(&mut sandbox, &address, &upgrade, b"");
        expect_status(
            &mut sandbox,
            "max_subscribers_per_plugin",
            "GET <base>ws (Upgrade: websocket, zero subscribers allowed)",
            &[429],
            &handshake,
        );
        let events = exchange_admitted(
            &mut sandbox,
            &address,
            &request_head(
                &address,
                "GET",
                &events_path,
                "",
                0,
                &[("Accept", "text/event-stream")],
            ),
            b"",
        );
        expect_status(
            &mut sandbox,
            "max_subscribers_per_plugin",
            "GET <base>events (SSE, zero subscribers allowed)",
            &[429],
            &events,
        );
        expect_error_code(
            &mut sandbox,
            "max_subscribers_per_plugin",
            "GET <base>events (SSE, zero subscribers allowed)",
            "too_many_subscribers",
            &events,
        );
    }

    assert_daemon_alive(&mut sandbox, phase);
    let port_after =
        fs::read_to_string(sandbox.data.join("run").join("http.port")).expect("http.port");
    if port_after != port_before {
        regression_failure(
            &mut sandbox,
            "liveness",
            &format!("http.port of the {phase} config"),
            &format!("{port_before:?} (the daemon must not restart)"),
            &port_after,
        );
    }
}

/// Open one streaming `tools/call` and return the connection. The caller reads
/// the response head first, so a 503 keeps its status without waiting for SSE.
pub(crate) fn mcp_stream_request(address: &str, id: u64, name: &str) -> TcpStream {
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "progressToken": "t1"
            }
        }
    }))
    .expect("body");
    let mut stream = TcpStream::connect(address).expect("mcp connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        TEST_PASSWORD,
        name,
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write head");
    stream.write_all(&body).expect("write body");
    stream
}

/// A `max_streams = 1` sandbox with the event plugin installed and started.
pub(crate) fn mcp_disconnect_sandbox() -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("max_streams = 1\n");
    let key = SigningKey::from_bytes(&[62u8; 32]);
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
    set_password(&sandbox, TEST_PASSWORD);
    (sandbox, address)
}

/// Open a hung `tools/call`, leave (optionally after trailing bytes), and
/// require the long-lived slot back before the plugin call can finish.
pub(crate) fn assert_mcp_disconnect_releases(address: &str, id: u64, trailing: Option<u8>) {
    // The plugin sleeps for five seconds, so the stream is still open when we
    // drop the client after its head.
    let mut slow = mcp_stream_request(address, id, "demo.slow");
    assert_eq!(read_response_head(&mut slow).0, 200);
    if let Some(byte) = trailing {
        slow.write_all(&[byte]).expect("trailing byte");
    }
    let dropped_at = Instant::now();
    drop(slow);

    let mut released = false;
    // The plugin sleeps for five seconds; a slot released only when that call
    // ends would still be busy here.
    for _ in 0..200 {
        let mut candidate = mcp_stream_request(address, id + 1, "demo.status");
        let (status, _) = read_response_head(&mut candidate);
        if status == 200 {
            released = true;
            drop(candidate);
            break;
        }
        assert_eq!(status, 503, "only the busy status is retried");
        drop(candidate);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        released,
        "a disconnected MCP client must release the max_streams slot"
    );
    assert!(
        dropped_at.elapsed() < Duration::from_secs(2),
        "the slot must be released when the client leaves, not when the hung \
         plugin call finally returns (took {:?})",
        dropped_at.elapsed()
    );
}

pub(crate) const MIN_CONFIG_PASSWORD: &str = "min-config-password";

/// Every runtime limit svcd.conf accepts, at the parser's raw minimum.
pub(crate) const MINIMAL_LIMITS: &[(&str, &str)] = &[
    ("request_stack_bytes", "1"),
    ("io_stack_bytes", "1"),
    ("blocking_threads", "0"),
    ("max_body_bytes", "1"),
    ("max_json_body_bytes", "1"),
    ("max_complete_body_bytes", "1"),
    ("max_header_bytes", "1"),
    ("max_streams", "0"),
    ("stream_buffer_bytes", "1"),
    ("max_connections", "1"),
    ("max_connections_per_ip", "1"),
    ("max_subscriptions", "0"),
    ("max_subscribers_per_plugin", "0"),
    ("http_idle_timeout_secs", "0"),
    ("log_max_bytes", "1"),
    ("log_rotations", "0"),
    ("max_package_bytes", "1"),
    ("transfer_timeout_secs", "0"),
    ("default_timeout", "0"),
    ("max_timeout", "0"),
    ("ws_max_message_bytes", "1"),
    ("ws_inflight_bytes", "1"),
    ("ws_inflight_requests", "0"),
    ("ws_buffer_shrink_bytes", "1"),
    ("ws_raw_max_bytes", "1"),
    ("ws_raw_queue", "0"),
    ("pending_queue", "0"),
    ("progress_queue", "0"),
];

pub(crate) const NO_OVERRIDES: &[(&str, &str)] = &[];

pub(crate) const USABLE_OVERRIDES: &[(&str, &str)] = &[
    ("max_connections", "2"),
    ("max_connections_per_ip", "2"),
    ("max_subscriptions", "1"),
    ("max_subscribers_per_plugin", "1"),
    ("default_timeout", "1000"),
];

pub(crate) fn override_for<'a>(overrides: &'a [(&str, &str)], key: &str) -> Option<&'a str> {
    overrides
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| *value)
}

pub(crate) fn minimal_conf(overrides: &[(&str, &str)]) -> String {
    let mut conf =
        String::from("# #108: every limit at its raw minimum; a live daemon must survive it.\n");
    for &(key, floor) in MINIMAL_LIMITS {
        let value = override_for(overrides, key).unwrap_or(floor);
        conf.push_str(&format!("{} = {}\n", key, value));
    }
    conf
}
