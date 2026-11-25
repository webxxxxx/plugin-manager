//! lifecycle daemon tests.

use super::*;

#[test]
fn daemon_starts_a_ready_service_and_serves_live_status() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("demo", &[], Script::Ready);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );

    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["state"], "running");
    assert!(status["summary"]["pid"].as_u64().unwrap_or(0) > 0);

    let list = sandbox.run_svc(&["list"]);
    assert_eq!(list.status.code(), Some(0));
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(listed.contains("demo"), "list:\n{}", listed);
    assert!(listed.contains("running"), "list:\n{}", listed);
    assert!(
        !listed
            .lines()
            .any(|line| line.split_whitespace().next() == Some("svc")),
        "list:\n{}",
        listed
    );

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["state"], "stopped");
    assert!(status["summary"]["pid"].is_null());
}

#[test]
fn service_stdout_and_stderr_are_logged() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("demo", &[], Script::Logging);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let text = loop {
        let output = sandbox.run_svc(&["logs", "demo", "--lines", "20"]);
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if text.contains("hello-stdout") && text.contains("hello-stderr") {
            break text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "service logs did not appear: {}",
            text
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    // ADR-0022: both pipes land in the one line shape, labelled `plugin:demo`
    // and stamped before the level column.
    for needle in ["hello-stdout", "hello-stderr"] {
        let line = text
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no {needle} line in {text:?}"));
        assert!(line.contains(" INFO  plugin:demo "), "{line}");
        assert_eq!(&line[10..11], "T", "timestamp column: {line}");
        assert!(line.starts_with("20"), "timestamp column: {line}");
    }

    let svcd = sandbox.run_svc(&["logs", "svcd"]);
    assert_eq!(svcd.status.code(), Some(0));
    let svcd_text = String::from_utf8_lossy(&svcd.stdout);
    assert!(
        svcd_text.contains(" INFO  svcd starting"),
        "svcd log: {svcd_text}"
    );

    let missing = sandbox.run_svc(&["logs", "missing"]);
    assert_eq!(missing.status.code(), Some(1));
}

#[test]
fn start_pulls_dependencies_before_the_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("base", &[], Script::Ready);
    sandbox.write_service("app", &["base"], Script::Ready);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("app");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );
    assert_eq!(sandbox.status("base")["summary"]["state"], "running");
    assert_eq!(sandbox.status("app")["summary"]["state"], "running");

    assert_eq!(sandbox.stop("app").status.code(), Some(0));
    assert_eq!(sandbox.stop("base").status.code(), Some(0));
    assert_eq!(sandbox.status("app")["summary"]["state"], "stopped");
    assert_eq!(sandbox.status("base")["summary"]["state"], "stopped");

    let restarted = sandbox.run_svc(&["start", "app"]);
    assert_eq!(
        restarted.status.code(),
        Some(0),
        "restart failed: {}\n{}",
        String::from_utf8_lossy(&restarted.stderr),
        sandbox.daemon_log()
    );
    assert_eq!(sandbox.status("base")["summary"]["state"], "running");
    assert_eq!(sandbox.status("app")["summary"]["state"], "running");
}

#[test]
fn enable_disable_round_trip_and_boot_skip() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("demo", &[], Script::Ready);
    sandbox.start_daemon();

    assert_eq!(
        sandbox.start_when_ready("demo").status.code(),
        Some(0),
        "boot/start failed: {}",
        sandbox.daemon_log()
    );

    let disabled = sandbox.run_when_ready(&["disable", "demo"]);
    assert_eq!(
        disabled.status.code(),
        Some(0),
        "disable failed: {}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["enabled"], false);
    assert_eq!(status["summary"]["state"], "running");

    assert_eq!(sandbox.stop("demo").status.code(), Some(0));
    let restarted = sandbox.run_svc(&["start", "demo"]);
    assert_eq!(
        restarted.status.code(),
        Some(0),
        "explicit start of disabled service failed: {}\n{}",
        String::from_utf8_lossy(&restarted.stderr),
        sandbox.daemon_log()
    );
    assert_eq!(sandbox.status("demo")["summary"]["enabled"], false);
    assert_eq!(sandbox.stop("demo").status.code(), Some(0));

    sandbox.shutdown();
    sandbox.start_daemon();
    let _ = sandbox.run_when_ready(&["disable", "demo"]);
    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["enabled"], false);
    assert_eq!(
        status["summary"]["state"], "stopped",
        "disabled service should not boot: {}",
        status
    );

    let enabled = sandbox.run_when_ready(&["enable", "demo"]);
    assert_eq!(
        enabled.status.code(),
        Some(0),
        "enable failed: {}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    assert_eq!(sandbox.status("demo")["summary"]["enabled"], true);
}

#[test]
fn a_missing_dependency_rejects_start_with_its_id() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("app", &["missing"], Script::Ready);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("app");
    assert_eq!(
        started.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let error = String::from_utf8_lossy(&started.stderr);
    assert!(error.contains("missing dependencies"), "stderr: {}", error);
    assert!(error.contains("missing"), "stderr: {}", error);
}

#[test]
fn a_notify_one_shot_definition_never_becomes_a_startable_service() {
    // #197: accepting `remain_after_exit` on a notify-ready definition would
    // let `svc start` settle on Running with no pid while the stdio channel is
    // already gone. The definition is rejected at load, so no service exists.
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("odd");
    sandbox.set_definition_field("odd", "remain_after_exit", json!(true));

    sandbox.start_daemon();

    let started = sandbox.start_when_ready("odd");
    assert_eq!(
        started.status.code(),
        Some(1),
        "stderr: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("unknown service: odd"),
        "stderr: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(
        sandbox.daemon_log().contains("remain_after_exit"),
        "the load warning must explain the rejection:\n{}",
        sandbox.daemon_log()
    );
}

#[test]
fn a_crashing_service_enters_backoff() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("crash", &[], Script::CrashAfterReady);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("crash");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start crashed: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = sandbox.status("crash");
        let state = status["summary"]["state"].as_str().unwrap_or_default();
        if state == "backoff" || state == "failed" {
            let failures = status["details"]["failures"].as_u64().unwrap_or(0);
            assert!(failures >= 1, "status: {}", status);
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "crash never entered backoff: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn core_lifecycle_hooks_run_for_start_stop_and_status() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("marker.txt");
    let exec = sandbox.write_lifecycle_hook("exec", &marker, true);
    let start = sandbox.write_lifecycle_hook("start", &marker, false);
    let stop = sandbox.write_lifecycle_hook("stop", &marker, false);
    let status = sandbox.write_lifecycle_hook("status", &marker, false);
    sandbox.write_lifecycle_service("demo", &exec, &start, &stop, &status, &marker);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let text = fs::read_to_string(&marker).expect("marker");
    // #323: the start action prepares first, the process runs after it.
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        ["start", "exec"],
        "the start action must finish before the process exists: {text}"
    );

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let text = fs::read_to_string(&marker).expect("marker");
    assert!(text.contains("stop"), "stop hook did not run: {text}");

    let status_output = sandbox.run_svc(&["status", "demo", "--json"]);
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "status failed: {}",
        String::from_utf8_lossy(&status_output.stderr)
    );
    let value: Value = serde_json::from_slice(&status_output.stdout).expect("status json");
    assert_eq!(value["details"]["probe"], "ok");
    let text = fs::read_to_string(&marker).expect("marker");
    assert!(text.contains("status"), "status hook did not run: {text}");

    assert_eq!(sandbox.start_when_ready("demo").status.code(), Some(0));
    assert_eq!(sandbox.stop("demo").status.code(), Some(0));
    let text = fs::read_to_string(&marker).expect("marker");
    assert_eq!(
        text.lines().filter(|line| *line == "stop").count(),
        2,
        "stop hook must run again after a restart: {text}"
    );
}

/// Write a hook body next to the sandbox so it is not discovered as a service.
fn write_hook(sandbox: &Sandbox, name: &str, body: &str) -> PathBuf {
    let path = sandbox.root.join(name);
    fs::write(&path, body).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod hook");
    }
    path
}

/// A long-running process for the stop-ordering tests: it never answers a
/// protocol handshake, so the definition declares `ready: exec`.
fn write_runner(sandbox: &Sandbox, name: &str) -> PathBuf {
    #[cfg(windows)]
    let body = "@echo off\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\nwhile :; do sleep 1; done\n";
    write_hook(sandbox, name, body)
}

fn write_json_definition(sandbox: &Sandbox, id: &str, definition: &Value) {
    fs::write(
        sandbox
            .config
            .join("services.d")
            .join(format!("{}.json", id)),
        serde_json::to_vec_pretty(definition).expect("definition"),
    )
    .expect("write definition");
}

fn exec_ready(path: &Path) -> Value {
    #[cfg(windows)]
    {
        json!({
            "path": r"C:\Windows\System32\cmd.exe",
            "args": ["/c", path.to_string_lossy()],
            "ready": "exec"
        })
    }
    #[cfg(unix)]
    {
        json!({
            "path": "/bin/sh",
            "args": [path.to_string_lossy()],
            "ready": "exec"
        })
    }
}

/// #323: a start action that fails keeps the process from starting, and the
/// stop action still cleans up the attempt.
#[test]
fn a_failing_start_action_keeps_the_process_from_spawning() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("prelude-fail.txt");
    let exec = sandbox.write_lifecycle_hook("exec", &marker, true);
    #[cfg(windows)]
    let start_body = "@echo off\r\necho start>>\"%SVCD_TEST_MARKER%\"\r\nexit /b 3\r\n";
    #[cfg(unix)]
    let start_body = "#!/bin/sh\necho start >> \"$SVCD_TEST_MARKER\"\nexit 3\n";
    let start = write_hook(
        &sandbox,
        if cfg!(windows) {
            "start-fail.cmd"
        } else {
            "start-fail.sh"
        },
        start_body,
    );
    let stop = sandbox.write_hook_script("failed-stop", &marker);
    let env = json!({ "SVCD_TEST_MARKER": marker.to_string_lossy() });
    let definition = json!({
        "schema": 1,
        "id": "demo",
        "version": "1.0.0",
        "desc": "start action fails",
        "protocol": 1,
        "exec": {
            "path": exec.to_string_lossy(),
            "args": [],
            "ready": "notify",
            "env": env
        },
        "restart": "never",
        "lifecycle": {
            "start": {"path": start.to_string_lossy(), "args": [], "env": env},
            "stop": {"path": stop.to_string_lossy(), "args": []}
        }
    });
    write_json_definition(&sandbox, "demo", &definition);
    sandbox.start_daemon();

    let started = sandbox.run_svc(&["start", "demo"]);
    assert_ne!(
        started.status.code(),
        Some(0),
        "start must fail when the start action fails: {}",
        String::from_utf8_lossy(&started.stdout)
    );
    wait_until(
        "the cleanup of the failed attempt",
        Duration::from_secs(10),
        || {
            fs::read_to_string(&marker)
                .map(|text| text.contains("stop"))
                .unwrap_or(false)
        },
    );
    let text = fs::read_to_string(&marker).unwrap_or_default();
    assert!(text.contains("start"), "start action did not run: {text}");
    assert!(
        !text.contains("exec"),
        "the process started although the start action failed: {text}"
    );
    assert_ne!(sandbox.status("demo")["summary"]["state"], "running");
}

/// #323: the stop action runs after the process is gone, and `Stopped` waits
/// for it. Returning earlier would let the next start abort the cleanup.
#[test]
fn stopping_waits_for_the_stop_action() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("slow-stop.txt");
    let runner = write_runner(
        &sandbox,
        if cfg!(windows) {
            "runner.cmd"
        } else {
            "runner.sh"
        },
    );
    #[cfg(windows)]
    let stop_body = format!(
        "@echo off\r\nping -n 2 127.0.0.1 >nul\r\necho stop>>\"{}\"\r\n",
        marker.display()
    );
    #[cfg(unix)]
    let stop_body = format!(
        "#!/bin/sh\nsleep 1\necho stop >> \"{}\"\n",
        marker.display()
    );
    let stop = write_hook(
        &sandbox,
        if cfg!(windows) {
            "slow-stop.cmd"
        } else {
            "slow-stop.sh"
        },
        &stop_body,
    );
    let definition = json!({
        "schema": 1,
        "id": "demo",
        "version": "1.0.0",
        "desc": "slow cleanup",
        "protocol": 1,
        "exec": exec_ready(&runner),
        "restart": "never",
        "lifecycle": {"stop": {"path": stop.to_string_lossy(), "args": []}}
    });
    write_json_definition(&sandbox, "demo", &definition);
    sandbox.start_daemon();

    assert_eq!(
        sandbox.start_when_ready("demo").status.code(),
        Some(0),
        "start failed"
    );
    let began = Instant::now();
    let stopped = sandbox.stop("demo");
    assert_eq!(stopped.status.code(), Some(0), "stop failed");
    assert!(
        began.elapsed() >= Duration::from_millis(800),
        "stop returned after {:?}, before the stop action finished",
        began.elapsed()
    );
    let text = fs::read_to_string(&marker).unwrap_or_default();
    assert!(text.contains("stop"), "stop action did not run: {text}");
    assert_eq!(sandbox.status("demo")["summary"]["state"], "stopped");
}

/// #323: a restart composes stop-then-start and must not abort the stop action
/// halfway through.
#[test]
fn a_restart_does_not_abort_the_stop_action() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("restart-stop.txt");
    let runner = write_runner(
        &sandbox,
        if cfg!(windows) {
            "restart-runner.cmd"
        } else {
            "restart-runner.sh"
        },
    );
    #[cfg(windows)]
    let stop_body = format!(
        "@echo off\r\nping -n 2 127.0.0.1 >nul\r\necho stop>>\"{}\"\r\n",
        marker.display()
    );
    #[cfg(unix)]
    let stop_body = format!(
        "#!/bin/sh\nsleep 1\necho stop >> \"{}\"\n",
        marker.display()
    );
    let stop = write_hook(
        &sandbox,
        if cfg!(windows) {
            "restart-stop.cmd"
        } else {
            "restart-stop.sh"
        },
        &stop_body,
    );
    let definition = json!({
        "schema": 1,
        "id": "demo",
        "version": "1.0.0",
        "desc": "restart cleanup",
        "protocol": 1,
        "exec": exec_ready(&runner),
        "restart": "never",
        "lifecycle": {"stop": {"path": stop.to_string_lossy(), "args": []}}
    });
    write_json_definition(&sandbox, "demo", &definition);
    sandbox.start_daemon();

    assert_eq!(
        sandbox.start_when_ready("demo").status.code(),
        Some(0),
        "start failed"
    );
    let restarted = sandbox.run_when_ready(&["restart", "demo"]);
    assert_eq!(
        restarted.status.code(),
        Some(0),
        "restart failed: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let text = fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        text.contains("stop"),
        "the restart aborted the stop action: {text}"
    );
    assert_eq!(sandbox.status("demo")["summary"]["state"], "running");
}

/// The direct ordering proof: the stop action sees a dead process.
#[cfg(unix)]
#[test]
fn the_stop_action_runs_after_the_process_is_gone() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("after-exit.txt");
    let pidfile = sandbox.root.join("runner.pid");
    let runner = write_hook(
        &sandbox,
        "after-exit-runner.sh",
        &format!(
            "#!/bin/sh\necho $$ > \"{}\"\nwhile :; do sleep 1; done\n",
            pidfile.display()
        ),
    );
    let stop = write_hook(
        &sandbox,
        "after-exit-stop.sh",
        &format!(
            "#!/bin/sh\nif kill -0 \"$(cat \\\"{}\\\")\" 2>/dev/null; then echo alive; else echo gone; fi > \"{}\"\n",
            pidfile.display(),
            marker.display()
        ),
    );
    let definition = json!({
        "schema": 1,
        "id": "demo",
        "version": "1.0.0",
        "desc": "stop after exit",
        "protocol": 1,
        "exec": exec_ready(&runner),
        "restart": "never",
        "lifecycle": {"stop": {"path": stop.to_string_lossy(), "args": []}}
    });
    write_json_definition(&sandbox, "demo", &definition);
    sandbox.start_daemon();

    assert_eq!(
        sandbox.start_when_ready("demo").status.code(),
        Some(0),
        "start failed"
    );
    wait_until("the runner pid file", Duration::from_secs(10), || {
        pidfile.is_file()
    });
    assert_eq!(sandbox.stop("demo").status.code(), Some(0), "stop failed");
    let text = fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        text.trim(),
        "gone",
        "the stop action saw a live process: {text}"
    );
}

#[test]
fn a_drop_in_script_is_discovered_and_supervised() {
    let mut sandbox = Sandbox::new();
    let script = sandbox.write_script_service("demo");
    assert!(script.is_file());
    sandbox.start_daemon_ready();

    let list = sandbox.run_svc(&["list"]);
    assert!(
        String::from_utf8_lossy(&list.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some("demo")),
        "script service missing from list: {}",
        String::from_utf8_lossy(&list.stdout)
    );

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let marker = sandbox.config.join("services.d").join("marker.txt");
    wait_for_marker(&marker, "start");

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    wait_for_marker(&marker, "stop");

    let status = sandbox.run_svc(&["status", "demo", "--json"]);
    assert_eq!(
        status.status.code(),
        Some(0),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let value: Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert_eq!(value["details"]["probe"], "ok");
    wait_for_marker(&marker, "status");
}

#[test]
fn a_script_without_actions_runs_from_the_top() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    let body = "@echo off\r\necho top>>\"%~dp0top.txt\"\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho top >> \"$(dirname \"$0\")/top.txt\"\nwhile :; do sleep 1; done\n";
    sandbox.write_script_body("demo", body);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let marker = sandbox.config.join("services.d").join("top.txt");
    wait_for_marker(&marker, "top");

    let status = sandbox.status("demo");
    assert!(status["details"]["probe"].is_null(), "{status}");

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(sandbox.status("demo")["summary"]["state"], "stopped");
}

#[test]
fn a_start_only_script_is_stopped_by_killing_it() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    let body = "@echo off\r\nif /I \"%1\"==\"start\" goto start\r\nexit /b 64\r\n:start\r\necho start>>\"%~dp0start.txt\"\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\ncase \"$1\" in\n  start)\n    echo start >> \"$(dirname \"$0\")/start.txt\"\n    while :; do sleep 1; done\n    ;;\n  *) exit 64 ;;\nesac\n";
    sandbox.write_script_body("demo", body);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let marker = sandbox.config.join("services.d").join("start.txt");
    wait_for_marker(&marker, "start");

    let began = Instant::now();
    let stopped = sandbox.stop("demo");

    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(sandbox.status("demo")["summary"]["state"], "stopped");
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "a script without a stop action took {:?} to stop",
        began.elapsed()
    );
    assert!(!sandbox.config.join("services.d").join("stop.txt").exists());
}

#[test]
fn status_output_becomes_supplementary_detail() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    let body = "@echo off\r\nif /I \"%1\"==\"start\" goto start\r\nif /I \"%1\"==\"status\" goto status\r\nexit /b 64\r\n:start\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n:status\r\necho clock detail\r\nexit /b 0\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\ncase \"$1\" in\n  start)\n    while :; do sleep 1; done\n    ;;\n  status)\n    echo clock detail\n    ;;\n  *) exit 64 ;;\nesac\n";
    sandbox.write_script_body("demo", body);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let status = sandbox.status("demo");

    assert_eq!(status["details"]["probe"], "clock detail");
}

#[test]
fn a_stop_action_still_runs_when_start_is_not_declared() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    let body = "@echo off\r\nif /I \"%1\"==\"stop\" goto stop\r\necho top>>\"%~dp0top.txt\"\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n:stop\r\necho stop>>\"%~dp0stop.txt\"\r\nexit /b 0\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\ncase \"$1\" in\n  stop)\n    echo stop >> \"$(dirname \"$0\")/stop.txt\"\n    ;;\n  *)\n    echo top >> \"$(dirname \"$0\")/top.txt\"\n    while :; do sleep 1; done\n    ;;\nesac\n";
    sandbox.write_script_body("demo", body);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let top = sandbox.config.join("services.d").join("top.txt");
    wait_for_marker(&top, "top");

    let stopped = sandbox.stop("demo");

    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    wait_for_marker(&sandbox.config.join("services.d").join("stop.txt"), "stop");
    assert_eq!(sandbox.status("demo")["summary"]["state"], "stopped");
}

#[test]
fn a_background_child_keeps_the_service_running_and_stops_with_it() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    {
        let body = r#"Remove-Item Alias:start -Force -ErrorAction SilentlyContinue
function start {
  $p = Start-Process -WindowStyle Hidden -PassThru -FilePath 'ping' -ArgumentList '-n','60','127.0.0.1'
  Set-Content -Path (Join-Path $PSScriptRoot 'child.pid') -Value $p.Id
}
function status {
  'child alive'
}
$action = $args[0]
if ($action -eq 'start') { start } elseif ($action -eq 'status') { status }
"#;
        sandbox.write_script_as("demo.ps1", body);
    }
    #[cfg(unix)]
    {
        let body = "#!/bin/sh\nstart() {\n  sleep 60 &\n  echo $! > \"$(dirname \"$0\")/child.pid\"\n}\nstatus() { echo 'child alive'; }\ncase \"$1\" in\n  start) start ;;\n  status) status ;;\n  *) exit 64 ;;\nesac\n";
        sandbox.write_script_body("demo", body);
    }
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let pid_file = sandbox.config.join("services.d").join("child.pid");
    wait_until("the background child pid", Duration::from_secs(10), || {
        fs::read_to_string(&pid_file)
            .map(|text| text.trim().parse::<u32>().is_ok())
            .unwrap_or(false)
    });
    let child_pid: u32 = fs::read_to_string(&pid_file)
        .expect("child pid file")
        .trim()
        .parse()
        .expect("child pid");

    let child = ProcessInstance::capture(child_pid);
    let leader = ProcessInstance::capture(
        sandbox.status("demo")["summary"]["pid"]
            .as_u64()
            .expect("leader pid") as u32,
    );
    wait_until("the script leader to exit", Duration::from_secs(10), || {
        !leader.is_alive()
    });

    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["state"], "running", "{status}");
    assert_eq!(status["details"]["probe"], "child alive");

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(sandbox.status("demo")["summary"]["state"], "stopped");
    wait_until(
        "the background child to die with the service",
        Duration::from_secs(10),
        || !child.is_alive(),
    );
}

#[test]
fn state_matrix_for_core_json_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.start_daemon_ready();
    assert_state_matrix(&sandbox, "core", "core", true);
}

#[test]
fn state_matrix_for_script_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_stop_script("script");
    sandbox.start_daemon_ready();
    assert_state_matrix(&sandbox, "script", "core", true);
}

#[test]
fn state_matrix_for_plugin_service() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);
    // A fresh install is disabled; opt in to autostart so the matrix matches
    // the enabled core services (the install default has its own test).
    assert_eq!(
        sandbox
            .run_when_ready_plugin(&["enable", "plug"])
            .status
            .code(),
        Some(0)
    );
    assert_state_matrix(&sandbox, "plug", "plugin", false);
}

#[test]
fn crash_restart_for_core_json_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.start_daemon();
    assert_crash_restart(&sandbox, "core");
}

#[test]
fn crash_restart_for_script_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_stop_script("script");
    sandbox.start_daemon();
    assert_crash_restart(&sandbox, "script");
}

#[test]
fn crash_restart_for_plugin_service() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);
    assert_crash_restart(&sandbox, "plug");
}

#[test]
fn five_quick_crashes_enter_failed_and_explicit_start_resets() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("crashy");
    sandbox.start_daemon();

    let mut instance = ProcessInstance::capture(sandbox.running_pid("crashy"));
    for attempt in 1..=5 {
        kill_pid(instance.pid);
        if attempt < 5 {
            let restarted = sandbox.wait_for_restart("crashy", instance, Duration::from_secs(20));
            instance =
                ProcessInstance::capture(restarted["summary"]["pid"].as_u64().expect("pid") as u32);
        }
    }

    let failed = sandbox.wait_for_state("crashy", "failed", Duration::from_secs(25));
    assert_eq!(failed["details"]["failures"], 5, "{failed}");
    assert!(failed["summary"]["pid"].is_null(), "{failed}");

    // Only an explicit start leaves Failed.
    let running = sandbox.start_service("crashy");
    assert_eq!(running["details"]["failures"], 0, "{running}");
    sandbox.stop_service("crashy");
}

#[test]
fn disable_keeps_the_crash_restart_policy() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("off");
    sandbox.start_daemon();
    assert_eq!(
        sandbox.run_when_ready(&["disable", "off"]).status.code(),
        Some(0)
    );

    let instance = ProcessInstance::capture(sandbox.running_pid("off"));
    kill_pid(instance.pid);
    let backoff = sandbox.wait_for_state("off", "backoff", Duration::from_secs(5));
    assert!(
        backoff["details"]["failures"].as_u64().unwrap_or(0) >= 1,
        "{backoff}"
    );

    // enabled only gates the boot start; the default on-failure policy still
    // brings the process back.
    let restarted = sandbox.wait_for_restart("off", instance, Duration::from_secs(20));
    assert_eq!(restarted["summary"]["enabled"], false, "{restarted}");
    sandbox.stop_service("off");
}

#[test]
fn a_never_policy_service_stops_without_restarting() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("once");
    sandbox.set_restart("once", "never");
    sandbox.start_daemon_ready();

    let running = sandbox.wait_for_state("once", "running", Duration::from_secs(15));
    assert_eq!(running["details"]["restart"], "never", "{running}");
    let instance =
        ProcessInstance::capture(running["summary"]["pid"].as_u64().expect("pid") as u32);
    kill_pid(instance.pid);

    let stopped = sandbox.wait_for_state("once", "stopped", Duration::from_secs(10));
    assert!(stopped["summary"]["pid"].is_null(), "{stopped}");
    assert_eq!(stopped["details"]["failures"], 0, "{stopped}");
    assert!(!instance.is_alive(), "{instance:?} is still alive");
}

#[test]
fn graceful_svcd_restart_boots_enabled_services_and_skips_disabled() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.write_stop_script("script");
    sandbox.write_core_json("off");
    fs::write(sandbox.config.join("services.d").join("off.disabled"), b"")
        .expect("disabled marker");

    sandbox.start_daemon_ready();
    let package = sandbox.write_plugin_package("plug");
    sandbox.install_plugin("plug", &package);
    // Fresh installs are disabled; boot-start requires the opt-in.
    assert_eq!(
        sandbox
            .run_when_ready_plugin(&["enable", "plug"])
            .status
            .code(),
        Some(0)
    );

    let mut instances = Vec::new();
    for id in ["core", "script", "plug"] {
        instances.push(ProcessInstance::capture(sandbox.running_pid(id)));
    }

    sandbox.shutdown();
    for instance in &instances {
        wait_until(
            &format!("instance {:?} to exit", instance),
            Duration::from_secs(10),
            || !instance.is_alive(),
        );
    }

    sandbox.start_daemon_ready();
    for id in ["core", "script", "plug"] {
        let status = sandbox.wait_for_state(id, "running", Duration::from_secs(15));
        assert!(
            status["summary"]["pid"].as_u64().unwrap_or(0) > 0,
            "{status}"
        );
    }
    let off = sandbox.status("off");
    assert_eq!(off["summary"]["state"], "stopped", "{off}");
    assert_eq!(off["summary"]["enabled"], false, "{off}");
}

#[test]
fn builtin_svc_stays_running_and_ignores_start_stop() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_ready();

    let status = sandbox.status("svc");
    assert_eq!(status["summary"]["state"], "running", "{status}");
    assert_eq!(status["summary"]["kind"], "core", "{status}");
    assert_eq!(status["summary"]["enabled"], true, "{status}");

    assert_eq!(
        sandbox.run_when_ready(&["start", "svc"]).status.code(),
        Some(0)
    );
    assert_eq!(
        sandbox.run_when_ready(&["stop", "svc"]).status.code(),
        Some(0)
    );
    assert_eq!(sandbox.status("svc")["summary"]["state"], "running");
}

#[test]
fn a_one_shot_service_stays_running_after_it_finishes() {
    let mut sandbox = Sandbox::new();
    sandbox.write_one_shot("once");
    sandbox.start_daemon_ready();

    // Boot starts it. Once the process exits 0 the service stays running with
    // no pid instead of flipping to stopped.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = sandbox.status("once");
        if status["summary"]["state"] == "running"
            && status["summary"]["pid"].is_null()
            && status["details"]["last_exit"] == json!(0)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "one-shot never settled into running/pid-null: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // Starting again is allowed and returns to the same finished state.
    let again = sandbox.start_when_ready("once");
    assert_eq!(
        again.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = sandbox.status("once");
        if status["summary"]["state"] == "running" && status["summary"]["pid"].is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the re-run never settled into running/pid-null: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // Stop only runs the stop action (none here) and settles immediately.
    let stopped = sandbox.stop_service("once");
    assert_eq!(stopped["summary"]["state"], "stopped", "{stopped}");
    assert!(stopped["summary"]["pid"].is_null(), "{stopped}");
    assert_ne!(stopped["summary"]["state"], "failed");
}

#[test]
fn a_finished_one_shot_dependency_is_not_rerun() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.config.join("services.d").join("once-marker.txt");
    #[cfg(windows)]
    let body = "@echo off\r\necho once>>\"%~dp0once-marker.txt\"\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho once >> \"$(dirname \"$0\")/once-marker.txt\"\n";
    sandbox.write_script_body("once", body);
    sandbox.write_service("app", &["once"], Script::Ready);

    sandbox.start_daemon_ready();
    // The one-shot may be observed in its brief Running-with-pid window right
    // after spawn; the point of this test is that it settles to
    // running/pid-null exactly once before the dependent app starts.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = sandbox.status("once");
        if status["summary"]["state"] == "running" && status["summary"]["pid"].is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the one-shot never settled to running/pid-null: {status}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    sandbox.wait_for_state("app", "running", Duration::from_secs(15));
    wait_until(
        "the one-shot body to run once",
        Duration::from_secs(5),
        || {
            fs::read_to_string(&marker)
                .map(|text| text.lines().count() == 1)
                .unwrap_or(false)
        },
    );

    // Starting the dependent again must not re-execute the finished one-shot.
    sandbox.stop_service("app");
    sandbox.start_service("app");
    std::thread::sleep(Duration::from_millis(200));
    let runs = fs::read_to_string(&marker).expect("marker").lines().count();
    assert_eq!(runs, 1, "the one-shot dependency ran again");
    sandbox.stop_service("app");
}

#[test]
fn restart_reruns_a_finished_one_shot() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.config.join("services.d").join("once-marker.txt");
    #[cfg(windows)]
    let body = "@echo off\r\necho once>>\"%~dp0once-marker.txt\"\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho once >> \"$(dirname \"$0\")/once-marker.txt\"\n";
    sandbox.write_script_body("once", body);

    sandbox.start_daemon();
    wait_until("the first run", Duration::from_secs(15), || {
        let output = sandbox.run_svc(&["status", "once", "--json"]);
        if output.status.code() != Some(0) {
            return false;
        }
        let settled = serde_json::from_slice::<Value>(&output.stdout)
            .map(|value| value["summary"]["pid"].is_null())
            .unwrap_or(false);
        settled
            && fs::read_to_string(&marker)
                .map(|text| text.lines().count() == 1)
                .unwrap_or(false)
    });

    let output = sandbox.run_when_ready(&["restart", "once"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until("the re-run", Duration::from_secs(15), || {
        let output = sandbox.run_svc(&["status", "once", "--json"]);
        if output.status.code() != Some(0) {
            return false;
        }
        let settled = serde_json::from_slice::<Value>(&output.stdout)
            .map(|value| value["summary"]["pid"].is_null())
            .unwrap_or(false);
        settled
            && fs::read_to_string(&marker)
                .map(|text| text.lines().count() == 2)
                .unwrap_or(false)
    });
    let status = sandbox.status("once");
    assert_eq!(status["summary"]["state"], "running", "{status}");
    assert!(status["summary"]["pid"].is_null(), "{status}");
}

#[test]
fn a_finished_one_shot_is_not_rerun_after_a_daemon_restart() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.config.join("services.d").join("once-marker.txt");
    #[cfg(windows)]
    let body = "@echo off\r\necho once>>\"%~dp0once-marker.txt\"\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho once >> \"$(dirname \"$0\")/once-marker.txt\"\n";
    sandbox.write_script_body("once", body);

    sandbox.start_daemon_ready();
    let record = sandbox.data.join("run").join("once.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = sandbox.status("once");
        let recorded_done = fs::read_to_string(&record)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|value| value["state"] == "running" && value["pid"].is_null())
            .unwrap_or(false);
        if status["summary"]["state"] == "running"
            && status["summary"]["pid"].is_null()
            && recorded_done
            && fs::read_to_string(&marker)
                .map(|text| text.lines().count() == 1)
                .unwrap_or(false)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "one-shot never finished: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    sandbox.force_kill_daemon();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    let status = sandbox.wait_for_state("once", "running", Duration::from_secs(10));
    assert!(status["summary"]["pid"].is_null(), "{status}");
    std::thread::sleep(Duration::from_millis(200));
    let runs = fs::read_to_string(&marker).expect("marker").lines().count();
    assert_eq!(runs, 1, "the finished one-shot was executed again");
}

#[test]
fn stopping_a_finished_one_shot_runs_its_stop_action() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("once-stop.txt");
    #[cfg(windows)]
    let hook = sandbox.root.join("once-stop.cmd");
    #[cfg(unix)]
    let hook = sandbox.root.join("once-stop.sh");
    #[cfg(windows)]
    fs::write(&hook, "@echo off\r\necho stop>>\"%~dp0once-stop.txt\"\r\n").expect("stop hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            &hook,
            format!("#!/bin/sh\necho stop >> \"{}\"\n", marker.display()),
        )
        .expect("stop hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod stop hook");
    }
    let (path, args) = if cfg!(windows) {
        (r"C:\Windows\System32\cmd.exe", json!(["/c", "exit", "0"]))
    } else {
        ("/bin/true", json!([]))
    };
    let definition = json!({
        "schema": 1,
        "id": "once",
        "version": "1.0.0",
        "desc": "one shot with cleanup",
        "protocol": 1,
        "exec": {"path": path, "args": args, "ready": "exec"},
        "restart": "never",
        "remain_after_exit": true,
        "lifecycle": {"stop": {"path": hook.to_string_lossy(), "args": []}}
    });
    fs::write(
        sandbox.config.join("services.d").join("once.json"),
        serde_json::to_vec_pretty(&definition).expect("definition"),
    )
    .expect("write definition");

    sandbox.start_daemon_ready();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = sandbox.status("once");
        if status["summary"]["state"] == "running" && status["summary"]["pid"].is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "one-shot never finished: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let stopped = sandbox.stop_service("once");
    assert_eq!(stopped["summary"]["state"], "stopped", "{stopped}");
    wait_until("the stop action", Duration::from_secs(10), || {
        fs::read_to_string(&marker)
            .map(|text| text.contains("stop"))
            .unwrap_or(false)
    });
}

#[test]
fn restart_runs_stop_then_start_and_changes_the_pid() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.start_daemon();
    let first = ProcessInstance::capture(sandbox.running_pid("core"));

    let output = sandbox.run_when_ready(&["restart", "core"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let status = sandbox.wait_for_state("core", "running", Duration::from_secs(15));
    let second = ProcessInstance::capture(status["summary"]["pid"].as_u64().expect("pid") as u32);
    // `restart` must replace the process. Pid inequality alone is the wrong
    // check: the OS may hand the old pid to the new process, and an unrelated
    // process may hold the old pid without being the old instance (#155).
    assert!(
        first.is_replaced_by(second),
        "restart kept the pre-crash instance: {status}"
    );
    // The old process exits asynchronously after `restart` returns. Asserting
    // `!pid_alive` immediately made a parallel full run red while the process
    // was still winding down (#153); wait for the observable end state instead.
    wait_until(
        "the pre-restart instance to exit",
        Duration::from_secs(10),
        || !first.is_alive(),
    );
    sandbox.stop_service("core");
}

/// A failed attempt may have created half its resources; the stop action must
/// run before the retry (shell `svcd` does `run_stop` before the retry sleep).
#[test]
fn a_failed_attempt_runs_the_stop_action_before_retrying() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("fail-stop.txt");
    let hook = sandbox.write_hook_script("fail-stop", &marker);
    let exec = sandbox.write_crash_after_ready_script("failer");
    let definition = json!({
        "schema": 1,
        "id": "failer",
        "version": "1.0.0",
        "desc": "crashes after ready",
        "protocol": 1,
        "exec": exec_for(&exec),
        "restart": "on-failure",
        "lifecycle": {"stop": {"path": hook.to_string_lossy(), "args": []}}
    });
    fs::write(
        sandbox.config.join("services.d").join("failer.json"),
        serde_json::to_vec_pretty(&definition).expect("definition"),
    )
    .expect("write definition");

    sandbox.start_daemon();
    wait_until("the retry cleanup", Duration::from_secs(15), || {
        fs::read_to_string(&marker)
            .map(|text| text.contains("stop"))
            .unwrap_or(false)
    });
}

/// A record caught in `stopping` is finished with the service's stop action,
/// not just force-killed.
#[cfg(unix)]
#[test]
fn a_stopping_record_is_finished_with_the_stop_action() {
    use std::os::unix::process::CommandExt;

    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("half-stop.txt");
    let hook = sandbox.write_hook_script("half-stop", &marker);
    let definition = json!({
        "schema": 1,
        "id": "half",
        "version": "1.0.0",
        "desc": "interrupted stop",
        "protocol": 1,
        "exec": {"path": "/bin/true", "args": [], "ready": "exec"},
        "restart": "never",
        "lifecycle": {"stop": {"path": hook.to_string_lossy(), "args": []}}
    });
    fs::write(
        sandbox.config.join("services.d").join("half.json"),
        serde_json::to_vec_pretty(&definition).expect("definition"),
    )
    .expect("write definition");
    fs::write(sandbox.config.join("services.d").join("half.disabled"), b"")
        .expect("disable at boot");

    let mut child = Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .expect("spawn leftover");
    let pid = child.id();
    let leftover = ProcessInstance::capture(pid);
    // The record stores the same field 22 that the instance identity captured.
    let starttime: u64 = leftover.start.expect("starttime");
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("boot id")
        .trim()
        .to_string();
    let run_dir = sandbox.data.join("run");
    fs::create_dir_all(&run_dir).expect("run dir");
    fs::write(
        run_dir.join("half.json"),
        serde_json::to_vec(&json!({
            "boot": boot,
            "state": "stopping",
            "pid": pid,
            "starttime": starttime,
            "since": 1u64,
        }))
        .expect("record"),
    )
    .expect("write record");

    sandbox.start_daemon();
    wait_until(
        "the stop action for the interrupted stop",
        Duration::from_secs(15),
        || {
            fs::read_to_string(&marker)
                .map(|text| text.contains("stop"))
                .unwrap_or(false)
        },
    );
    wait_until(
        "the leftover process to go away",
        Duration::from_secs(10),
        || !leftover.is_alive(),
    );
    let _ = child.wait();
}

#[test]
fn a_finished_one_shot_is_rerun_after_a_graceful_restart() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.config.join("services.d").join("once-marker.txt");
    #[cfg(windows)]
    let body = "@echo off\r\necho once>>\"%~dp0once-marker.txt\"\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho once >> \"$(dirname \"$0\")/once-marker.txt\"\n";
    sandbox.write_script_body("once", body);

    sandbox.start_daemon();
    wait_until("the one-shot to run", Duration::from_secs(15), || {
        fs::read_to_string(&marker)
            .map(|text| text.lines().count() >= 1)
            .unwrap_or(false)
    });
    sandbox.shutdown();
    sandbox.start_daemon();
    wait_until(
        "the one-shot to run again after restart",
        Duration::from_secs(15),
        || {
            fs::read_to_string(&marker)
                .map(|text| text.lines().count() >= 2)
                .unwrap_or(false)
        },
    );
}

/// The live text view carries both the summary and the details block.
#[test]
fn status_text_shows_summary_and_details() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.start_daemon();

    let output = sandbox.run_when_ready(&["status", "core"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "summary:",
        "details:",
        "id:",
        "kind:",
        "state:",
        "dependencies:",
        "failures:",
        "restart:",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }
    assert!(text.contains("core"), "service id missing:\n{text}");
    // The daemon starts an enabled core service, so the state is `running`
    // here; only its presence matters to the text layout.
    assert!(
        text.contains("running") || text.contains("stopped"),
        "state missing:\n{text}"
    );
}
