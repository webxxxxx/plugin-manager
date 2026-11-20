//! install daemon tests.

use super::*;

#[test]
fn an_install_for_another_device_is_rejected() {
    let mut sandbox = Sandbox::new();
    sandbox.write_device_marker("sr6110");
    let package = sandbox.write_compat_package("plug", &["sr1010"]);
    sandbox.start_daemon();

    let output = sandbox.run_when_ready_plugin(&["install", package.to_str().expect("path")]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requires device"), "{stderr}");

    let list = sandbox.run_when_ready_plugin(&["list"]);
    assert!(
        !String::from_utf8_lossy(&list.stdout).contains("plug"),
        "rejected package leaked into the list"
    );
}

#[test]
fn an_install_for_this_device_is_accepted() {
    let mut sandbox = Sandbox::new();
    sandbox.write_device_marker("sr6110");
    let package = sandbox.write_compat_package("plug", &["sr6110", "sr1010-v1"]);
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);

    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["kind"], "plugin", "{status}");
    assert_eq!(status["summary"]["enabled"], false, "{status}");
}

#[test]
fn a_universal_package_installs_without_a_marker() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);
    assert_eq!(sandbox.status("plug")["summary"]["kind"], "plugin");
}

#[test]
fn a_marker_change_marks_an_installed_plugin_failed() {
    let mut sandbox = Sandbox::new();
    sandbox.write_device_marker("sr6110");
    let package = sandbox.write_compat_package("plug", &["sr6110"]);
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    sandbox.install_plugin("plug", &package);
    sandbox.shutdown();

    // The firmware changes generation under the same installation.
    sandbox.write_device_marker("sr1010");
    sandbox.start_daemon();
    // The reload probe also proves a reload does not prune the plugin from the
    // registry: the status below would say "unknown service" if it did.
    sandbox.wait_for_daemon();

    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["state"], "failed", "{status}");
    assert!(status["summary"]["pid"].is_null(), "{status}");

    let started = sandbox.run_when_ready(&["start", "plug"]);
    assert_eq!(started.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("requires device"),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
}

#[test]
fn a_fresh_plugin_install_is_disabled_until_enabled() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);

    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["enabled"], false, "{status}");
    assert_eq!(status["summary"]["state"], "stopped", "{status}");

    // `plugin enable` flips the boot flag without starting the process.
    let enabled = sandbox.run_when_ready_plugin(&["enable", "plug"]);
    assert_eq!(
        enabled.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["enabled"], true, "{status}");
    assert_eq!(status["summary"]["state"], "stopped", "{status}");

    let disabled = sandbox.run_when_ready_plugin(&["disable", "plug"]);
    assert_eq!(
        disabled.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    assert_eq!(sandbox.status("plug")["summary"]["enabled"], false);

    // The plugin entry refuses core services.
    sandbox.write_core_json("core");
    assert_eq!(
        sandbox
            .run_when_ready_plugin(&["enable", "core"])
            .status
            .code(),
        Some(1)
    );
}
