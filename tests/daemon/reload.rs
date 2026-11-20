//! reload daemon tests.

use super::*;

/// A reload reconciles `services.d` and the plugin store. Carrying the
/// scanned plugins into `desired` is what keeps the stale sweep from
/// unregistering every installed plugin on each reload.
#[test]
fn reload_keeps_an_installed_plugin_registered() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    sandbox.install_plugin("plug", &package);

    let output = sandbox.run_when_ready(&["reload"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["state"], "stopped", "{status}");
}

#[test]
fn reload_picks_up_definitions_written_after_boot() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    // Wait for the boot scan to finish before adding the definition. A
    // `svc list` probe would answer from disk before the daemon is ready.
    sandbox.wait_for_daemon();
    sandbox.write_core_json("late");

    let output = sandbox.run_when_ready(&["reload"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("added: late"), "reload output: {}", text);

    let status = sandbox.wait_for_state("late", "running", Duration::from_secs(15));
    assert_eq!(status["summary"]["enabled"], true, "{status}");
    sandbox.stop_service("late");
}

#[test]
fn reload_stops_and_forgets_a_deleted_definition() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("gone");
    sandbox.start_daemon();
    let instance = ProcessInstance::capture(sandbox.running_pid("gone"));

    fs::remove_file(sandbox.config.join("services.d").join("gone.json"))
        .expect("remove definition");
    let output = sandbox.run_when_ready(&["reload"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("removed: gone"), "reload output: {}", text);

    wait_until(
        "the removed service to exit",
        Duration::from_secs(15),
        || !instance.is_alive(),
    );
    let status = sandbox.run_when_ready(&["status", "gone"]);
    assert_eq!(status.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&status.stderr).contains("unknown service"));
}

#[test]
fn reload_without_changes_reports_nothing_to_do() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();

    let output = sandbox.run_when_ready(&["reload"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("no changes"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn a_definition_added_while_running_is_picked_up_without_reload() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_with_env(&[("SVCD_RESCAN_INTERVAL_MS", "50")]);
    sandbox.wait_for_daemon();

    sandbox.write_core_json("late");
    wait_until(
        "the new definition to start",
        Duration::from_secs(15),
        || {
            let output = sandbox.run_svc(&["status", "late", "--json"]);
            if output.status.code() != Some(0) {
                return false;
            }
            serde_json::from_slice::<Value>(&output.stdout)
                .map(|value| value["summary"]["state"] == "running")
                .unwrap_or(false)
        },
    );
    let status = sandbox.status("late");
    assert_eq!(status["summary"]["enabled"], true, "{status}");
    sandbox.stop_service("late");
}

#[test]
fn a_definition_deleted_while_running_is_stopped_without_reload() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("gone");
    sandbox.start_daemon_with_env(&[("SVCD_RESCAN_INTERVAL_MS", "50")]);
    let instance = ProcessInstance::capture(sandbox.running_pid("gone"));

    fs::remove_file(sandbox.config.join("services.d").join("gone.json"))
        .expect("remove definition");
    wait_until(
        "the removed service to disappear",
        Duration::from_secs(15),
        || sandbox.run_svc(&["status", "gone"]).status.code() == Some(1) && !instance.is_alive(),
    );
}
