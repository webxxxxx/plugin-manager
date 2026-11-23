//! auth http tests.

use super::*;

/// An unauthenticated write must not receive 100 Continue: the server refuses
/// it before the body and closes, so unread body bytes cannot precede the next
/// request on a keep-alive connection.
#[test]
fn an_unauthenticated_write_with_expect_is_refused_before_the_body() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_http();
    let (address, _password) = enable_auth(&mut sandbox);
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "PUT /api/v1/config/demo HTTP/1.1\r\nHost: {}\r\nContent-Type: text/plain\r\nExpect: 100-continue\r\nContent-Length: 5\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let response = read_raw_response(&mut stream);
    assert_eq!(
        response.status,
        401,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    assert_eq!(
        raw_header_values(&response, "connection"),
        vec!["close"],
        "an early refusal must close instead of leaving an unread body in front of the next request"
    );
}

#[test]
fn lan_bind_without_password_falls_back_to_loopback() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_with_bind("0.0.0.0:0");
    let address = sandbox.wait_for_http();

    let open = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(open.status, 200);
    let status = sandbox.run_svc(&["lan", "status"]);
    assert_eq!(status.status.code(), Some(0));
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("requested: 0.0.0.0:0"), "{text}");
    assert!(text.contains("auth required: false"), "{text}");
}

#[test]
fn password_auth_bearer_cookie_csrf() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let open = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(open.status, 200);

    let before_password = sandbox.run_svc(&["lan", "enable", "--bind", "0.0.0.0"]);
    assert_ne!(before_password.status.code(), Some(0));

    let changed = sandbox.run_svc(&["lan", "passwd", TEST_PASSWORD]);
    assert_eq!(
        changed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    // auth is derived from bind, so restart with a LAN bind after the password
    // exists. No `lan enable` step is involved anymore.
    sandbox.shutdown();
    sandbox.start_daemon_with_bind("0.0.0.0:0");
    let address = sandbox.wait_for_http();
    let password = TEST_PASSWORD.to_string();
    assert!(!sandbox.config.join("lan-token").exists());
    assert!(!sandbox.config.join("lan.json").exists());

    let page = raw_request(&address, "GET", "/", "", b"");
    assert_eq!(page.status, 200, "console must load before login");

    let icon = raw_request(&address, "GET", "/assets/favicon.svg", "", b"");
    assert_eq!(icon.status, 200, "the tab icon must load before login");
    assert_eq!(
        raw_header_values(&icon, "content-type"),
        vec!["image/svg+xml"]
    );

    let denied = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(denied.status, 401);
    assert_eq!(denied.body["error"]["code"], "unauthorized");

    // `/api/v1/version` moved behind the S4 face; the server still resolves
    // authentication before the face runs, so it stays behind the same gate.
    let denied_version = request(&address, "GET", "/api/v1/version", "", b"");
    assert_eq!(denied_version.status, 401);
    assert_eq!(denied_version.body["error"]["code"], "unauthorized");

    // An export is a streamed face route now, but it is not a public route:
    // authentication still happens before any archive work.
    let denied_export = request(&address, "GET", "/api/v1/export/demo", "", b"");
    assert_eq!(denied_export.status, 401);
    assert_eq!(denied_export.body["error"]["code"], "unauthorized");

    let bearer = format!("Bearer {}", password);
    let allowed = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(allowed.status, 200, "{}", allowed.body);
    let allowed_version = request_with(
        &address,
        "GET",
        "/api/v1/version",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(allowed_version.status, 200, "{}", allowed_version.body);
    assert_eq!(allowed_version.body["protocol"], 1);

    let login = request_with(
        &address,
        "POST",
        "/login",
        "application/json",
        serde_json::to_string(&json!({ "password": password }))
            .expect("login")
            .as_bytes(),
        &[],
    );
    assert_eq!(login.status, 200, "{}", login.body);
    let cookie = cookie_value(&login, "svcd_api").expect("svcd_api cookie");
    let session = cookie_value(&login, "svcd_session").expect("svcd_session cookie");
    let cookies = header_values(&login, "set-cookie").join("\n");
    assert!(cookies.contains("Path=/api/v1/"), "{cookies}");
    let session_header = header_values(&login, "set-cookie")
        .into_iter()
        .find(|value| value.starts_with("svcd_session="))
        .expect("svcd_session header");
    // The session cookie must be sent to /logout, not only to /plugin-ui/.
    assert!(session_header.contains("Path=/;"), "{session_header}");
    assert!(cookies.contains("HttpOnly"), "{cookies}");
    assert!(cookies.contains("SameSite=Strict"), "{cookies}");
    assert!(cookies.contains("Max-Age=86400"), "{cookies}");
    assert!(!session.is_empty());

    let api_cookie = format!("svcd_api={}", cookie);
    let cookie_get = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Cookie", api_cookie.as_str())],
    );
    assert_eq!(cookie_get.status, 200);

    let no_csrf = request_with(
        &address,
        "POST",
        "/api/v1/services/svc/stop",
        "",
        b"",
        &[("Cookie", api_cookie.as_str())],
    );
    assert_eq!(no_csrf.status, 403);

    // The blob upload face applies the same cookie-write CSRF policy as the
    // buffered management routes. Send only the head: the server rejects
    // before the body, and writing the body could race the close with a reset.
    let mut blob_stream = TcpStream::connect(&address).expect("blob csrf connect");
    let blob_head = request_head(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        9,
        &[("Cookie", api_cookie.as_str())],
    );
    blob_stream
        .write_all(blob_head.as_bytes())
        .expect("blob csrf head");
    let (blob_status, _) = read_response_head(&mut blob_stream);
    assert_eq!(blob_status, 403);

    let origin = format!("http://{}", address);
    let csrf = request_with(
        &address,
        "POST",
        "/api/v1/services/svc/stop",
        "",
        b"",
        &[
            ("Cookie", api_cookie.as_str()),
            ("Origin", origin.as_str()),
            ("X-Svcd-Csrf", "1"),
        ],
    );
    assert_eq!(csrf.status, 200, "{}", csrf.body);

    let auth_status = request_with(
        &address,
        "GET",
        "/api/v1/auth",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(auth_status.status, 200);
    assert_eq!(auth_status.body["authRequired"], true);
    assert_eq!(auth_status.body["passwordSet"], true);
    assert!(
        auth_status.body["bind"]
            .as_str()
            .unwrap_or_default()
            .starts_with("0.0.0.0:"),
        "{}",
        auth_status.body
    );

    let logout = request_with(
        &address,
        "POST",
        "/logout",
        "",
        b"",
        &[("Cookie", api_cookie.as_str())],
    );
    assert_eq!(logout.status, 200);
    let after_logout = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Cookie", api_cookie.as_str())],
    );
    assert_eq!(after_logout.status, 401);

    let changed = sandbox.run_svc(&["lan", "passwd", TEST_PASSWORD_2]);
    assert_eq!(changed.status.code(), Some(0));
    let old_bearer = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(old_bearer.status, 401);
    let new_bearer = format!("Bearer {}", TEST_PASSWORD_2);
    let new_allowed = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", new_bearer.as_str())],
    );
    assert_eq!(new_allowed.status, 200);

    let status = sandbox.run_svc(&["lan", "status"]);
    assert_eq!(status.status.code(), Some(0));
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(status_text.contains("auth required: true"), "{status_text}");
    assert!(status_text.contains("password: set"), "{status_text}");

    sandbox.shutdown();
    sandbox.start_daemon_with_bind("0.0.0.0:0");
    let address = sandbox.wait_for_http();
    let denied_after_restart = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(denied_after_restart.status, 401);
    let new_bearer = format!("Bearer {}", TEST_PASSWORD_2);
    let allowed_after_restart = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", new_bearer.as_str())],
    );
    assert_eq!(allowed_after_restart.status, 200);
}

#[test]
fn plugin_ui_token_is_bound_to_its_plugin() {
    let (_sandbox, address) = web_ui_sandbox(44);
    let key = SigningKey::from_bytes(&[44u8; 32]);
    let other = web_plugin_bundle(&key, "other", true);
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &other,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);

    let demo_base = plugin_ui_base(&address, "demo");
    let own = plugin_ui_base(&address, "other");
    let page = raw_request(&address, "GET", &own, "", b"");
    assert_eq!(page.status, 200);

    // The demo token must not unlock the other plugin's UI.
    let cross = demo_base.replacen("/plugin-ui/demo/", "/plugin-ui/other/", 1);
    let denied = raw_request(&address, "GET", &cross, "", b"");
    assert_eq!(denied.status, 401);
}

#[test]
fn plugin_web_ui_requires_a_session_when_lan_is_enabled() {
    let (mut sandbox, _) = web_ui_sandbox(43);

    let (address, password) = enable_auth(&mut sandbox);
    let bearer = format!("Bearer {}", password);
    let base = plugin_ui_base_with(&address, "demo", &[("Authorization", bearer.as_str())]);

    let body = rpc_body(21, "status");
    let page = raw_request(&address, "GET", &base, "", b"");
    assert_eq!(page.status, 401);
    let browsed = raw_request_with(&address, "GET", &base, "", b"", &[("Accept", "text/html")]);
    assert_eq!(browsed.status, 303);
    assert!(raw_header_values(&browsed, "location")[0].contains(&format!("/?next={}", base)));
    let anonymous = raw_request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &body,
    );
    assert_eq!(anonymous.status, 401);
    // The 308 canonicalisation runs after auth, so a slashless URL is just
    // another unauthenticated /plugin-ui/* request when LAN is enabled.
    let canonical = raw_request(&address, "GET", "/plugin-ui/demo", "", b"");
    assert_eq!(canonical.status, 401);

    let started = request_with(
        &address,
        "POST",
        "/api/v1/services/demo/start",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(started.status, 200, "{}", started.body);

    let login = request_with(
        &address,
        "POST",
        "/login",
        "application/json",
        serde_json::to_string(&json!({"password": password}))
            .expect("login body")
            .as_bytes(),
        &[],
    );
    assert_eq!(login.status, 200, "{}", login.body);
    let session = cookie_value(&login, "svcd_session").expect("svcd_session cookie");
    let cookie = format!("svcd_session={}", session);

    let page = raw_request_with(
        &address,
        "GET",
        &base,
        "",
        b"",
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));

    let origin = format!("http://{}", address);
    let call = request_with(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &body,
        &[("Cookie", cookie.as_str()), ("Origin", origin.as_str())],
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(call.body["id"], 21);
    assert_eq!(call.body["result"]["content"][0]["text"], "pong");

    // Cookie-authenticated /rpc still needs a same-origin Origin (CSRF guard).
    let no_origin = raw_request_with(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &body,
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(no_origin.status, 403);
    let foreign = raw_request_with(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &body,
        &[
            ("Cookie", cookie.as_str()),
            ("Origin", "http://evil.example"),
        ],
    );
    assert_eq!(foreign.status, 403);

    // Logout must revoke the session server-side, not just clear the console's
    // local token: a plugin tab that keeps the old cookie must not stay logged in.
    let logout = request_with(
        &address,
        "POST",
        "/logout",
        "",
        b"",
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(logout.status, 200);
    let after_logout = raw_request_with(
        &address,
        "GET",
        &base,
        "",
        b"",
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(after_logout.status, 401);
}

#[test]
fn plugin_ui_streams_require_lan_auth_and_same_origin() {
    let (mut sandbox, _) = daemon_with_web_event_plugin(53);
    let (address, password) = enable_auth(&mut sandbox);
    let bearer = format!("Bearer {}", password);
    let base = plugin_ui_base_with(&address, "demo", &[("Authorization", bearer.as_str())]);

    let sse = raw_request(&address, "GET", &format!("{}events", base), "", b"");
    assert_eq!(sse.status, 401);
    let (_stream, status, _head) = ws_handshake(&address, &format!("{}ws", base), &[]);
    assert_eq!(status, 401);

    let evil = ws_handshake(
        &address,
        &format!("{}ws", base),
        &[
            ("Authorization", bearer.as_str()),
            ("Origin", "http://evil"),
        ],
    );
    assert_eq!(evil.1, 403);

    let origin = format!("http://{}", address);
    let good = ws_handshake(
        &address,
        &format!("{}ws", base),
        &[
            ("Authorization", bearer.as_str()),
            ("Origin", origin.as_str()),
        ],
    );
    assert_eq!(good.1, 101, "{}", good.2);
}
