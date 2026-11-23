//! ui http tests.

use super::*;

#[test]
fn serves_the_embedded_console() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let page = raw_request(&address, "GET", "/", "", b"");
    assert_eq!(page.status, 200);
    assert_eq!(
        raw_header_values(&page, "content-type"),
        vec!["text/html; charset=utf-8"]
    );
    let html = String::from_utf8_lossy(&page.body);
    assert!(html.contains("id=\"rows\""), "console table missing");
    assert!(
        !html.contains("允许安装未签名包"),
        "unsigned install is controlled by svcd.conf, not a console checkbox"
    );
    assert!(
        html.contains("/api/v1/services"),
        "console does not call the management API"
    );
    // The trust upload moved into the ⋮ menu after 退出; there is no direct
    // toolbar button any more.
    assert!(
        !html.contains("上传公钥"),
        "the trust upload must stay out of the toolbar"
    );
    assert!(
        html.contains("id=\"morebtn\"") && html.contains("aria-haspopup=\"menu\""),
        "console missing the overflow menu"
    );
    for needle in [
        "上传安装",
        "添加公钥",
        "查看日志",
        "设置",
        "检查更新",
        "升级并重启",
        "保存",
        "正在重启",
        "清空",
        "确认清空？",
        "自动刷新（3s）",
        "hasConfig",
        "hasUI",
        "打开",
        "安装包",
        "导出备份",
        ".spkb",
        "purge=true",
        "/spk",
    ] {
        assert!(html.contains(needle), "console missing {}", needle);
    }
    // The settings sheet keeps its actions outside the scrolling body, and the
    // English key plus the docs live in the hover tip, not in the row.
    assert!(
        html.contains("panel sheet") && html.contains("sheet-foot"),
        "settings dialog must keep a fixed footer"
    );
    assert!(
        html.contains("id=\"tip\"") && html.contains("entry.label || entry.key"),
        "settings dialog shows Chinese labels and the English key in a hover tip"
    );
    let index = raw_request(&address, "GET", "/index.html", "", b"");
    assert_eq!(index.status, 200);

    let post = raw_request(&address, "POST", "/", "", b"");
    assert_eq!(post.status, 405);
    assert_eq!(raw_header_values(&post, "allow"), vec!["GET"]);
}

#[test]
fn plugin_web_ui_serves_assets_with_base_and_etag() {
    let (_sandbox, address) = web_ui_sandbox(41);
    let key = SigningKey::from_bytes(&[41u8; 32]);
    let plain = web_plugin_bundle(&key, "plain", false);
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &plain,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    assert_eq!(installed.body["id"], "plain");

    let services = request(&address, "GET", "/api/v1/services", "", b"");
    let summaries = services.body["services"].as_array().expect("services");
    let demo = summaries
        .iter()
        .find(|service| service["id"] == "demo")
        .expect("demo");
    let plain = summaries
        .iter()
        .find(|service| service["id"] == "plain")
        .expect("plain");
    assert_eq!(demo["hasUI"], true);
    assert_eq!(plain["hasUI"], false);

    let base = plugin_ui_base(&address, "demo");

    let page = raw_request(&address, "GET", &base, "", b"");
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));
    assert_eq!(
        raw_header_values(&page, "content-type"),
        vec!["text/html; charset=utf-8"]
    );
    assert_eq!(raw_header_values(&page, "cache-control"), vec!["no-cache"]);
    assert_eq!(raw_header_values(&page, "vary"), vec!["Accept"]);
    assert_eq!(
        raw_header_values(&page, "x-content-type-options"),
        vec!["nosniff"]
    );
    let html = String::from_utf8_lossy(&page.body);
    assert!(
        html.starts_with(&format!("<!doctype html>\n<base href=\"{}\">\n", base)),
        "{html}"
    );
    assert!(html.contains("plugin page"), "{html}");

    let etags = raw_header_values(&page, "etag");
    assert_eq!(etags.len(), 1, "{:?}", page.headers);
    let etag = etags[0].to_string();
    let revalidated = raw_request_with(
        &address,
        "GET",
        &base,
        "",
        b"",
        &[("If-None-Match", etag.as_str())],
    );
    assert_eq!(revalidated.status, 304);
    assert!(revalidated.body.is_empty());
    assert_eq!(raw_header_values(&revalidated, "etag"), vec![etag.as_str()]);

    let script = raw_request(&address, "GET", &format!("{}app.js", base), "", b"");
    assert_eq!(script.status, 200);
    assert_eq!(
        raw_header_values(&script, "content-type"),
        vec!["text/javascript"]
    );
    assert_eq!(script.body, b"export const answer = 42;\n");

    // A page larger than the stream buffer is chunked through the bounded
    // output buffer; the response still arrives complete and with the base tag.
    let large = raw_request(&address, "GET", &format!("{}large.html", base), "", b"");
    assert_eq!(large.status, 200);
    assert!(large.body.len() > 20_000, "{}", large.body.len());
    let large_html = String::from_utf8_lossy(&large.body);
    assert!(
        large_html.starts_with(&format!("<!doctype html>\n<base href=\"{}\">\n", base)),
        "{}",
        &large_html[..large_html.len().min(120)]
    );
    assert!(large_html.ends_with("</body></html>\n"));

    // The canonical URL has a trailing slash so relative assets resolve.
    let no_slash = raw_request(&address, "GET", base.trim_end_matches('/'), "", b"");
    assert_eq!(no_slash.status, 308);
    assert_eq!(
        raw_header_values(&no_slash, "location"),
        vec![base.as_str()]
    );

    // SPA fallback only for HTML navigations; a missing asset stays a 404.
    let missing_asset = raw_request(&address, "GET", &format!("{}missing.js", base), "", b"");
    assert_eq!(missing_asset.status, 404);
    let spa = raw_request_with(
        &address,
        "GET",
        &format!("{}settings/tunnel", base),
        "",
        b"",
        &[("Accept", "text/html")],
    );
    assert_eq!(spa.status, 200);
    assert!(String::from_utf8_lossy(&spa.body).contains("plugin page"));

    for path in ["/plugin-ui/Demo/", "/plugin-ui/missing/"] {
        let denied = raw_request(&address, "GET", path, "", b"");
        assert_eq!(denied.status, 404, "{path}");
        // A rejected path is not a client-side route: an HTML navigation must
        // not turn traversal into the SPA entry.
        let denied_html =
            raw_request_with(&address, "GET", path, "", b"", &[("Accept", "text/html")]);
        assert_eq!(denied_html.status, 404, "{path} with Accept: text/html");
    }

    // A missing token is refused even for an installed UI.
    let no_token = raw_request(&address, "GET", "/plugin-ui/demo/", "", b"");
    assert_eq!(no_token.status, 401);

    // A plugin without manifest.ui has no route even if payload/web exists.
    let no_ui = raw_request(&address, "GET", "/plugin-ui/plain/", "", b"");
    assert_eq!(no_ui.status, 404);

    let post = raw_request(&address, "POST", &base, "", b"");
    assert_eq!(post.status, 405);
    assert_eq!(raw_header_values(&post, "allow"), vec!["GET"]);
}

#[test]
fn a_slow_realtime_client_does_not_block_other_requests() {
    let (_sandbox, address) = daemon_with_web_event_plugin(60);
    let base = plugin_ui_base(&address, "demo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}ws", base), &[]);
    assert_eq!(status, 101, "{head}");

    // A 512 KiB binary message is echoed back. The test never reads the echo,
    // so the server's socket buffer fills and this connection is backpressured.
    ws_send(&mut ws, 0x2, &vec![0x41; 512 * 1024]);
    std::thread::sleep(Duration::from_millis(100));

    // Other connections must still be served while that reader is stalled.
    for _ in 0..4 {
        let response = request(&address, "GET", "/api/v1/version", "", b"");
        assert_eq!(response.status, 200, "{}", response.body);
    }
}

#[test]
fn plugin_ui_realtime_streams_consume_the_long_lived_stream_cap() {
    // One SSE stream fills max_streams. A second long-lived stream is refused
    // before its headers; a one-shot asset is still exempt; closing the stream
    // releases the slot.
    let (_sandbox, address) = web_ui_sandbox_with_conf(58, "max_streams = 1\n");
    let base = plugin_ui_base(&address, "demo");
    let events = format!("{}events", base);
    let ws = format!("{}ws", base);

    let first = sse_connect(&address, &events);
    let second = raw_request(&address, "GET", &events, "", b"");
    assert_eq!(
        second.status,
        503,
        "{}",
        String::from_utf8_lossy(&second.body)
    );
    assert!(String::from_utf8_lossy(&second.body).contains("busy"));
    assert_eq!(raw_header_values(&second, "retry-after"), vec!["1"]);

    let (_stream, status, head) = ws_handshake(&address, &ws, &[]);
    assert_eq!(status, 503, "{head}");

    // A page load is many one-shot responses; it must not trip the cap.
    let page = raw_request(&address, "GET", &base, "", b"");
    assert_eq!(page.status, 200);

    drop(first);
    let mut released = false;
    for _ in 0..100 {
        // A successful SSE response never ends, so probe the head instead of
        // reading to EOF.
        let (status, stream) = sse_probe(&address, &events);
        drop(stream);
        if status == 200 {
            released = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        released,
        "closing the SSE stream must release its max_streams slot"
    );
}

#[test]
fn plugin_ui_assets_do_not_consume_the_long_lived_stream_cap() {
    // One long-lived stream is the whole cap; a browser page still loads all
    // of its assets because one-shot responses are exempt from max_streams.
    let (_sandbox, address) = web_ui_sandbox_with_conf(57, "max_streams = 1\n");
    let base = plugin_ui_base(&address, "demo");

    let mut handles = Vec::new();
    for index in 0..16 {
        let address = address.clone();
        let base = base.clone();
        handles.push(std::thread::spawn(move || {
            let path = match index % 3 {
                0 => base.clone(),
                1 => format!("{}app.js", base),
                _ => format!("{}large.html", base),
            };
            let response = raw_request(&address, "GET", &path, "", b"");
            assert_eq!(response.status, 200, "{path}");
            assert!(!response.body.is_empty(), "{path} returned no body");
        }));
    }
    for handle in handles {
        handle.join().expect("asset request thread");
    }

    // The cap did not leak after the burst.
    let page = raw_request(&address, "GET", &base, "", b"");
    assert_eq!(page.status, 200);
}

#[test]
fn plugin_web_ui_rpc_bridges_tools_call() {
    let (_sandbox, address) = web_ui_sandbox(42);
    let base = plugin_ui_base(&address, "demo");

    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);

    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &rpc_body(7, "status"),
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(call.body["jsonrpc"], "2.0");
    assert_eq!(call.body["id"], 7);
    assert_eq!(call.body["result"]["content"][0]["text"], "pong");
    assert_eq!(call.body["result"]["isError"], false);

    // A fully qualified capability name is accepted too.
    let prefixed = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &rpc_body(8, "demo.status"),
    );
    assert_eq!(prefixed.body["id"], 8);
    assert_eq!(prefixed.body["result"]["content"][0]["text"], "pong");

    // Same-origin Origin is accepted; a foreign one is rejected.
    let origin = format!("http://{}", address);
    let body = rpc_body(9, "status");
    let same_origin = request_with(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &body,
        &[("Origin", origin.as_str())],
    );
    assert_eq!(same_origin.status, 200, "{}", same_origin.body);
    let cross_origin = request_with(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &body,
        &[("Origin", "http://evil.example")],
    );
    assert_eq!(cross_origin.status, 403);

    let unsupported = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        br#"{"jsonrpc":"2.0","id":10,"method":"svcd.ping"}"#,
    );
    assert_eq!(unsupported.status, 200);
    assert_eq!(unsupported.body["id"], 10);
    assert_eq!(unsupported.body["error"]["code"], -32601);

    let malformed = raw_request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        b"{",
    );
    assert_eq!(malformed.status, 200);
    assert_eq!(json_body(&malformed)["error"]["code"], -32700);

    let unknown = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &rpc_body(11, "nope"),
    );
    assert_eq!(unknown.body["id"], 11);
    assert_eq!(unknown.body["error"]["code"], -32602);

    let get = raw_request(&address, "GET", &format!("{}rpc", base), "", b"");
    assert_eq!(get.status, 405);
    assert_eq!(raw_header_values(&get, "allow"), vec!["POST"]);

    // A stopped plugin is a tool error inside a successful JSON-RPC envelope.
    let stopped = request(&address, "POST", "/api/v1/services/demo/stop", "", b"");
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &rpc_body(12, "status"),
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(call.body["id"], 12);
    assert_eq!(call.body["result"]["isError"], true);
    assert!(
        call.body["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("not running"),
        "{}",
        call.body
    );
}

#[test]
fn plugin_ui_sse_streams_plugin_notifications() {
    let (_sandbox, address) = daemon_with_web_event_plugin(51);
    let base = plugin_ui_base(&address, "demo");
    let mut sse = sse_connect(&address, &format!("{}events", base));
    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(call.body["result"]["content"][0]["text"], "pong");
    let event = sse_wait_for_data(&mut sse, "pong-event");
    assert!(
        event.contains("\"method\":\"notifications/message\""),
        "{event}"
    );
    assert!(event.contains("id: "), "{event}");
}

#[test]
fn plugin_ui_websocket_bridges_calls_notifications_and_control_frames() {
    let (_sandbox, address) = daemon_with_web_event_plugin(52);
    let base = plugin_ui_base(&address, "demo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}ws", base), &[]);
    assert_eq!(status, 101, "{head}");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": { "name": "status", "arguments": {} }
    });
    ws_send(&mut ws, 0x1, request.to_string().as_bytes());

    let mut notification = false;
    let mut response = false;
    for _ in 0..6 {
        let (opcode, payload) = ws_read(&mut ws);
        if opcode != 0x1 {
            continue;
        }
        let value: Value = serde_json::from_slice(&payload).expect("ws json");
        if value["method"] == "notifications/message" {
            notification = true;
        }
        if value["id"] == 9 && value["result"]["content"][0]["text"] == "pong" {
            response = true;
        }
        if notification && response {
            break;
        }
    }
    assert!(notification, "notification frame missing");
    assert!(response, "response frame missing");

    ws_send(&mut ws, 0x9, b"ping");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0xA);
    assert_eq!(payload, b"ping");

    let (mut binary, status, head) = ws_handshake(&address, &format!("{}ws", base), &[]);
    assert_eq!(status, 101, "{head}");
    ws_send(&mut binary, 0x2, b"binary");
    let (opcode, payload) = ws_read(&mut binary);
    assert_eq!(opcode, 0x2);
    assert_eq!(payload, b"binary");
}

/// The `/raw` route only exists for protocol-3 plugins: a JSON-RPC plugin has
/// no raw transport, and a platform without a plugin-stdio reactor says so
/// instead of returning a misleading 404.
#[test]
fn plugin_ui_raw_requires_a_raw_capable_plugin() {
    let (_sandbox, address) = web_ui_sandbox(63);
    let base = plugin_ui_base(&address, "demo");
    let (mut stream, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    let mut body = Vec::new();
    let _ = stream.read_to_end(&mut body);
    let body = String::from_utf8_lossy(&body).to_ascii_lowercase();
    assert_eq!(status, 409, "{head}");
    assert!(body.contains("raw_unavailable"), "{head}{body}");
    assert!(body.contains("protocol 3"), "{head}{body}");
}

#[cfg(unix)]
#[test]
fn plugin_ui_raw_round_trips_binary_frames() {
    let (_sandbox, address) = daemon_with_raw_plugin(61);
    let base = plugin_ui_base(&address, "echo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    assert_eq!(status, 101, "{head}");

    // One binary frame crosses the plugin link byte for byte.
    ws_send(&mut ws, 0x2, b"raw-bytes\x00\xff");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x2, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(payload, b"raw-bytes\x00\xff");

    // A fragmented message is reassembled before it reaches the plugin.
    ws_send_with(&mut ws, 0x2, b"part-one/", false);
    ws_send_with(&mut ws, 0x0, b"part-two", true);
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x2, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(payload, b"part-one/part-two");

    // A zero-length frame is a valid frame, not a disconnect.
    ws_send(&mut ws, 0x2, b"");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x2);
    assert!(payload.is_empty());

    // The transport is binary-only.
    ws_send(&mut ws, 0x1, b"text");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x8, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1003);
}

#[cfg(unix)]
#[test]
fn plugin_ui_raw_closes_on_an_oversized_message() {
    let (_sandbox, address) = daemon_with_raw_plugin_with(62, "ws_raw_max_bytes = 1024\n");
    let base = plugin_ui_base(&address, "echo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    assert_eq!(status, 101, "{head}");
    ws_send(&mut ws, 0x2, &vec![0x41; 2048]);
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x8, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1009);
}

#[cfg(unix)]
#[test]
fn plugin_ui_raw_reports_protocol_errors_with_a_close_frame() {
    let (_sandbox, address) = daemon_with_raw_plugin(64);
    let base = plugin_ui_base(&address, "echo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    assert_eq!(status, 101, "{head}");

    // A frame with reserved bits set is a protocol violation. The client must
    // see the 1002 close, not a bare EOF from the daemon dropping the socket.
    let mut frame = vec![0xF2, 0x80];
    frame.extend_from_slice(&[1, 2, 3, 4]);
    ws.write_all(&frame).expect("reserved-bit frame");
    ws.flush().expect("flush");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x8, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1002);
}

#[test]
fn plugin_ui_sse_replays_after_last_event_id() {
    let (_sandbox, address) = daemon_with_web_event_plugin(54);
    let base = plugin_ui_base(&address, "demo");
    let mut first = sse_connect(&address, &format!("{}events", base));
    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        br#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    );
    assert_eq!(call.status, 200, "{}", call.body);
    let event = sse_wait_for_data(&mut first, "pong-event");
    let last_id = sse_id(&event).expect("event id");
    drop(first);

    // No SSE subscriber is attached now, so this event survives only in the
    // replay ring until the next connection asks for it.
    let second = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        br#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    );
    assert_eq!(second.status, 200, "{}", second.body);

    let mut replay = sse_connect_with_last_id(&address, &format!("{}events", base), Some(last_id));
    let replayed = sse_wait_for_data(&mut replay, "pong-event");
    let replayed_id = sse_id(&replayed).expect("replayed event id");
    assert!(
        replayed_id > last_id,
        "expected an event after {last_id}, got {replayed_id}: {replayed}"
    );
}

#[test]
fn plugin_ui_websocket_rejects_oversized_messages_with_1009() {
    let (_sandbox, address) = daemon_with_web_event_plugin(55);
    let base = plugin_ui_base(&address, "demo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}ws", base), &[]);
    assert_eq!(status, 101, "{head}");
    ws_send(&mut ws, 0x1, &vec![b'x'; 1024 * 1024 + 1]);
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x8);
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1009);
}

/// #265: one protocol-3 plugin serves JSON-RPC (stream 0) and raw (stream >= 1)
/// at the same time. The ready ping, a capability call, its notification and a
/// `/raw` echo all cross the same pipes.
#[test]
fn protocol_three_plugin_serves_json_and_raw_together() {
    let (_sandbox, address) = daemon_with_raw_json_plugin(64, "rawjson");
    let base = plugin_ui_base(&address, "rawjson");

    let mut sse = sse_connect(&address, &format!("{}events", base));
    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo","arguments":{}}}"#,
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(
        call.body["result"]["content"][0]["text"], "pong",
        "{}",
        call.body
    );

    let event = sse_wait_for_data(&mut sse, "panel-event");
    assert!(
        event.contains("\"method\":\"notifications/message\""),
        "{event}"
    );

    // The same instance still echoes raw frames on its own stream.
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    assert_eq!(status, 101, "{head}");
    ws_send(&mut ws, 0x2, b"raw-bytes");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x2);
    assert_eq!(payload, b"raw-bytes");
}

/// #266: a plugin can end a raw stream, and it hears about a subscriber that
/// left instead of writing into a dead stream.
#[test]
fn a_plugin_ends_a_raw_stream_and_hears_about_a_subscriber_leaving() {
    let (_sandbox, address) = daemon_with_raw_json_plugin(66, "rawjson");
    let base = plugin_ui_base(&address, "rawjson");
    let mut sse = sse_connect(&address, &format!("{}events", base));

    // Plugin -> subscriber: the close frame becomes a clean WebSocket close.
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    assert_eq!(status, 101, "{head}");
    ws_send(&mut ws, 0x2, b"close-me");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x2);
    assert_eq!(payload, b"close-me");
    let (opcode, _) = ws_read(&mut ws);
    assert_eq!(opcode, 0x8, "the plugin's close must reach the WebSocket");

    // Subscriber -> plugin: dropping the socket makes the hub tell the plugin.
    {
        let (_second, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
        assert_eq!(status, 101, "{head}");
    }
    let event = sse_wait_for_data(&mut sse, "stream-closed");
    assert!(
        event.contains("\"method\":\"notifications/message\""),
        "{event}"
    );
}

/// #260: a protocol-2 plugin (length-prefixed JSON) installs and answers a call.
#[test]
fn a_protocol_two_plugin_installs_and_calls() {
    let (_sandbox, address) = daemon_with_protocol_two_plugin(65, "ptwo");
    let base = plugin_ui_base(&address, "ptwo");

    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo","arguments":{}}}"#,
    );

    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(
        call.body["result"]["content"][0]["text"], "pong",
        "{}",
        call.body
    );
}

/// #265: a non-JSON frame on stream 0 is a protocol violation; the instance is
/// stopped (and the fixture, which misbehaves once, can start again).
#[test]
fn a_non_json_stream_zero_frame_stops_the_instance() {
    let (sandbox, address) = install_raw_json_plugin(67, "garbled", true);
    let started = request(&address, "POST", "/api/v1/services/garbled/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    let base = plugin_ui_base(&address, "garbled");

    // The ready ping is answered properly (the fixture only misbehaves on a
    // call); the call gets a non-JSON frame and must take the instance down.
    let _ = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo","arguments":{}}}"#,
    );

    let log = sandbox.logs.join("garbled.log");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut text = String::new();
    while Instant::now() < deadline {
        text = fs::read_to_string(&log).unwrap_or_default();
        if text.contains("protocol violation") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        text.contains("protocol violation"),
        "the violation was not logged: {text}"
    );

    // The fixture only answers garbage once: a second start proves the first
    // instance was stopped, not merely left waiting.
    let restarted = request(&address, "POST", "/api/v1/services/garbled/start", "", b"");
    assert_eq!(restarted.status, 200, "{}", restarted.body);
    let status = request(&address, "GET", "/api/v1/services/garbled", "", b"");
    assert_eq!(
        status.body["summary"]["state"], "running",
        "{}",
        status.body
    );
}
