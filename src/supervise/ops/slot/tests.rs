#![cfg(test)]

use super::calls::{test_reply_sink, CallError, PendingCall};
use std::collections::BTreeMap;

use crate::kernel::contract::Wire;
use crate::kernel::registry::{Hook, Interpreter};

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
    let root = crate::test_support::temp_dir().join(format!(
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

    let command = hook_command("demo", "stop", &hook, &test_paths());
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
    let root = crate::test_support::temp_dir().join(format!(
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

#[cfg(unix)]
fn elapsed(started: &Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

#[cfg(unix)]
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
