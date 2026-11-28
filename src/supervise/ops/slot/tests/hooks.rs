#![cfg(test)]
//! 能力 hook 调用（`#342`）：stdout 即结果、超时、取消与并发上限。

use super::*;

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
