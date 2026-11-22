//! Log read/clear http tests (#262).

use super::*;

#[test]
fn the_daemon_log_is_readable_and_clearable() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let log = raw_request(&address, "GET", "/api/v1/logs?lines=50", "", b"");
    assert_eq!(log.status, 200, "{}", String::from_utf8_lossy(&log.body));
    assert_eq!(
        raw_header_values(&log, "content-type"),
        vec!["text/plain; charset=utf-8"]
    );
    assert!(
        String::from_utf8_lossy(&log.body).contains("svcd"),
        "the daemon log should carry its own startup lines"
    );

    assert_eq!(
        raw_request(&address, "GET", "/api/v1/logs?lines=2001", "", b"").status,
        400
    );
    assert_eq!(
        raw_request(&address, "GET", "/api/v1/logs?level=loud", "", b"").status,
        400
    );
    assert_eq!(
        raw_request(&address, "POST", "/api/v1/logs", "", b"").status,
        405
    );

    let cleared = request(&address, "DELETE", "/api/v1/logs", "", b"");
    assert_eq!(cleared.status, 200, "{:?}", cleared.body);
    assert_eq!(cleared.body["cleared"], "svcd");
    assert!(
        cleared.body["files"].as_u64().unwrap_or(0) >= 1,
        "{:?}",
        cleared.body
    );

    // The reset goes through the writer that holds the handle, which then
    // reopens the file: the audit line lands in the new file and later daemon
    // lines keep arriving in the visible one.
    let after = raw_request(&address, "GET", "/api/v1/logs", "", b"");
    assert_eq!(after.status, 200);
    let text = String::from_utf8_lossy(&after.body).into_owned();
    assert!(text.contains("logs cleared: svcd"), "{text}");

    let again = request(&address, "DELETE", "/api/v1/logs", "", b"");
    assert_eq!(again.status, 200, "{:?}", again.body);
    let text = String::from_utf8_lossy(&raw_request(&address, "GET", "/api/v1/logs", "", b"").body)
        .into_owned();
    assert_eq!(
        text.matches("logs cleared: svcd").count(),
        1,
        "a second clear must drop the first audit line: {text}"
    );
}

#[test]
fn clearing_a_running_plugin_log_keeps_the_writer_on_the_visible_file() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    // `/mcp` always needs a bearer, even on a loopback bind.
    set_password(&sandbox, TEST_PASSWORD);
    let key = SigningKey::from_bytes(&[52u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &chatty_plugin_bundle(&key, "chatty"),
    );
    assert_eq!(installed.status, 200, "{:?}", installed.body);
    let started = request(&address, "POST", "/api/v1/services/chatty/start", "", b"");
    assert_eq!(started.status, 200, "{:?}", started.body);

    let log = sandbox.logs.join("chatty.log");
    wait_until(
        &sandbox,
        "the plugin writes its first stderr line",
        || {
            fs::read_to_string(&log)
                .map(|text| text.contains("chatty ready"))
                .unwrap_or(false)
        },
        || {
            format!(
                "log dir: {:?}",
                fs::read_dir(&sandbox.logs).map(|entries| entries.count())
            )
        },
    );

    let cleared = request(&address, "DELETE", "/api/v1/logs/chatty", "", b"");
    assert_eq!(cleared.status, 200, "{:?}", cleared.body);
    assert_eq!(cleared.body["cleared"], "chatty");
    // The writer reopens the path. Deleting the file instead would leave a
    // delete-pending path on Windows: gone from the namespace, still written to.
    assert!(log.is_file(), "the cleared log must still be the live path");
    // The reopened file is empty, not missing: a 200 with no lines.
    let empty = raw_request(&address, "GET", "/api/v1/logs/chatty", "", b"");
    assert_eq!(empty.status, 200);
    assert!(empty.body.is_empty(), "{:?}", empty.body);

    // A second clear proves the reopened path is a real file the writer holds:
    // a deleted-but-still-open file reports zero files instead.
    let again = request(&address, "DELETE", "/api/v1/logs/chatty", "", b"");
    assert_eq!(again.status, 200, "{:?}", again.body);
    assert_eq!(again.body["files"], 1, "{:?}", again.body);
    let tail = raw_request(&address, "GET", "/api/v1/logs/chatty", "", b"");
    assert_eq!(tail.status, 200);
    assert!(tail.body.is_empty(), "{:?}", tail.body);
}

#[test]
fn clearing_removes_hooks_and_rotations_and_404s_when_empty() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let key = SigningKey::from_bytes(&[53u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &plugin_bundle(&key, "demo", "1.0.0", None, false),
    );
    assert_eq!(installed.status, 200, "{:?}", installed.body);

    let files = [
        "demo.log",
        "demo.log.1",
        "demo-hooks.log",
        "demo-hooks.log.1",
    ];
    for name in files {
        fs::write(sandbox.logs.join(name), "line\n").expect("write log");
    }
    let cleared = request(&address, "DELETE", "/api/v1/logs/demo", "", b"");
    assert_eq!(cleared.status, 200, "{:?}", cleared.body);
    assert_eq!(cleared.body["files"], 4, "{:?}", cleared.body);
    for name in files {
        assert!(
            !sandbox.logs.join(name).exists(),
            "{name} survived the clear"
        );
    }

    assert_eq!(
        request(&address, "DELETE", "/api/v1/logs/demo", "", b"").status,
        404,
        "nothing left to clear is a 404"
    );
    assert_eq!(
        request(&address, "DELETE", "/api/v1/logs/nope", "", b"").status,
        404,
        "an unknown plugin is a 404"
    );

    let (address, _password) = enable_auth(&mut sandbox);
    assert_eq!(
        raw_request(&address, "DELETE", "/api/v1/logs", "", b"").status,
        401
    );
    assert_eq!(
        raw_request(&address, "DELETE", "/api/v1/logs/demo", "", b"").status,
        401
    );
}
