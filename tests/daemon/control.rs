//! control daemon tests.

use super::*;

/// The suite already bounded this wait; it now shares the helper with the
/// other sandboxes, so the request has a bound too.
#[test]
fn stop_wait_is_bounded_when_the_request_cannot_reach_the_daemon() {
    let mut sandbox = Sandbox::new();
    let timeout = Duration::from_secs(1);
    sandbox.stop_timeout = timeout;
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    let unreachable = sandbox.root.join("no-daemon");
    fs::create_dir_all(&unreachable).expect("unreachable config");

    support::assert_bounded_stop(|| sandbox.stop_daemon_at(&unreachable), timeout);
    assert!(sandbox.daemon.is_none(), "the handle must be consumed");
}

/// The channel authorizes by endpoint ACL now; a token left by the previous
/// version must not survive a start.
#[test]
fn the_legacy_control_token_is_removed_on_start() {
    let mut sandbox = Sandbox::new();
    fs::write(sandbox.config.join("svcd.token"), "legacy-token").expect("legacy token");
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert!(
        !sandbox.config.join("svcd.token").exists(),
        "the daemon must remove the token it no longer reads"
    );
}

#[test]
fn a_graceful_shutdown_runs_the_stop_action() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("shutdown-stop.txt");
    let hook = sandbox.write_hook_script("shutdown-stop", &marker);
    sandbox.write_core_json("script");
    let path = sandbox.config.join("services.d").join("script.json");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&path).expect("definition")).expect("json");
    value["lifecycle"] = json!({"stop": {"path": hook.to_string_lossy(), "args": []}});
    fs::write(&path, serde_json::to_vec_pretty(&value).expect("json")).expect("write definition");

    sandbox.start_daemon();
    sandbox.running_pid("script");
    sandbox.shutdown();

    wait_until("the shutdown cleanup", Duration::from_secs(10), || {
        fs::read_to_string(&marker)
            .map(|text| text.contains("stop"))
            .unwrap_or(false)
    });
}

#[test]
fn a_second_daemon_refuses_to_start() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    let mut command = Command::new(&sandbox.svcd);
    command
        .args(["run", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    sandbox.env(&mut command);
    let mut second = command.spawn().expect("spawn second daemon");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = second.try_wait().expect("try_wait") {
            assert_eq!(
                status.code(),
                Some(1),
                "second daemon should refuse to start"
            );
            return;
        }
        assert!(Instant::now() < deadline, "the second daemon did not exit");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// An HTTP bind failure must disable only HTTP. The control channel owns the
/// server loop now, so the CLI keeps working on the same daemon.
#[test]
fn a_busy_http_port_keeps_the_control_channel_alive() {
    let occupied = TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let port = occupied.local_addr().expect("occupied port").port();
    let bind = format!("127.0.0.1:{}", port);
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_with_env(&[("SVCD_BIND", bind.as_str())]);
    // reload has no offline fallback; answering it proves the control loop is up.
    sandbox.wait_for_daemon();
    sandbox.shutdown();
}
