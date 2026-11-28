#![cfg(test)]
//! 生命周期动作（`#335`/`#337`）：started 的时序、失败与 stop 的树快照。

use super::*;

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

/// Kill the dummy exec but keep the handle, so `check_exit` settles the exit.
fn kill_exec(slot: &mut Slot) {
    if let Some(child) = slot.child.as_mut() {
        let _ = child.kill();
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
