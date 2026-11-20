//! control http tests.

#[cfg(unix)]
use std::io::{BufRead, BufReader};

use super::*;

/// The suite used to wait on `child.wait()` forever when the shutdown request
/// never reached the daemon. The wait is bounded now and reports why.
#[test]
fn stop_wait_is_bounded_when_the_request_cannot_reach_the_daemon() {
    let mut sandbox = Sandbox::new();
    let timeout = Duration::from_secs(1);
    sandbox.stop_timeout = timeout;
    sandbox.start_daemon();
    sandbox.wait_for_http();
    let unreachable = sandbox.root.join("no-daemon");
    fs::create_dir_all(&unreachable).expect("unreachable config");

    support::assert_bounded_stop(|| sandbox.stop_daemon_at(&unreachable), timeout);
    assert!(sandbox.daemon.is_none(), "the handle must be consumed");
}

#[cfg(unix)]
#[test]
fn a_stale_control_socket_under_data_run_is_replaced() {
    let mut sandbox = Sandbox::new();
    let run = sandbox.data.join("run");
    fs::create_dir_all(&run).expect("run dir");
    fs::write(run.join("svcd.sock"), b"stale").expect("stale socket");

    sandbox.start_daemon();
    sandbox.wait_for_http();

    assert!(
        run.join("svcd.sock").exists(),
        "the control socket must bind under data/run"
    );
    assert!(
        !sandbox.config.join("svcd.sock").exists(),
        "config_dir must stay input-only"
    );
}

/// The control channel and `GET /api/v1/auth` render the same four-key auth
/// status from one `LanStatus`. Keep them honest with a real daemon: a drift in
/// either renderer (or in the value it derives) turns this red.
#[test]
fn control_and_http_report_the_same_auth_status() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let _ = sandbox.wait_for_http();

    let changed = sandbox.run_svc(&["lan", "passwd", TEST_PASSWORD]);
    assert_eq!(
        changed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    // Bind to a LAN address so `authRequired` is true on both surfaces.
    sandbox.shutdown();
    sandbox.start_daemon_with_bind("0.0.0.0:0");
    let address = sandbox.wait_for_http();

    let bearer = format!("Bearer {}", TEST_PASSWORD);
    let http = request_with(
        &address,
        "GET",
        "/api/v1/auth",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(http.status, 200, "{}", http.body);

    let status = sandbox.run_svc(&["lan", "status"]);
    assert_eq!(
        status.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let text = String::from_utf8_lossy(&status.stdout);

    let bind = lan_status_field(&text, "bind:");
    let requested = lan_status_field(&text, "requested:");
    let auth_required = lan_status_field(&text, "auth required:");
    let password = lan_status_field(&text, "password:");

    assert_eq!(http.body["bind"].as_str(), Some(bind.as_str()), "{text}");
    assert_eq!(
        http.body["requestedBind"].as_str(),
        Some(requested.as_str()),
        "{text}"
    );
    assert_eq!(http.body["authRequired"], auth_required == "true", "{text}");
    assert_eq!(http.body["passwordSet"], password == "set", "{text}");
}

/// The server loop owns the control channel now. A shutdown request must still
/// receive its reply: the loop drains queued control replies and flushes them
/// before the process exits.
#[cfg(unix)]
#[test]
fn control_shutdown_reply_is_flushed_before_exit() {
    use std::os::unix::net::UnixStream;

    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let _address = sandbox.wait_for_http();
    let mut stream = UnixStream::connect(sandbox.data.join("run").join("svcd-admin.sock"))
        .expect("admin connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let request = "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"svcd.shutdown\",\"params\":{}}\n";
    stream
        .write_all(request.as_bytes())
        .expect("write shutdown request");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("shutdown reply");
    let response: Value = serde_json::from_str(line.trim()).expect("shutdown reply json");
    assert_eq!(response["id"], 7, "{line}");
    assert!(response.get("error").is_none(), "{line}");

    if let Some(mut child) = sandbox.daemon.take() {
        let mut exited = false;
        for _ in 0..300 {
            if child.try_wait().expect("daemon wait").is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(exited, "daemon did not exit after the shutdown reply");
    }
}

/// The endpoint, not a token, decides: an admin method on the user endpoint
/// gets the same -32000 unauthorized reply and must not run.
#[cfg(unix)]
#[test]
fn control_channel_rejects_an_admin_method_on_the_user_endpoint() {
    use std::os::unix::net::UnixStream;

    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let _address = sandbox.wait_for_http();
    let mut stream =
        UnixStream::connect(sandbox.data.join("run").join("svcd.sock")).expect("control connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let request = "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"svcd.shutdown\",\"params\":{}}\n";
    stream
        .write_all(request.as_bytes())
        .expect("write shutdown request");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("shutdown reply");
    let response: Value = serde_json::from_str(line.trim()).expect("shutdown reply json");
    assert_eq!(response["id"], 3, "{line}");
    assert_eq!(response["error"]["code"], -32000, "{line}");
    assert_eq!(response["error"]["message"], "unauthorized", "{line}");

    // The rejected admin method must leave the user tier usable: `svc list`
    // goes through svcd.sock and would fail if shutdown had run.
    let listed = sandbox.run_svc(&["list"]);
    assert_eq!(
        listed.status.code(),
        Some(0),
        "user tier after rejected admin method: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
}

/// A stuck supervisor must not pin a blocking worker or an HTTP connection.
/// Two concurrent management calls hit their control deadline (504, not the
/// queue-full 503), and the daemon serves requests again once the stop hook
/// returns. The hook runs on the supervisor loop, so both calls queue behind
/// the same stall; without a deadline they would occupy both workers for the
/// whole hook.
#[test]
fn a_stuck_supervisor_answers_504_and_the_worker_recovers() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("control_timeout_secs = 1\n");
    sandbox.write_core();

    let hook = if cfg!(windows) {
        sandbox.root.join("slow-stop.cmd")
    } else {
        sandbox.root.join("slow-stop.sh")
    };
    // The marker makes the test deterministic: it proves the supervisor is
    // inside the hook before the second request is sent, instead of relying on
    // a fixed sleep.
    let entered = sandbox.root.join("hook-entered");
    #[cfg(windows)]
    {
        // Four pings, one per second: roughly three seconds of sleep.
        fs::write(
            &hook,
            format!(
                "@echo off\r\necho entered>\"{}\"\r\nping -n 4 127.0.0.1 >nul\r\n",
                entered.display()
            ),
        )
        .expect("stop hook");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\nsleep 3\n", entered.display()),
        )
        .expect("stop hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("hook mode");
    }
    let definition = sandbox.config.join("services.d").join("demo.json");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&definition).expect("definition")).expect("json");
    value["lifecycle"] = json!({
        "stop": {"path": hook.to_string_lossy(), "args": [], "timeout": 5}
    });
    fs::write(&definition, serde_json::to_vec(&value).expect("definition")).expect("write");

    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    // Setup, not subject: the test runs with the deliberately tiny
    // `control_timeout_secs = 1`, and a loaded host can push a normal control
    // round trip past it. Retry the start (it is idempotent) instead of reading
    // one 504 as a regression; the 504 assertions below are the subject. Keep
    // the last answer so a timeout still prints what the daemon replied.
    let last_start = std::cell::RefCell::new(String::from("no attempt"));
    wait_until_within(
        &sandbox,
        "the demo service never started",
        Duration::from_secs(10),
        || {
            let response = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
            if response.status == 200 {
                return true;
            }
            *last_start.borrow_mut() = format!("status {}: {}", response.status, response.body);
            false
        },
        || last_start.borrow().clone(),
    );

    let stop_address = address.clone();
    let stop = std::thread::spawn(move || {
        let began = Instant::now();
        let response = request(&stop_address, "POST", "/api/v1/services/demo/stop", "", b"");
        (response, began.elapsed())
    });
    // Wait until the stop request is inside the hook. The marker appears well
    // before the hook's own timeout and before either control deadline.
    wait_until_within(
        &sandbox,
        "the stop hook never started",
        Duration::from_secs(2),
        || entered.exists(),
        String::new,
    );
    let list_address = address.clone();
    let list = std::thread::spawn(move || {
        let began = Instant::now();
        let response = request(&list_address, "GET", "/api/v1/services", "", b"");
        (response, began.elapsed())
    });

    let (stop_response, stop_elapsed) = stop.join().expect("stop request");
    let (list_response, list_elapsed) = list.join().expect("list request");
    assert_eq!(
        stop_response.status, 504,
        "a stuck supervisor is a timeout, not a busy queue: {}",
        stop_response.body
    );
    assert_eq!(list_response.status, 504, "{}", list_response.body);
    assert_eq!(stop_response.body["error"]["code"], "timeout");
    assert_eq!(list_response.body["error"]["code"], "timeout");
    for elapsed in [stop_elapsed, list_elapsed] {
        assert!(
            elapsed < Duration::from_millis(2200),
            "control_timeout_secs must release the worker before the hook returns, took {elapsed:?}"
        );
    }

    // Once the hook returns, the workers are free and normal requests resume.
    wait_until_within(
        &sandbox,
        "the daemon did not recover after the stop hook",
        Duration::from_secs(10),
        || request(&address, "GET", "/api/v1/services", "", b"").status == 200,
        String::new,
    );
}

/// On Unix the install stream job runs on the shared blocking pool. A client
/// that disconnects while `plugin.install_staged` is stuck in a post-install
/// hook must release that worker: two other management calls then both reach a
/// worker and time out at `control_timeout_secs`, instead of one queueing
/// behind the dead install until the hook returns.
#[cfg(unix)]
#[test]
fn a_disconnected_install_job_cancels_its_control_round_trip() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("control_timeout_secs = 1\n");
    let key = SigningKey::from_bytes(&[0x5b; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let marker = sandbox.root.join("install-hook-entered");
    let package = slow_hook_plugin_bundle(&key, &marker);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let mut install = TcpStream::connect(&address).expect("install connect");
    let head = format!(
        "POST /api/v1/plugins?strategy=replace HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        package.len()
    );
    install.write_all(head.as_bytes()).expect("install head");
    install.write_all(&package).expect("install body");
    install.flush().expect("install flush");
    wait_until_within(
        &sandbox,
        "the post-install hook did not start",
        Duration::from_secs(3),
        || marker.exists(),
        String::new,
    );
    install
        .shutdown(std::net::Shutdown::Both)
        .expect("install shutdown");
    drop(install);
    // The FIN arrives while the install job is still in flight; the server
    // flips the job's token and the job frees its worker on the next poll.
    // Submitting the two probes immediately makes a missing cancellation show
    // up as a queued request, which is what the bound below catches.
    let started = Instant::now();
    let mut workers = Vec::new();
    for _ in 0..2 {
        let address = address.clone();
        workers.push(std::thread::spawn(move || {
            let began = Instant::now();
            let response = request(&address, "GET", "/api/v1/services", "", b"");
            (response, began.elapsed())
        }));
    }
    for worker in workers {
        let (response, elapsed) = worker.join().expect("management request");
        assert_eq!(response.status, 504, "{}", response.body);
        assert!(
            elapsed < Duration::from_millis(1500),
            "a cancelled install must free its worker, took {elapsed:?}"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "both management calls must reach a worker"
    );
}
