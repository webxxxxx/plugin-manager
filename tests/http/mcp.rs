//! mcp http tests.

use super::*;

#[test]
fn http_mcp_streams_progress_for_tools_call() {
    let (sandbox, address) = daemon_with_web_event_plugin(61);
    set_password(&sandbox, TEST_PASSWORD);

    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "demo.status",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "progressToken": "t1"
            }
        }
    }))
    .expect("body");

    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: demo.status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        TEST_PASSWORD,
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write head");
    stream.write_all(&body).expect("write body");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");

    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("headers");
    let head = String::from_utf8_lossy(&response[..split]);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "{head}"
    );
    let text = String::from_utf8_lossy(&decode_chunked(&response[split + 4..])).to_string();
    let progress = text
        .find("notifications/progress")
        .expect("a progress event before the result");
    let final_at = text.find("\"id\":7").expect("the final response");
    assert!(
        progress < final_at,
        "progress must arrive before the final result: {text}"
    );
    assert!(text.contains("\"progressToken\":\"t1\""), "{text}");
    assert!(text.contains("pong"), "{text}");
}

/// A streaming MCP call is a long-lived stream: it draws from the same
/// `max_streams` budget as downloads, and a full budget refuses it with the
/// stable 503 before the plugin call starts.
#[test]
fn http_mcp_stream_consumes_the_long_lived_max_streams_budget() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("max_streams = 1\n");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    set_password(&sandbox, TEST_PASSWORD);

    // Hold the only long-lived slot with a stalled download.
    let content = large_download_body();
    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        &content,
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let id = json_body(&put)["id"].as_str().expect("blob id").to_string();
    let mut stalled = TcpStream::connect(&address).expect("stalled connect");
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    stalled
        .write_all(
            format!(
                "GET /api/v1/blobs/{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                id, address
            )
            .as_bytes(),
        )
        .expect("stalled request");
    assert_eq!(read_response_head(&mut stalled).0, 200);

    let mut busy = mcp_stream_request(&address, 8, "demo.status");
    let (status, headers) = read_response_head(&mut busy);
    assert_eq!(status, 503, "a full stream budget refuses MCP SSE");
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .map(|(_, value)| value.as_str()),
        Some("1"),
        "the busy response keeps the stable Retry-After contract"
    );
    drop(busy);

    // Releasing the stalled reader releases the slot for the same call.
    drop(stalled);
    let mut accepted = None;
    for _ in 0..200 {
        let mut candidate = mcp_stream_request(&address, 9, "demo.status");
        let (status, headers) = read_response_head(&mut candidate);
        if status == 200 {
            accepted = Some((candidate, headers));
            break;
        }
        assert_eq!(status, 503, "only the busy status is retried");
        drop(candidate);
        std::thread::sleep(Duration::from_millis(10));
    }
    let (mut stream, headers) = accepted.expect("the MCP stream fits after the release");
    assert!(
        headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("content-type") && value.contains("text/event-stream")
        }),
        "{headers:?}"
    );
    let mut body = Vec::new();
    stream.read_to_end(&mut body).expect("read the SSE body");
    let text = String::from_utf8_lossy(&decode_chunked(&body)).to_string();
    assert!(
        text.contains("\"id\":9"),
        "the stream ends with a result: {text}"
    );
}

/// A disconnected streaming MCP call must release its long-lived slot: with
/// `max_streams = 1`, a second streaming call can start as soon as the first
/// client goes away, even while the plugin call is still running. Both engines
/// own the slot: the Unix loop cancels the reactor call, and the Windows
/// fallback peeks the client socket and abandons the control read (#135).
#[test]
fn http_mcp_stream_disconnect_releases_the_budget() {
    let (_sandbox, address) = mcp_disconnect_sandbox();
    assert_mcp_disconnect_releases(&address, 11, None);
}

/// The same disconnect, but with a byte sent after the request body. The
/// Windows probe must drain bytes a `Connection: close` response can never
/// serve, or a FIN queued behind them would keep the slot until the hung call
/// ends.
#[cfg(windows)]
#[test]
fn http_mcp_stream_disconnect_with_trailing_bytes_releases_the_budget() {
    let (_sandbox, address) = mcp_disconnect_sandbox();
    assert_mcp_disconnect_releases(&address, 11, Some(b'G'));
}

/// When the management channel cannot be reached, the MCP stream still starts
/// with 200 + an SSE -32603, matching the old reactor's wire behaviour.
#[cfg(unix)]
#[test]
fn http_mcp_stream_reports_an_unreachable_channel_as_an_sse_error() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    set_password(&sandbox, TEST_PASSWORD);
    fs::remove_file(sandbox.data.join("run").join("svcd.sock")).expect("remove control socket");

    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "tools/call",
        "params": {
            "name": "demo.status",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "progressToken": "t1"
            }
        }
    }))
    .expect("body");
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: demo.status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        TEST_PASSWORD,
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write head");
    stream.write_all(&body).expect("write body");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("headers");
    let head = String::from_utf8_lossy(&response[..split]);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "{head}"
    );
    let text = String::from_utf8_lossy(&decode_chunked(&response[split + 4..])).to_string();
    assert!(text.contains("\"code\":-32603"), "{text}");
    assert!(text.contains("svcd unreachable"), "{text}");

    // The control socket is gone, so stop the daemon directly; Drop would
    // otherwise wait for the unreachable shutdown RPC.
    if let Some(mut child) = sandbox.daemon.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}
