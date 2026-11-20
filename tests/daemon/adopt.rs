//! adopt daemon tests.

use super::*;

/// A pid that has been reused by another process is not the pre-crash
/// instance; this is the false red #155 reported ("the pre-crash instance is
/// still alive"). The reuse cannot be forced, so the creation time of a live
/// process is compared against a recorded one that belongs to a different
/// process: `pid_alive` is true and the instance must still read as gone.
#[test]
fn a_reused_pid_is_not_the_old_instance() {
    let (mut child, pid) = spawn_sleeper();
    let live = ProcessInstance::capture(pid);
    assert!(live.is_alive(), "a live child is the instance we captured");

    // A host without a creation time has only the pid to compare, so the reuse
    // case below cannot be stated there.
    #[cfg(any(windows, target_os = "linux"))]
    {
        let start = live.start.expect("a creation time for a live child");
        let reused = ProcessInstance {
            pid,
            start: Some(start.wrapping_add(1)),
            gone: false,
        };
        assert!(pid_alive(pid), "the pid itself is still occupied");
        assert!(
            !reused.is_alive(),
            "a different process holding pid {pid} is not the pre-crash instance"
        );
        assert!(live.is_replaced_by(reused));
    }

    kill_pid(pid);
    let _ = child.wait();
    assert!(!live.is_alive(), "a reaped child is gone");

    // Release the last handle: until then Windows keeps the terminated process
    // (and its pid) reachable. A pid that is already free at capture carries no
    // identity, so anyone who takes it later is not the instance the test meant
    // to watch.
    drop(child);
    wait_until("the pid to be freed", Duration::from_secs(5), || {
        process_start_time(pid).is_none() && !pid_alive(pid)
    });
    let absent = ProcessInstance::capture(pid);
    assert!(absent.gone, "a free pid must capture as gone: {absent:?}");
    assert!(!absent.is_alive());
}

#[test]
fn a_runtime_record_for_a_deleted_definition_is_dropped_on_start() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("orphan");
    sandbox.start_daemon();
    sandbox.running_pid("orphan");

    let record = sandbox.data.join("run").join("orphan.json");
    wait_until("the runtime record", Duration::from_secs(10), || {
        record.is_file()
    });

    sandbox.force_kill_daemon();
    fs::remove_file(sandbox.config.join("services.d").join("orphan.json"))
        .expect("remove definition");

    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    wait_until(
        "the orphan record to be dropped",
        Duration::from_secs(10),
        || !record.exists(),
    );
}

/// On Windows the daemon-wide job object (`KILL_ON_JOB_CLOSE`) reaps every
/// child when svcd dies, so the restarted daemon starts exactly one fresh
/// instance per service. Unix deliberately does the opposite and adopts the
/// survivors; `a_restarted_daemon_adopts_an_exec_ready_service` covers that.
#[cfg(windows)]
#[test]
fn abrupt_svcd_exit_reaps_children_and_restart_starts_one_instance() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.write_stop_script("script");
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);
    assert_eq!(
        sandbox
            .run_when_ready_plugin(&["enable", "plug"])
            .status
            .code(),
        Some(0)
    );

    let core = ProcessInstance::capture(sandbox.running_pid("core"));
    let script = ProcessInstance::capture(sandbox.running_pid("script"));
    let plugin = ProcessInstance::capture(sandbox.running_pid("plug"));

    sandbox.force_kill_daemon();

    wait_until(
        "all children to die with svcd",
        Duration::from_secs(10),
        || !core.is_alive() && !script.is_alive() && !plugin.is_alive(),
    );

    sandbox.start_daemon();
    for (id, old) in [("core", core), ("script", script), ("plug", plugin)] {
        let status = sandbox.wait_for_restart(id, old, Duration::from_secs(15));
        let current =
            ProcessInstance::capture(status["summary"]["pid"].as_u64().unwrap_or(0) as u32);
        assert!(current.pid > 0, "{status}");
        // The pre-crash instance must be gone. The pid alone cannot say that:
        // under a parallel run the OS hands the freed pid to an unrelated
        // process quickly, so compare the instance identity instead (#155).
        assert!(
            !old.is_alive(),
            "{}: the pre-crash instance (pid {}, start {:?}) is still alive after {:?}",
            id,
            old.pid,
            old.start,
            current
        );
        assert!(
            old.is_replaced_by(current),
            "{}: the restarted daemon adopted the pre-crash instance: {status}",
            id
        );
    }

    sandbox.stop_service("core");
    sandbox.stop_service("script");
    sandbox.stop_service("plug");
}

/// Unix keeps the supervised tree alive when svcd is killed and the next daemon
/// reconnects to it from the runtime record: same pid, no duplicate instance.
#[cfg(unix)]
#[test]
fn a_restarted_daemon_adopts_an_exec_ready_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_stop_script("script");
    sandbox.start_daemon_ready();
    let adopted = ProcessInstance::capture(sandbox.running_pid("script"));

    sandbox.force_kill_daemon();
    wait_until(
        "the service to survive svcd",
        Duration::from_secs(10),
        || adopted.is_alive(),
    );

    sandbox.start_daemon_ready();
    let status = sandbox.wait_for_state("script", "running", Duration::from_secs(15));
    assert_eq!(
        status["summary"]["pid"].as_u64(),
        Some(adopted.pid as u64),
        "the restarted daemon should adopt pid {}: {}",
        adopted.pid,
        status
    );
    assert_eq!(status["details"]["failures"], 0, "{status}");
    sandbox.stop_service("script");
}

/// A record from another boot must never be adopted: pid numbers restart and a
/// stale starttime could match an unrelated process.
#[cfg(unix)]
#[test]
fn a_runtime_record_from_another_boot_is_ignored() {
    let mut sandbox = Sandbox::new();
    sandbox.write_stop_script("script");
    sandbox.start_daemon_ready();
    let stale = ProcessInstance::capture(sandbox.running_pid("script"));
    sandbox.shutdown();

    // Leave a record as if the previous daemon had written it before a reboot.
    let run_dir = sandbox.data.join("run");
    fs::create_dir_all(&run_dir).expect("run dir");
    fs::write(
        run_dir.join("script.json"),
        serde_json::to_vec(&json!({
            "boot": "an-older-boot",
            "state": "running",
            "pid": stale.pid,
        }))
        .expect("json"),
    )
    .expect("rewrite record");

    sandbox.start_daemon_ready();
    let status = sandbox.wait_for_state("script", "running", Duration::from_secs(15));
    let current = ProcessInstance::capture(status["summary"]["pid"].as_u64().expect("pid") as u32);
    assert!(
        stale.is_replaced_by(current),
        "a record from another boot must not be adopted: {}",
        status
    );
    sandbox.stop_service("script");
}
