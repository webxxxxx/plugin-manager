#![cfg(test)]

use super::calls::{test_reply_sink, CallError, PendingCall};
use std::collections::BTreeMap;

use crate::kernel::contract::Wire;
use crate::kernel::failure::Class;
use crate::kernel::registry::{Hook, Interpreter};
use std::sync::{Arc, Mutex};

use super::*;
use std::time::{Duration, Instant};

fn exec() -> Exec {
    Exec {
        path: "/bin/true".into(),
        args: Vec::new(),
        env: BTreeMap::new(),
        ready: Ready::Notify,
        interpreter: None,
    }
}

/// Every slot test owns its tree; nothing here reads or writes the
/// process environment.
fn test_paths() -> Arc<Paths> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = crate::platform::test_support::temp_dir().join(format!(
        "svcd-slot-unit-{}-{}",
        std::process::id(),
        nanos
    ));
    let paths = Paths::under(&root);
    std::fs::create_dir_all(&paths.logs_dir).expect("logs");
    Arc::new(paths)
}

fn test_limits() -> Arc<crate::platform::limits::Limits> {
    crate::platform::test_support::limits()
}

fn slot() -> Slot {
    Slot::new(
        "demo".into(),
        exec(),
        None,
        Restart::OnFailure,
        test_paths(),
        test_limits(),
    )
}

#[test]
fn a_port_probe_settles_ready_when_the_address_accepts() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let addr = listener.local_addr().expect("addr");
    let mut slot = slot();
    let pid = 4242;
    slot.machine.state = State::Starting;
    slot.machine.pid = Some(pid);
    slot.start_port_probe(pid, addr);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut now = 0u64;
    loop {
        now += 20;
        slot.pump(now);
        if slot.machine.state == State::Running {
            break;
        }
        assert!(Instant::now() < deadline, "the probe never settled a Ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(slot.port.is_none(), "a settled probe is dropped");
}

#[test]
fn a_port_probe_keeps_waiting_while_nothing_accepts() {
    // The listener is dropped before the probe starts: the address refuses
    // connections, so the instance must not be reported ready.
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        listener.local_addr().expect("addr")
    };
    let mut slot = slot();
    slot.machine.state = State::Starting;
    slot.machine.pid = Some(7);
    slot.start_port_probe(7, addr);

    for now in (20..600).step_by(20) {
        slot.pump(now);
    }

    assert_eq!(slot.machine.state, State::Starting);
    assert!(slot.port.is_some(), "the probe is still waiting");
}

#[test]
fn notify_and_raw_transport_services_are_channel_peers() {
    // #183: a channel peer must stay alive for callers; a clean exit is
    // death, not completion. Protocol 1/2 exec services own no channel.
    let mut slot = slot();
    assert!(
        slot.is_channel_peer(),
        "a notify service must stay reachable"
    );

    let mut fire_and_forget = exec();
    fire_and_forget.ready = Ready::Exec;
    slot.set_exec(fire_and_forget);
    assert!(
        !slot.is_channel_peer(),
        "exec protocol 1 is fire-and-forget"
    );

    slot.set_protocol(3);
    assert!(
        slot.is_channel_peer(),
        "protocol 3 carries the raw transport"
    );
}

fn queue(slot: &mut Slot, id: u64, deadline: u64) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    slot.pending.insert(
        id,
        PendingCall {
            capability: "echo".into(),
            deadline,
            progress_token: None,
            trace: None,
            client: Some(crate::wire::control::channel::Reply::plain(tx.clone())),
            sink: test_reply_sink(
                crate::wire::control::channel::Reply::plain(tx),
                Some(json!(100 + id)),
            ),
        },
    );
    rx
}

#[test]
fn the_slot_handle_reports_degraded_health() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    slot.machine.mark_degraded("last call failed");
    slot.pump(0);
    assert_eq!(slot.handle().probe(), Settled::Degraded("degraded".into()));
}

#[test]
fn an_interpreter_wraps_the_script_and_its_arguments() {
    let exec = Exec {
        path: "clock.sh".into(),
        args: vec!["start".into()],
        env: BTreeMap::new(),
        ready: Ready::Exec,
        interpreter: Some(Interpreter {
            path: "/usr/bin/sh".into(),
            args: vec!["-e".into()],
        }),
    };

    let command = command_for(&exec);
    let args: Vec<String> = command
        .get_args()
        .map(|argument| argument.to_string_lossy().to_string())
        .collect();

    assert_eq!(command.get_program().to_string_lossy(), "/usr/bin/sh");
    assert_eq!(args, vec!["-e", "clock.sh", "start"]);
}

#[test]
fn a_hook_with_an_interpreter_keeps_the_action_environment() {
    let hook = Hook {
        path: "actions.sh".into(),
        args: vec!["stop".into()],
        env: BTreeMap::new(),
        timeout_ms: 1_000,
        interpreter: Some(Interpreter {
            path: "/usr/bin/bash".into(),
            args: Vec::new(),
        }),
    };

    let command = hook_command("demo", "stop", &hook, &test_paths(), None);
    let args: Vec<String> = command
        .get_args()
        .map(|argument| argument.to_string_lossy().to_string())
        .collect();

    assert_eq!(command.get_program().to_string_lossy(), "/usr/bin/bash");
    assert_eq!(args, vec!["actions.sh", "stop"]);
    assert!(command.get_envs().any(|(key, value)| {
        key.to_string_lossy() == "SVCD_ACTION"
            && value
                .map(|value| value.to_string_lossy() == "stop")
                .unwrap_or(false)
    }));
    // A core service is the operator's own code: the plugin-only directory is
    // not invented for it.
    assert!(!command
        .get_envs()
        .any(|(key, _)| key.to_string_lossy() == "SVCD_PLUGIN_DIR"));
}

/// #312: a plugin is third-party code. It learns where to put its own files and
/// nothing about the daemon's config (password hash, `trust.d`) or data tree.
#[test]
fn a_plugin_child_learns_only_where_to_put_its_files() {
    let hook = Hook {
        path: "actions.sh".into(),
        args: vec!["stop".into()],
        env: BTreeMap::new(),
        timeout_ms: 1_000,
        interpreter: None,
    };
    let paths = test_paths();
    let plugin_dir = std::path::Path::new("/mnt/sd/plugins/demo");

    let command = hook_command("demo", "stop", &hook, &paths, Some(plugin_dir));
    let env: BTreeMap<String, String> = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                (
                    key.to_string_lossy().to_string(),
                    value.to_string_lossy().to_string(),
                )
            })
        })
        .collect();
    // The daemon's own environment carries these; a plugin child must not
    // inherit them, so they are removed rather than merely left unset.
    let mut removed: Vec<String> = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| key.to_string_lossy().to_string())
        .collect();
    removed.sort();
    assert_eq!(
        removed,
        vec![
            "SVCD_ACTION",
            "SVCD_CONFIG_DIR",
            "SVCD_DATA_DIR",
            "SVCD_SERVICE_ID"
        ]
    );

    let keys: Vec<&str> = env.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["SVCD_LOG_DIR", "SVCD_PLUGIN_DIR", "SVCD_TEMP_DIR"],
        "a plugin gets exactly the directories it places files in"
    );
    assert_eq!(
        env["SVCD_PLUGIN_DIR"],
        plugin_dir.to_string_lossy(),
        "the directory is the one the package was installed into"
    );
    assert_eq!(env["SVCD_LOG_DIR"], paths.logs_dir.to_string_lossy());
    assert_eq!(env["SVCD_TEMP_DIR"], paths.temp_dir.to_string_lossy());
}

#[test]
fn notifications_are_forwarded_to_the_face_sink() {
    let (tx, rx) = mpsc::channel();
    let mut slot = Slot::new(
        "slot-ui-test".into(),
        exec(),
        None,
        Restart::OnFailure,
        test_paths(),
        test_limits(),
    );
    slot.set_notifier(Arc::new(move |method: &str, params: &Value| {
        let _ = tx.send((method.to_string(), params.clone()));
    }));
    slot.handle_notification(
        "notifications/message",
        Some(&json!({"level": "info", "data": "hello"})),
    );
    let (method, params) = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("notification");
    assert_eq!(method, "notifications/message");
    assert_eq!(params["data"], "hello");
}

#[test]
fn in_flight_calls_are_capped_at_sixty_four() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    for id in 0..MAX_IN_FLIGHT as u64 {
        let _ = queue(&mut slot, id + 1, u64::MAX);
    }
    let (tx, _rx) = mpsc::channel();

    let result = slot.request_capability(
        "echo",
        json!({}),
        Some(json!(500)),
        0,
        None,
        crate::wire::control::channel::Reply::plain(tx),
    );

    assert!(matches!(
        result,
        Err(CallError::Failed(failure)) if failure.class == Class::Busy
    ));
}

#[test]
fn a_full_call_queue_reports_busy() {
    let slot = slot();
    let handle = slot.handle();
    for _ in 0..CALL_QUEUE {
        handle
            .submit(
                Invocation::new("echo", json!({}), Duration::from_secs(1)),
                None,
                Box::new(|_: Result<Reply, Failure>| {}),
            )
            .unwrap_or_else(|_| panic!("the queue must accept a call"));
    }
    let (failure, _done) = match handle.submit(
        Invocation::new("echo", json!({}), Duration::from_secs(1)),
        None,
        Box::new(|_: Result<Reply, Failure>| {}),
    ) {
        Ok(()) => panic!("a full queue must report backpressure"),
        Err(pair) => pair,
    };
    assert_eq!(failure.class, Class::Busy);
}

#[test]
fn a_timed_out_completion_sink_is_called() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    let (tx, rx) = mpsc::channel();
    slot.pending.insert(
        7,
        PendingCall {
            capability: "echo".into(),
            deadline: 10,
            progress_token: None,
            trace: Some("7".into()),
            client: None,
            sink: Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        },
    );

    slot.expire_pending(10);

    match rx.try_recv().expect("completion") {
        Err(failure) => assert_eq!(failure.class, Class::Timeout),
        Ok(reply) => panic!("a timed out call must fail, got {reply:?}"),
    }
}

#[test]
fn dropping_a_slot_fails_a_queued_call() {
    let slot = slot();
    let handle = slot.handle();
    let (tx, rx) = mpsc::channel();
    handle
        .submit(
            Invocation::new("echo", json!({}), Duration::from_secs(1)),
            None,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        )
        .unwrap_or_else(|_| panic!("the queue must accept a call"));

    drop(slot);

    match rx.try_recv().expect("completion") {
        Err(failure) => assert_eq!(failure.class, Class::NotRunning),
        Ok(reply) => panic!("a dropped slot must not report success: {reply:?}"),
    }
}

#[test]
fn a_timed_out_call_sends_a_cancellation_then_a_tool_error() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    let rx = queue(&mut slot, 7, 10);

    slot.expire_pending(10);

    let line = rx.try_recv().expect("timeout response");
    let response: rpc::Response = serde_json::from_str(&line).expect("json");
    assert_eq!(response.id, Some(json!(107)));
    assert!(response.error.is_none());
    let result = response.result.expect("tool result");
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"]
        .as_str()
        .expect("text")
        .contains("timed out"));
    assert!(slot.pending.is_empty());
}

#[test]
fn a_strict_plugin_rejects_non_protocol_stdout() {
    let mut slot = slot();
    slot.set_protocol_only(true);
    slot.machine.state = State::Running;
    let rx = queue(&mut slot, 5, u64::MAX);

    slot.handle_frame("hello from stdout", 0);

    let line = rx.try_recv().expect("failure response");
    let response: rpc::Response = serde_json::from_str(&line).expect("json");
    let result = response.result.expect("tool result");
    assert_eq!(result["isError"], true);
    assert!(slot.pending.is_empty());
}

#[test]
fn a_protocol_violation_fails_pending_calls() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    let rx = queue(&mut slot, 5, u64::MAX);

    slot.protocol_violation("frame exceeds 1 MiB");

    let line = rx.try_recv().expect("failure response");
    let response: rpc::Response = serde_json::from_str(&line).expect("json");
    let result = response.result.expect("tool result");
    assert_eq!(result["isError"], true);
    assert!(slot.pending.is_empty());
}

#[test]
fn a_crashed_service_marks_pending_calls_as_tool_errors() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    let rx = queue(&mut slot, 3, u64::MAX);

    slot.fail_pending("service exited");

    let line = rx.try_recv().expect("failure response");
    let response: rpc::Response = serde_json::from_str(&line).expect("json");
    let result = response.result.expect("tool result");
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "service exited");
}

#[cfg(unix)]
#[test]
fn a_plugin_serves_ping_and_tool_calls_then_loses_its_pending_call_on_crash() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = crate::platform::test_support::temp_dir().join(format!(
        "svcd-ipc-unit-{}-{}",
        std::process::id(),
        nanos
    ));
    let paths = Arc::new(Paths::under(&root));
    fs::create_dir_all(&paths.logs_dir).expect("logs");

    let script = root.join("agent.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
*svcd.ping*) printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}
' ;;
*'"name":"echo"'*) printf '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}
' ;;
*'"name":"fail"'*) printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"boom"}}
' ;;
*'"name":"hang"'*) ;;
*'"name":"crash"'*) exit 0 ;;
  esac
done
"#,
    )
    .expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

    let exec = Exec {
        path: "/bin/sh".into(),
        args: vec![script.to_string_lossy().to_string()],
        env: BTreeMap::new(),
        ready: Ready::Notify,
        interpreter: None,
    };
    let mut slot = Slot::new(
        "demo".into(),
        exec,
        None,
        Restart::OnFailure,
        paths,
        test_limits(),
    );
    slot.set_protocol_only(true);
    let started = Instant::now();
    slot.apply(Event::Start, 0);
    while slot.machine.state != State::Running {
        slot.pump(started.elapsed().as_millis() as u64);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "plugin did not become ready: {:?}",
            slot.machine.state
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let (tx, rx) = mpsc::channel();
    slot.request_capability(
        "echo",
        json!({}),
        Some(json!(100)),
        elapsed(&started),
        None,
        crate::wire::control::channel::Reply::plain(tx),
    )
    .expect("echo call");
    let response = wait_response(&mut slot, &rx, &started);
    assert_eq!(
        response.result.expect("echo result")["content"][0]["text"],
        "pong"
    );

    let (tx, rx) = mpsc::channel();
    slot.request_capability(
        "fail",
        json!({}),
        Some(json!(101)),
        elapsed(&started),
        None,
        crate::wire::control::channel::Reply::plain(tx),
    )
    .expect("fail call");
    let result = wait_response(&mut slot, &rx, &started)
        .result
        .expect("failure result");
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "boom");

    let (tx, rx) = mpsc::channel();
    slot.request_capability(
        "hang",
        json!({}),
        Some(json!(102)),
        elapsed(&started),
        None,
        crate::wire::control::channel::Reply::plain(tx),
    )
    .expect("hang call");
    let (crash_tx, _crash_rx) = mpsc::channel();
    let _ = slot.request_capability(
        "crash",
        json!({}),
        Some(json!(103)),
        elapsed(&started),
        None,
        crate::wire::control::channel::Reply::plain(crash_tx),
    );

    let result = wait_response(&mut slot, &rx, &started)
        .result
        .expect("crash result");
    assert_eq!(result["isError"], true);
    assert!(slot.pending.is_empty());

    while slot.machine.state == State::Running && started.elapsed() < Duration::from_secs(5) {
        slot.pump(elapsed(&started));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(slot.machine.state, State::Running);

    let _ = fs::remove_dir_all(&root);
}

fn elapsed(started: &Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn wait_response(slot: &mut Slot, rx: &Receiver<String>, started: &Instant) -> rpc::Response {
    loop {
        slot.pump(elapsed(started));
        if let Ok(line) = rx.try_recv() {
            return serde_json::from_str(&line).expect("response json");
        }
        assert!(started.elapsed() < Duration::from_secs(5), "no response");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The conformance suite lives in the kernel, so the slot-backed adapter's
/// conformance test lives here: a domain module may reference the kernel,
/// the reverse edge would be a layering violation (tools/dep-lint.mjs).
fn conforming_wire(
    respond: impl Fn(&Invocation) -> Result<Value, Failure> + Send + 'static,
) -> crate::kernel::contract::Instance {
    let contract =
        crate::kernel::conformance::sample_contract(crate::kernel::contract::Limits::default());
    crate::kernel::contract::Instance::new(
        contract,
        Box::new(SlotWire::new(SlotHandle::scripted(respond), None)),
    )
    .expect("valid contract")
}

#[test]
fn the_slot_wire_conforms_on_the_call_surface_and_delegates_lifecycle() {
    let contract =
        crate::kernel::conformance::sample_contract(crate::kernel::contract::Limits::default());
    crate::kernel::conformance::assert_call_conforming(
        &contract,
        || conforming_wire(|_| Ok(json!({"ok": true}))),
        |class| {
            conforming_wire(move |_| Err(Failure::new(class, Retry::Never, "injected failure")))
        },
    );
    crate::kernel::conformance::assert_delegates_lifecycle(&contract, || {
        conforming_wire(|_| Ok(json!({})))
    });
}

#[test]
fn the_slot_wire_cannot_pass_the_owned_lifecycle_suite() {
    // 钉住「为什么套件要按入口拆」：槽的生命周期属于 daemon（ADR-0015），
    // SlotWire 只能拒绝 Ensure / Quiesce，所以它永远过不了生命周期那一套。
    let contract =
        crate::kernel::conformance::sample_contract(crate::kernel::contract::Limits::default());
    let violations = crate::kernel::conformance::check(
        &contract,
        || conforming_wire(|_| Ok(json!({}))),
        |_| conforming_wire(|_| Ok(json!({}))),
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("Ensure")),
        "the slot wire must fail the owned lifecycle suite: {violations:#?}"
    );
}

#[test]
fn a_cancelled_call_never_reaches_the_slot() {
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = seen.clone();
    let mut wire = SlotWire::new(
        SlotHandle::scripted(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"ok": true}))
        }),
        None,
    );

    let invocation = Invocation::new("echo", json!({}), std::time::Duration::from_secs(1));
    invocation.cancel.cancel();
    let (tx, rx) = mpsc::channel();
    wire.call(
        invocation,
        Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }),
    )
    .expect("dispatch");
    match rx
        .recv_timeout(Duration::from_secs(1))
        .expect("cancelled result")
    {
        Err(failure) => assert_eq!(failure.class, Class::Cancelled),
        Ok(reply) => panic!("a cancelled call must not return Ok: {reply:?}"),
    }
    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "the slot must not see a call that was already cancelled"
    );
}

#[test]
fn a_call_returns_before_the_slot_answers() {
    let (release_tx, release_rx) = mpsc::channel();
    let mut wire = SlotWire::new(
        SlotHandle::scripted(move |_| {
            let _ = release_rx.recv();
            Ok(json!({"ok": true}))
        }),
        None,
    );

    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    wire.call(
        Invocation::new("echo", json!({}), Duration::from_secs(1)),
        Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }),
    )
    .expect("dispatch");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "submit must return without waiting for the slot"
    );
    assert!(
        rx.try_recv().is_err(),
        "the result must not be ready before the slot answers"
    );
    release_tx.send(()).expect("release the scripted slot");
    let outcome = rx.recv_timeout(Duration::from_secs(1)).expect("done");
    assert!(matches!(outcome, Ok(Reply::Value(value)) if value["ok"] == true));
}

/// A stop can be requested twice (a CLI stop on a `Stopping` service, shutdown
/// after a timeout). The second request must keep the first snapshot: by then
/// the leader may be gone, and recapturing would come back empty — leaving the
/// escaped child neither counted nor killed (#334).
#[cfg(target_os = "linux")]
#[test]
fn a_second_stop_request_keeps_the_first_tree_snapshot() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut slot = slot();
    let mut leader = Command::new("sh")
        .arg("-c")
        .arg("setsid sleep 30 >/dev/null 2>&1 & wait")
        // Its own group, like a supervised service: the group signal below has
        // to reach the leader and miss the child that left that group.
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the leader");
    let pid = leader.id();
    slot.machine.pid = Some(pid);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        slot.capture_tree();
        if !slot.descendants.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "no descendant appeared");
        std::thread::sleep(Duration::from_millis(10));
    }
    let captured = slot.descendants.len();
    assert!(slot.tree_alive(), "the setsid child keeps the tree alive");

    // The leader dies first, exactly what the first stop's kill does; the
    // second request must not trade the snapshot for an empty one.
    crate::platform::host::process::terminate_tree(pid);
    assert!(slot.machine.pid.is_some());
    let _ = leader.wait();
    slot.capture_tree();
    assert_eq!(
        slot.descendants.len(),
        captured,
        "the snapshot must survive"
    );
    assert!(
        slot.tree_alive(),
        "the escaped child is still alive after the leader is gone"
    );

    slot.descendants.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    while slot.tree_alive() {
        assert!(Instant::now() < deadline, "the kill half must take it");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// --- lifecycle actions (#335/#337) -----------------------------------------
//
// A lifecycle action is a real process. These tests use the test binary itself
// as the hook: it runs one named fixture test, which exits with a real status on
// every platform and needs no shell. `current_exe` keeps the fixtures
// platform-neutral.

/// A hook that runs one fixture test of this binary.
fn fixture_hook(test: &str, timeout_ms: u64, env: BTreeMap<String, String>) -> Hook {
    let mut env = env;
    env.insert("SVCD_HOOK_FIXTURE".into(), "1".into());
    Hook {
        path: std::env::current_exe()
            .expect("test executable")
            .to_string_lossy()
            .into_owned(),
        args: vec![test.into()],
        env,
        timeout_ms,
        interpreter: None,
    }
}

fn lifecycle(start: Option<Hook>, started: Option<Hook>, stop: Option<Hook>) -> Lifecycle {
    Lifecycle {
        start,
        started,
        stop,
        status: None,
    }
}

/// A stop action that records `SVCD_STOP_REASON` where the test can read it.
fn recorder_hook(out: &std::path::Path) -> Hook {
    fixture_hook(
        "hook_fixture_records_the_stop_reason",
        5_000,
        BTreeMap::from([(
            "SVCD_TEST_STOP_OUT".to_string(),
            out.to_string_lossy().into_owned(),
        )]),
    )
}

fn temp_file(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    crate::platform::test_support::temp_dir().join(format!(
        "svcd-slot-{}-{}-{}.txt",
        tag,
        std::process::id(),
        nanos
    ))
}

/// A live process standing in for the supervised `exec`; the slot holds it, so
/// `check_exit` can reap it like a real child.
fn dummy_process() -> std::process::Child {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .arg("hook_fixture_sleeps")
        .env("SVCD_HOOK_FIXTURE", "1");
    command.spawn().expect("dummy process")
}

/// A slot in `Starting` with the dummy process as its exec, and the start
/// deadline already armed as `Action::Start` does (Spawn + ArmStart).
fn starting_slot(lifecycle: Option<Lifecycle>) -> (Slot, u32) {
    let mut slot = slot();
    slot.set_lifecycle(lifecycle);
    let child = dummy_process();
    let pid = child.id();
    slot.machine.state = State::Starting;
    slot.machine.pid = Some(pid);
    slot.child = Some(child);
    slot.start_deadline = Some(supervisor::START_TIMEOUT_MS);
    slot.reset_started();
    (slot, pid)
}

fn pump_until(slot: &mut Slot, timeout: Duration, done: impl Fn(&Slot) -> bool) {
    let deadline = Instant::now() + timeout;
    let mut now = 0u64;
    loop {
        now += 20;
        slot.pump(now);
        if done(slot) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition never held; state {:?}",
            slot.machine.state
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Kill the dummy exec but keep the handle, so `check_exit` settles the exit.
fn kill_exec(slot: &mut Slot) {
    if let Some(child) = slot.child.as_mut() {
        let _ = child.kill();
    }
}

fn reap_exec(slot: &mut Slot) {
    if let Some(mut child) = slot.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn wait_for_reason(path: &std::path::Path, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.is_empty() {
                return text;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the stop action never recorded a reason"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_declared_started_action_withholds_ready_until_it_exits() {
    let (mut slot, pid) = starting_slot(Some(lifecycle(
        None,
        Some(fixture_hook(
            "hook_fixture_exits_zero",
            5_000,
            BTreeMap::new(),
        )),
        None,
    )));
    slot.apply(Event::Ready { pid }, 0);
    assert_eq!(
        slot.machine.state,
        State::Starting,
        "Ready must be withheld while the action runs"
    );
    assert!(slot.started.running());
    pump_until(&mut slot, Duration::from_secs(10), |slot| {
        slot.machine.state == State::Running
    });
    reap_exec(&mut slot);
}

#[test]
fn ready_settles_directly_without_a_started_action() {
    let (mut slot, pid) = starting_slot(None);
    slot.apply(Event::Ready { pid }, 0);
    assert_eq!(slot.machine.state, State::Running);
    reap_exec(&mut slot);
}

#[test]
fn a_failing_started_action_fails_the_start_and_stop_reports_start_failed() {
    let out = temp_file("started-failed");
    let (mut slot, pid) = starting_slot(Some(lifecycle(
        None,
        Some(fixture_hook(
            "hook_fixture_exits_nonzero",
            5_000,
            BTreeMap::new(),
        )),
        Some(recorder_hook(&out)),
    )));
    slot.apply(Event::Ready { pid }, 0);
    pump_until(&mut slot, Duration::from_secs(10), |slot| {
        slot.machine.state == State::Stopping
    });
    let failure = slot.take_start_failure().expect("a reason for the start");
    assert!(
        failure.reason.contains("started action exited"),
        "{}",
        failure.reason
    );
    assert!(!failure.timeout, "a non-zero exit is not a timeout");

    kill_exec(&mut slot);
    pump_until(&mut slot, Duration::from_secs(10), |slot| {
        slot.machine.state == State::Backoff
    });
    assert_eq!(
        wait_for_reason(&out, Duration::from_secs(10)),
        "start-failed"
    );
    reap_exec(&mut slot);
}

#[test]
fn a_started_action_timeout_fails_the_start_as_a_timeout() {
    let (mut slot, pid) = starting_slot(Some(lifecycle(
        None,
        Some(fixture_hook("hook_fixture_sleeps", 50, BTreeMap::new())),
        None,
    )));
    slot.apply(Event::Ready { pid }, 0);
    pump_until(&mut slot, Duration::from_secs(10), |slot| {
        slot.machine.state == State::Stopping
    });
    let failure = slot.take_start_failure().expect("a reason for the start");
    assert!(failure.timeout, "a window that ran out is a 504");
    assert!(failure.reason.contains("timed out"), "{}", failure.reason);
    reap_exec(&mut slot);
}

#[test]
fn a_process_exit_during_the_started_action_fails_the_start() {
    let (mut slot, pid) = starting_slot(Some(lifecycle(
        None,
        Some(fixture_hook("hook_fixture_sleeps", 5_000, BTreeMap::new())),
        None,
    )));
    slot.apply(Event::Ready { pid }, 0);
    kill_exec(&mut slot);
    pump_until(&mut slot, Duration::from_secs(10), |slot| {
        slot.machine.state == State::Backoff
    });
    let failure = slot.take_start_failure().expect("a reason for the start");
    assert!(
        failure.reason.contains("while the started action ran"),
        "{}",
        failure.reason
    );
    assert!(!failure.timeout);
    reap_exec(&mut slot);
}

#[test]
fn a_slow_started_action_is_bounded_by_its_own_timeout_not_the_start_window() {
    // `Action::ArmStart` runs after `Spawn`, so a `ready: exec` Ready that
    // settles inside the spawn used to get the 30s window re-armed underneath
    // the action (#341).
    let out = temp_file("slow-started");
    let (mut slot, pid) = starting_slot(Some(lifecycle(
        None,
        Some(fixture_hook("hook_fixture_sleeps", 45_000, BTreeMap::new())),
        Some(recorder_hook(&out)),
    )));
    slot.apply(Event::Ready { pid }, 0);
    // Exactly what `ArmStart` did after the gate had already lifted the window.
    slot.start_deadline = Some(30_000);
    for now in (1_000..=30_000).step_by(1_000) {
        slot.pump(now);
    }
    assert_eq!(
        slot.machine.state,
        State::Starting,
        "the 30s start window must not cut the action"
    );
    assert!(slot.started.running());

    // The action's own deadline still bounds it.
    slot.pump(45_001);
    assert_eq!(slot.machine.state, State::Stopping);
    let failure = slot.take_start_failure().expect("a reason for the start");
    assert!(failure.timeout);
    reap_exec(&mut slot);
}

#[test]
fn a_failed_start_action_reports_start_failed_to_the_stop_hook() {
    // The stop action spawns before the machine settles the failure, so the
    // reason cannot come from `machine.cause` yet (#341).
    let out = temp_file("start-failed");
    let mut slot = slot();
    slot.set_lifecycle(Some(lifecycle(
        Some(fixture_hook(
            "hook_fixture_exits_nonzero",
            5_000,
            BTreeMap::new(),
        )),
        None,
        Some(recorder_hook(&out)),
    )));
    slot.machine.state = State::Starting;
    slot.begin_start(0);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut now = 0u64;
    while slot.prelude.is_some() {
        now += 20;
        slot.check_prelude(now);
        assert!(Instant::now() < deadline, "the start action never settled");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(slot.machine.state, State::Backoff);
    let failure = slot.take_start_failure().expect("a reason for the start");
    assert!(
        failure.reason.contains("start action exited"),
        "{}",
        failure.reason
    );
    assert!(!failure.timeout);
    assert_eq!(
        wait_for_reason(&out, Duration::from_secs(10)),
        "start-failed"
    );
}

#[test]
fn a_port_probe_from_an_older_instance_never_settles_ready() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let addr = listener.local_addr().expect("addr");
    let mut slot = slot();
    slot.machine.state = State::Starting;
    slot.machine.pid = Some(7);
    slot.start_port_probe(7, addr);

    // The instance is gone before the probe connects; the verdict belongs to it.
    slot.machine.state = State::Backoff;
    slot.machine.pid = None;
    for now in (20..400).step_by(20) {
        slot.pump(now);
    }
    assert_eq!(slot.machine.state, State::Backoff);
    assert!(slot.port.is_none(), "the stale probe is dropped");
}

// Fixtures below run only when the test binary is the hook; as ordinary tests
// they pass without side effects.
#[test]
fn hook_fixture_exits_zero() {}

#[test]
fn hook_fixture_exits_nonzero() {
    if std::env::var_os("SVCD_HOOK_FIXTURE").is_some() {
        panic!("fixture: requested failure");
    }
}

#[test]
fn hook_fixture_sleeps() {
    if std::env::var_os("SVCD_HOOK_FIXTURE").is_some() {
        std::thread::sleep(Duration::from_secs(10));
    }
}

#[test]
fn hook_fixture_records_the_stop_reason() {
    let Some(out) = std::env::var_os("SVCD_TEST_STOP_OUT") else {
        return;
    };
    let reason = std::env::var("SVCD_STOP_REASON").unwrap_or_default();
    std::fs::write(out, reason).expect("write the stop reason");
}

#[test]
fn a_port_that_never_accepts_names_the_address_and_is_a_timeout() {
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        listener.local_addr().expect("addr")
    };
    let mut slot = slot();
    slot.machine.state = State::Starting;
    let child = dummy_process();
    let pid = child.id();
    slot.machine.pid = Some(pid);
    slot.child = Some(child);
    slot.start_deadline = Some(100);
    slot.start_port_probe(pid, addr);

    for now in (20..=100).step_by(20) {
        slot.pump(now);
    }

    let failure = slot.take_start_failure().expect("a reason for the start");
    assert!(failure.timeout, "a window that ran out is a 504");
    assert!(
        failure.reason.contains(&addr.to_string()),
        "{}",
        failure.reason
    );
    reap_exec(&mut slot);
}

#[test]
fn a_port_ready_instance_that_dies_before_listening_names_the_address() {
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        listener.local_addr().expect("addr")
    };
    let mut slot = slot();
    slot.machine.state = State::Starting;
    let child = dummy_process();
    let pid = child.id();
    slot.machine.pid = Some(pid);
    slot.child = Some(child);
    slot.start_port_probe(pid, addr);

    slot.apply(Event::Exited { pid, code: Some(1) }, 40);

    let failure = slot.take_start_failure().expect("a reason for the start");
    assert!(!failure.timeout, "an exit is not a timeout");
    assert!(
        failure.reason.contains(&addr.to_string()),
        "{}",
        failure.reason
    );
    assert_eq!(slot.machine.state, State::Backoff);
    reap_exec(&mut slot);
}

/// A hook script under this slot's temp dir, with the body for each host: `unix`
/// is sh, `windows` is cmd. Both platforms really run it — a hook is one code
/// path with two launchers, and testing only one host tests half of it.
fn hook_script(slot: &Slot, unix: &str, windows: &str) -> Hook {
    let _ = (unix, windows);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::fs::create_dir_all(&slot.paths.temp_dir).expect("temp dir");
    #[cfg(unix)]
    {
        let path = slot.paths.temp_dir.join(format!("hook-{}.sh", nanos));
        std::fs::write(&path, format!("#!/bin/sh\n{unix}\n")).expect("hook script");
        Hook {
            path: path.to_string_lossy().to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            timeout_ms: 5_000,
            interpreter: Some(Interpreter {
                path: "/bin/sh".into(),
                args: Vec::new(),
            }),
        }
    }
    #[cfg(not(unix))]
    {
        let path = slot.paths.temp_dir.join(format!("hook-{}.cmd", nanos));
        std::fs::write(
            &path,
            format!("@echo off\r\n{}\r\n", windows.replace('\n', "\r\n")),
        )
        .expect("hook script");
        // No interpreter: `command_for_parts` wraps a `.cmd` in COMSPEC itself,
        // which is the path a real plugin takes on Windows.
        Hook {
            path: path.to_string_lossy().to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            timeout_ms: 5_000,
            interpreter: None,
        }
    }
}

/// A hook that blocks until `flag` appears, then exits. The test owns the flag,
/// so "these calls are still in flight" is a fact, not a timing hope.
fn blocking_hook(slot: &Slot, flag: &std::path::Path) -> Hook {
    hook_script(
        slot,
        &format!("while [ ! -f '{}' ]; do sleep 0.02; done", flag.display()),
        &format!(
            ":wait\nif exist \"{}\" goto done\ngoto wait\n:done",
            flag.display()
        ),
    )
}

/// A hook that never exits on its own: only the host can end it.
fn endless_hook(slot: &Slot) -> Hook {
    hook_script(slot, "while :; do sleep 0.05; done", ":spin\ngoto spin")
}

/// A `HookCall` for the tests that drive `run` directly, so they can assert the
/// failure class instead of the text a sink happens to carry.
fn call_for(
    slot: &Slot,
    hook: Hook,
    cancel: crate::kernel::contract::CancelToken,
    max_result: usize,
) -> call_hook::HookCall {
    call_hook::HookCall {
        id: slot.id.clone(),
        hook,
        trace: Some("7".into()),
        capability: "echo".into(),
        arguments: serde_json::json!({}),
        timeout: Duration::from_secs(5),
        cancel,
        paths: slot.paths.clone(),
        plugin_dir: None,
        limits: slot.limits.clone(),
        max_result,
    }
}

/// A capability with a hook answers the call with the hook's stdout, and the
/// arguments arrive on its stdin (#342).
#[test]
fn a_capability_hook_answers_the_call_with_its_stdout() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    let hook = hook_script(&slot, "cat", "set /p line=\necho %line%");
    slot.set_capability_hooks(BTreeMap::from([("echo".to_string(), hook)]));

    let (tx, rx) = mpsc::channel();
    slot.request_capability(
        "echo",
        json!({"mode": "tproxy"}),
        Some(json!(100)),
        0,
        None,
        crate::wire::control::channel::Reply::plain(tx),
    )
    .expect("hook call is accepted");

    let started = Instant::now();
    let response = wait_response(&mut slot, &rx, &started);
    let result = response.result.expect("hook result");
    let text = result["content"][0]["text"].as_str().expect("text result");
    let echoed: serde_json::Value =
        serde_json::from_str(text).expect("the hook echoes its arguments");
    assert_eq!(echoed["mode"], "tproxy");
    assert_eq!(result["isError"], false);
    // No channel frame was ever allocated: the hook path owns no pending call.
    assert!(slot.pending.is_empty());
    assert_eq!(slot.hook_calls.load(Ordering::SeqCst), 0);
}

/// A hook whose stdout is not JSON still answers: the text is the result, the
/// host does not validate it (#342).
#[test]
fn a_hook_answers_with_whatever_text_it_printed() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    let hook = hook_script(&slot, "printf 'off: 12'", "echo off: 12");
    slot.set_capability_hooks(BTreeMap::from([("mode".to_string(), hook)]));

    let (tx, rx) = mpsc::channel();
    slot.request_capability(
        "mode",
        json!({}),
        Some(json!(104)),
        0,
        None,
        crate::wire::control::channel::Reply::plain(tx),
    )
    .expect("hook call is accepted");

    let started = Instant::now();
    let response = wait_response(&mut slot, &rx, &started);
    let result = response.result.expect("hook result");
    assert_eq!(result["content"][0]["text"], "off: 12");
    assert_eq!(result["isError"], false);
}

#[test]
fn a_hook_that_exits_non_zero_fails_the_call() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    let hook = hook_script(&slot, "echo boom >&2; exit 3", "echo boom 1>&2\nexit /b 3");
    slot.set_capability_hooks(BTreeMap::from([("mode".to_string(), hook)]));

    let (tx, rx) = mpsc::channel();
    slot.request_capability(
        "mode",
        json!({}),
        Some(json!(101)),
        0,
        None,
        crate::wire::control::channel::Reply::plain(tx),
    )
    .expect("hook call is accepted");

    let started = Instant::now();
    let response = wait_response(&mut slot, &rx, &started);
    let result = response.result.expect("a failed call still answers");
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("exited"),
        "{result}"
    );
    assert_eq!(slot.hook_calls.load(Ordering::SeqCst), 0);
}

/// A failure carries a class, not just text: the face projects the class, so a
/// non-zero exit must not be spelled as a timeout (#342).
#[test]
fn a_non_zero_exit_is_an_implementation_error_not_a_timeout() {
    let slot = slot();
    let hook = hook_script(&slot, "echo boom >&2; exit 3", "echo boom 1>&2\nexit /b 3");
    let call = call_for(
        &slot,
        hook,
        crate::kernel::contract::CancelToken::default(),
        crate::wire::message::ipc::CALL_FRAME_MAX,
    );
    let failure = call_hook::run(&call).expect_err("a non-zero exit is a failed call");
    assert_eq!(failure.class, Class::ImplementationError);
    assert!(failure.detail.contains("exited"), "{}", failure.detail);
}

#[test]
fn a_hook_that_overruns_its_timeout_fails_the_call() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    let mut hook = endless_hook(&slot);
    hook.timeout_ms = 200;
    slot.set_capability_hooks(BTreeMap::from([("slow".to_string(), hook)]));

    let (tx, rx) = mpsc::channel();
    slot.request_capability(
        "slow",
        json!({}),
        Some(json!(102)),
        0,
        None,
        crate::wire::control::channel::Reply::plain(tx),
    )
    .expect("hook call is accepted");

    let started = Instant::now();
    let response = wait_response(&mut slot, &rx, &started);
    let result = response.result.expect("a timed-out call still answers");
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("timed out"),
        "{result}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

/// A caller that goes away stops the hook instead of waiting for it: the whole
/// point of the cancel token on this path.
#[test]
fn a_cancelled_call_does_not_wait_for_the_hook() {
    let slot = slot();
    let hook = endless_hook(&slot);
    let token = crate::kernel::contract::CancelToken::default();
    token.cancel();
    let call = call_for(
        &slot,
        hook,
        token,
        crate::wire::message::ipc::CALL_FRAME_MAX,
    );

    let started = Instant::now();
    let failure = call_hook::run(&call).expect_err("a cancelled call has no result");
    assert_eq!(failure.class, Class::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the hook was waited for after the caller left"
    );
}

/// A result over the contract's inline limit is refused, not truncated into a
/// shorter answer that reads like a real one.
#[test]
fn a_result_over_the_inline_limit_is_refused() {
    let slot = slot();
    let hook = hook_script(
        &slot,
        "head -c 400 /dev/zero | tr '\\0' x",
        "for /l %%i in (1,1,10) do @echo 0123456789012345678901234567890123456789",
    );
    let call = call_for(
        &slot,
        hook,
        crate::kernel::contract::CancelToken::default(),
        64,
    );
    let failure = call_hook::run(&call).expect_err("an oversized result is a failed call");
    assert_eq!(failure.class, Class::FrameTooLarge);
    assert!(failure.detail.contains("64"), "{}", failure.detail);
}

/// The hook fan-out is bounded: each call forks a process, so a full set of
/// in-flight hooks is backpressure, not a queue. The hooks really block, so the
/// counter is saturated by running work, not by writing the number in.
#[test]
fn a_full_set_of_hook_calls_reports_busy() {
    let mut slot = slot();
    slot.machine.state = State::Running;
    let flag = slot.paths.temp_dir.join("go");
    let hook = blocking_hook(&slot, &flag);
    slot.set_capability_hooks(BTreeMap::from([("echo".to_string(), hook)]));

    let mut replies = Vec::new();
    let started = Instant::now();
    for index in 0..call_hook::MAX_HOOK_CALLS {
        let (tx, rx) = mpsc::channel();
        slot.request_capability(
            "echo",
            json!({}),
            Some(json!(200 + index)),
            0,
            None,
            crate::wire::control::channel::Reply::plain(tx),
        )
        .expect("a call inside the cap is accepted");
        replies.push(rx);
    }
    assert_eq!(
        slot.hook_calls.load(Ordering::SeqCst),
        call_hook::MAX_HOOK_CALLS,
        "the accepted calls are in flight"
    );

    let (tx, rx) = mpsc::channel();
    slot.request_capability(
        "echo",
        json!({}),
        Some(json!(203)),
        0,
        None,
        crate::wire::control::channel::Reply::plain(tx),
    )
    .expect("the request is accepted, then refused as busy");
    let line = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a busy reply");
    assert!(line.contains("in-flight"), "{line}");

    // The budget comes back when the blocked hooks are let go.
    std::fs::write(&flag, b"go").expect("release the hooks");
    for rx in &replies {
        let response = wait_response(&mut slot, rx, &started);
        let result = response.result.expect("a hook result");
        assert_eq!(result["isError"], false, "{result}");
    }
    assert_eq!(slot.hook_calls.load(Ordering::SeqCst), 0);
}

/// A worker that never starts still answers its caller and gives the slot back.
/// Both properties live in one `Drop`, so this pins the pair: a decrement
/// without the reply (or twice) is what the previous shape got wrong.
#[test]
fn a_worker_that_never_starts_answers_and_releases_its_slot() {
    let slot = slot();
    let counter = slot.hook_calls.clone();
    counter.fetch_add(1, Ordering::SeqCst);
    let seen = Arc::new(Mutex::new(None));
    let sink_seen = seen.clone();
    let sink: crate::kernel::contract::ReplySink = Box::new(move |outcome| {
        if let Err(failure) = outcome {
            *sink_seen.lock().expect("sink") = Some(failure.class);
        }
    });
    drop(call_hook::Answer::new(sink, counter.clone()));
    assert_eq!(
        *seen.lock().expect("sink"),
        Some(Class::Busy),
        "the caller gets a reply it can retry"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "the slot is released once"
    );
}
