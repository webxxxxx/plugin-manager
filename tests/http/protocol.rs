//! protocol http tests.

use super::*;

/// A server-level 401 keeps GET's Content-Length on HEAD and writes no body.
/// If the body leaked, the next response on the keep-alive connection would
/// start with JSON instead of a status line.
#[test]
fn head_unauthorized_has_no_body_and_the_connection_stays_parseable() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    // enable_auth writes the password through the control socket, so the
    // daemon must be ready before it runs.
    sandbox.wait_for_http();
    let (address, password) = enable_auth(&mut sandbox);

    let get_denied = raw_request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(get_denied.status, 401);
    assert!(!get_denied.body.is_empty(), "GET 401 keeps its JSON body");
    let get_length = header_value(&get_denied.headers, "content-length")
        .expect("GET 401 advertises a body")
        .to_string();

    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let head_request = format!(
        "HEAD /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    stream
        .write_all(head_request.as_bytes())
        .expect("write HEAD");

    let head = read_response_head_raw(&mut stream);
    let (status, headers) = parse_response_head(&head);
    assert_eq!(status, 401, "{}", String::from_utf8_lossy(&head));
    assert_eq!(
        header_value(&headers, "content-length"),
        Some(get_length.as_str()),
        "HEAD 401 keeps the GET Content-Length"
    );
    assert_eq!(
        header_value(&headers, "connection"),
        Some("keep-alive"),
        "{}",
        String::from_utf8_lossy(&head)
    );

    // The connection must still parse: a HEAD body would land here first.
    let next = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        address, password
    );
    stream
        .write_all(next.as_bytes())
        .expect("write next request");
    let next_head = read_response_head_raw(&mut stream);
    assert!(
        next_head.starts_with(b"HTTP/1.1 200"),
        "HEAD 401 left body bytes on the connection: {}",
        String::from_utf8_lossy(&next_head)
    );
    // The GET after the HEAD keeps its own body: the per-request flag must not
    // leak into the next response.
    let (_, next_headers) = parse_response_head(&next_head);
    let next_length: usize = header_value(&next_headers, "content-length")
        .expect("GET 200 advertises its body")
        .parse()
        .expect("content-length is a number");
    let mut next_body = vec![0u8; next_length];
    stream.read_exact(&mut next_body).expect("GET 200 body");
    let next_json: Value = serde_json::from_slice(&next_body).expect("GET 200 JSON");
    assert!(
        next_json["services"].is_array(),
        "GET 200 body after a HEAD: {next_json}"
    );
}

/// A HEAD request refused for an oversized declared body gets the same 413 as
/// GET, with the same Content-Length and no body.
#[test]
fn head_payload_too_large_has_no_body() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let get_request = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        512u64 * 1024 * 1024
    );
    let get_refused = send_request_head(&sandbox, &address, &get_request);
    assert_eq!(get_refused.status, 413, "{:?}", get_refused.body);
    let get_length = header_value(&get_refused.headers, "content-length")
        .expect("GET 413 advertises a body")
        .to_string();
    assert!(!get_refused.body.is_empty(), "GET 413 keeps its JSON body");

    let head_request = format!(
        "HEAD /api/v1/services HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        512u64 * 1024 * 1024
    );
    let head_refused = send_request_head(&sandbox, &address, &head_request);
    assert_eq!(head_refused.status, 413);
    assert!(
        head_refused.body.is_empty(),
        "HEAD 413 carried a body: {}",
        String::from_utf8_lossy(&head_refused.body)
    );
    assert_eq!(
        header_value(&head_refused.headers, "content-length"),
        Some(get_length.as_str()),
        "HEAD 413 keeps the GET Content-Length"
    );
}

/// An ambiguous request framing is rejected before the body policy resolves.
/// The parsed method is still enough to keep a HEAD 400 body-less.
#[test]
fn head_bad_framing_has_no_body() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let get_request = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        address
    );
    let get_refused = send_request_head(&sandbox, &address, &get_request);
    assert_eq!(get_refused.status, 400, "{:?}", get_refused.body);
    assert!(!get_refused.body.is_empty(), "GET 400 keeps its JSON body");
    let get_length = header_value(&get_refused.headers, "content-length")
        .expect("GET 400 advertises a body")
        .to_string();

    let head_request = format!(
        "HEAD /api/v1/services HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        address
    );
    let head_refused = send_request_head(&sandbox, &address, &head_request);
    assert_eq!(head_refused.status, 400);
    assert!(
        head_refused.body.is_empty(),
        "HEAD 400 carried a body: {}",
        String::from_utf8_lossy(&head_refused.body)
    );
    assert_eq!(
        header_value(&head_refused.headers, "content-length"),
        Some(get_length.as_str()),
        "HEAD 400 keeps the GET Content-Length"
    );
}

/// A blocked route answers through the blocking pool's completion, not the
/// inline write path. A HEAD 404 produced there must drop its body too.
#[test]
fn head_blocking_unknown_endpoint_has_no_body() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let get_missing = raw_request(&address, "GET", "/api/v1/does-not-exist", "", b"");
    assert_eq!(get_missing.status, 404);
    assert!(!get_missing.body.is_empty(), "GET 404 keeps its JSON body");
    let get_length = header_value(&get_missing.headers, "content-length")
        .expect("GET 404 advertises a body")
        .to_string();

    let head_missing = raw_request(&address, "HEAD", "/api/v1/does-not-exist", "", b"");
    assert_eq!(head_missing.status, 404);
    assert!(
        head_missing.body.is_empty(),
        "HEAD 404 carried a body: {}",
        String::from_utf8_lossy(&head_missing.body)
    );
    assert_eq!(
        header_value(&head_missing.headers, "content-length"),
        Some(get_length.as_str()),
        "HEAD 404 keeps the GET Content-Length"
    );
}

#[test]
fn chunked_bodies_work_for_management_json_routes() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let key = SigningKey::from_bytes(&[31u8; 32]);
    let pem = pem_from_public(&key.verifying_key().to_bytes());
    let chunked = raw_request_chunked(
        &address,
        "POST",
        "/api/v1/trust?key_id=chunked-key",
        "application/octet-stream",
        &pem,
        &[],
    );
    assert_eq!(chunked.status, 200, "{:?}", chunked.body);

    let listed = request(&address, "GET", "/api/v1/trust", "", b"");
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert!(listed.body["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .any(|key| key == "chunked-key"));
}

#[test]
fn oversized_upload_is_rejected_before_the_body() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let mut stream = TcpStream::connect(&address).expect("connect");
    let request = format!(
        "POST /api/v1/plugins HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        512u64 * 1024 * 1024 + 1
    );
    stream.write_all(request.as_bytes()).expect("write headers");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let head = String::from_utf8_lossy(&response);
    assert!(head.starts_with("HTTP/1.1 413"), "{}", head);
}

/// The route's body policy is enforced before the face runs. An Empty route
/// declares that it reads no body, so a client that announces one is refused
/// without the server buffering a byte.
#[test]
fn a_route_that_declares_no_body_refuses_one_before_the_face() {
    let (_sandbox, address) = daemon_with_demo_plugin(47);
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "POST /api/v1/services/demo/start HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nExpect: 100-continue\r\nContent-Length: 2\r\nConnection: close\r\n\r\n",
        address
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let response = read_raw_response(&mut stream);
    assert_eq!(
        response.status,
        413,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
}

/// A Streamed face route declares that it reads no buffered body. A client
/// that announces one is refused before the face opens the stream.
#[test]
fn a_streamed_route_refuses_a_declared_body_before_the_face() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "GET /api/v1/export/demo HTTP/1.1\r\nHost: {}\r\nExpect: 100-continue\r\nContent-Length: 1\r\nConnection: close\r\n\r\n",
        address
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let response = read_raw_response(&mut stream);
    assert_eq!(
        response.status,
        413,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
}

/// A buffered route accepts the body only after announcing the interim
/// response, so a client that waits for 100 Continue still makes progress
/// through the ordinary management surface.
#[test]
fn a_buffered_route_sends_100_continue_before_reading_its_body() {
    let (_sandbox, address) = daemon_with_demo_plugin(48);
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let body = b"good config\n";
    let head = format!(
        "PUT /api/v1/config/demo HTTP/1.1\r\nHost: {}\r\nContent-Type: text/plain\r\nExpect: 100-continue\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let mut interim = Vec::new();
    let mut byte = [0u8; 1];
    while !interim.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("100 continue");
        interim.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&interim).starts_with("HTTP/1.1 100"),
        "{:?}",
        String::from_utf8_lossy(&interim)
    );
    stream.write_all(body).expect("write body");
    let response = read_raw_response(&mut stream);
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
}

/// A 413 before the body closes instead of treating the bytes after the
/// rejected request as the next keep-alive request.
#[test]
fn a_413_before_the_body_does_not_answer_the_next_keep_alive_request() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("max_json_body_bytes = 4096\n");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let pipelined = format!(
        "PUT /api/v1/config/demo HTTP/1.1\r\nHost: {}\r\nContent-Type: text/plain\r\nContent-Length: 4097\r\nConnection: keep-alive\r\n\r\nGET /api/v1/version HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address, address
    );
    stream
        .write_all(pipelined.as_bytes())
        .expect("write pipelined requests");
    let response = read_raw_response(&mut stream);
    assert_eq!(
        response.status,
        413,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    assert!(
        !String::from_utf8_lossy(&response.body).contains("protocol"),
        "the rejected request's trailing bytes must not be answered: {}",
        String::from_utf8_lossy(&response.body)
    );
}

/// A route that reads no body must not answer Expect: 100-continue: there is
/// nothing to wait for, and a 100 line would change the response the client
/// reads.
#[test]
fn a_bodyless_route_does_not_send_100_continue() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "GET /api/v1/version HTTP/1.1\r\nHost: {}\r\nExpect: 100-continue\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        address
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let response = read_raw_response(&mut stream);
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
}

#[test]
fn a_large_body_sent_after_its_headers_is_read_to_completion() {
    let (_sandbox, address) = daemon_with_demo_plugin(32);

    // A small body split from its headers binds the assertion to the socket
    // behaviour: the broken non-blocking socket answered 400 "cannot read
    // request body" instead of waiting. The 300ms pause is deliberately
    // generous; the in-process daemon parses the headers in microseconds, so
    // it is already inside the body read when the rest arrives.
    let small = raw_request_split_body(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"good config\n",
        Duration::from_millis(300),
    );
    assert_eq!(
        small.status,
        200,
        "{}",
        String::from_utf8_lossy(&small.body)
    );

    // The oversized body must be rejected by the config cap, not by a read
    // error. This is the ticket's 1 MiB + 1 case.
    let too_large = vec![b'x'; 1024 * 1024 + 1];
    let response = raw_request_split_body(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        &too_large,
        Duration::from_millis(300),
    );
    assert_eq!(
        response.status,
        413,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
}

#[test]
fn http_keep_alive_serves_two_requests_on_one_connection() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");

    let first = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    stream.write_all(first.as_bytes()).expect("first request");
    let first = read_one_response(&mut stream);
    assert_eq!(first.status, 200);
    assert_eq!(raw_header_values(&first, "connection"), vec!["keep-alive"]);

    let second = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    stream.write_all(second.as_bytes()).expect("second request");
    let second = read_one_response(&mut stream);
    assert_eq!(second.status, 200);
    assert_eq!(raw_header_values(&second, "connection"), vec!["close"]);
}

#[test]
fn http_keep_alive_serves_version_through_the_face() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");

    let first = format!(
        "GET /api/v1/version HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    stream.write_all(first.as_bytes()).expect("first request");
    let first = read_one_response(&mut stream);
    assert_eq!(first.status, 200);
    assert_eq!(raw_header_values(&first, "connection"), vec!["keep-alive"]);
    let body: Value = serde_json::from_slice(&first.body).expect("json body");
    assert_eq!(body["protocol"], 1);

    let second = format!(
        "GET /api/v1/version HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    stream.write_all(second.as_bytes()).expect("second request");
    let second = read_one_response(&mut stream);
    assert_eq!(second.status, 200);
    assert_eq!(raw_header_values(&second, "connection"), vec!["close"]);
}

/// An idle connection that has not sent a request must be closed silently
/// after `http_idle_timeout_secs`; it must not receive a 400 the client never
/// asked for. Both engines close the socket, so the assertion is the same.
#[test]
fn an_idle_connection_is_closed_without_a_response() {
    let mut sandbox = Sandbox::new();
    fs::write(
        sandbox.config.join("svcd.conf"),
        "http_idle_timeout_secs = 1\n",
    )
    .expect("write svcd.conf");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let mut idle = TcpStream::connect(&address).expect("idle connection");
    idle.set_read_timeout(Some(Duration::from_secs(8)))
        .expect("timeout");
    let mut sink = [0u8; 64];
    match idle.read(&mut sink) {
        Ok(0) => {}
        Ok(read) => panic!(
            "an idle connection must be closed, not answered: {:?}",
            String::from_utf8_lossy(&sink[..read])
        ),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            panic!("the daemon did not close an idle connection within 8 s")
        }
        // A reset from the server's close is still a close, not a response.
        Err(_) => {}
    }
}

#[cfg(target_os = "linux")]
#[test]
fn unix_idle_thread_count_has_no_per_request_or_per_stream_threads() {
    let (sandbox, address) = daemon_with_web_event_plugin(55);
    let pid = sandbox.daemon.as_ref().expect("running daemon").id();

    let idle = stable_thread_count(pid);
    assert_eq!(
        idle, IDLE_DAEMON_THREADS,
        "idle daemon uses {idle} threads; the default S4 baseline is {IDLE_DAEMON_THREADS}, so this is a thread-budget regression"
    );
    let names = daemon_thread_names(pid);
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("svcd-channel-r") || name.starts_with("svcd-channel-c")),
        "the control and channel-client reactor threads are gone: {names:?}"
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| name.starts_with("svcd-server-loo"))
            .count(),
        1,
        "the control channel runs on the one server loop: {names:?}"
    );
    // /proc/<pid>/task/*/comm truncates the name to 15 characters, so these
    // prefixes are the observable form of svcd-http-worker, svcd-temp-cleanup,
    // svcd-import-http, svcd-install-http and the standalone svcd-reactor.
    for gone in [
        "svcd-http-work",
        "svcd-temp-clea",
        "svcd-import-ht",
        "svcd-install-h",
        "svcd-reactor",
    ] {
        assert!(
            !names.iter().any(|name| name.starts_with(gone)),
            "{gone} must not survive #125: {names:?}"
        );
    }

    let mut connections = Vec::new();
    for _ in 0..16 {
        connections.push(open_keep_alive(&address));
    }
    assert_eq!(
        stable_thread_count(pid),
        idle,
        "16 idle keep-alive connections must not add one daemon thread each"
    );

    let base = plugin_ui_base(&address, "demo");
    let mut streams = Vec::new();
    for _ in 0..8 {
        streams.push(sse_connect(&address, &format!("{}events", base)));
    }
    assert_eq!(
        stable_thread_count(pid),
        idle,
        "8 open SSE streams must not add one daemon thread each"
    );

    // A browser page load is many static assets. Serving them on the server
    // loop must not create one daemon thread per asset.
    let mut assets = Vec::new();
    for _ in 0..16 {
        let address = address.clone();
        let page = base.clone();
        assets.push(std::thread::spawn(move || {
            let response = raw_request(&address, "GET", &page, "", b"");
            assert_eq!(
                response.status,
                200,
                "{}",
                String::from_utf8_lossy(&response.body)
            );
        }));
    }
    for asset in assets {
        asset.join().expect("plugin UI asset request");
    }
    assert_eq!(
        stable_thread_count(pid),
        idle,
        "a page load's static assets must not add one daemon thread each"
    );

    drop(streams);
    drop(connections);
    assert_eq!(
        stable_thread_count(pid),
        idle,
        "closing connections and streams must leave no daemon threads behind"
    );
}
