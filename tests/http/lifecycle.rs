//! lifecycle http tests.

use super::*;

#[test]
fn services_version_start_and_stop() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let services = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(services.status, 200);
    let ids: Vec<&str> = services.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .filter_map(|service| service["id"].as_str())
        .collect();
    assert!(!ids.contains(&"svc"), "{ids:?}");
    assert!(ids.contains(&"demo"), "{ids:?}\n{}", sandbox.daemon_log());

    let status = request(&address, "GET", "/api/v1/services/demo", "", b"");
    assert_eq!(status.status, 200);
    assert_eq!(status.body["summary"]["kind"], "core");

    let version = request(&address, "GET", "/api/v1/version", "", b"");
    assert_eq!(version.status, 200);
    assert_eq!(version.body["protocol"], 1);
    assert!(version.body["platform"].is_string());

    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    assert_eq!(started.body["summary"]["state"], "running");

    let stopped = request(&address, "POST", "/api/v1/services/demo/stop", "", b"");
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    assert_eq!(stopped.body["summary"]["state"], "stopped");

    let missing = request(&address, "GET", "/api/v1/services/missing", "", b"");
    assert_eq!(missing.status, 404);
    assert!(missing.body["error"]["code"].is_string());
    assert!(missing.body["error"]["message"].is_string());
}

/// One installed plugin, exercised end to end across every surface this phase
/// built: HTTP install -> start -> MCP (Streamable HTTP and the stdio bridge
/// agree) -> config/reset -> Web UI -> logs -> stop shrinks the tool surface ->
/// uninstall removes it everywhere.
#[test]
fn phase_end_to_end_install_start_mcp_config_logs_and_uninstall() {
    let (sandbox, address) = web_ui_sandbox(34);
    set_password(&sandbox, TEST_PASSWORD);
    let token = TEST_PASSWORD.to_string();
    let base = plugin_ui_base(&address, "demo");

    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    assert_eq!(started.body["summary"]["state"], "running");

    // The CLI reports the same state the HTTP API just returned.
    let cli = sandbox.run_svc(&["status", "demo", "--json"]);
    assert_eq!(
        cli.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_status: Value = serde_json::from_slice(&cli.stdout).expect("svc status json");
    assert_eq!(cli_status["summary"]["state"], "running");

    // Streamable HTTP: a live plugin exposes start + its manifest capability.
    let listed = mcp_request(&address, &token, "tools/list", json!({}), None);
    assert_eq!(listed.status, 200, "{}", listed.body);
    let http_tools = tool_names(&listed.body["result"]["tools"]);
    assert!(
        http_tools.contains(&"demo.status".to_string()),
        "{http_tools:?}"
    );
    assert!(
        http_tools.contains(&"demo.start".to_string()),
        "{http_tools:?}"
    );

    let called = mcp_request(
        &address,
        &token,
        "tools/call",
        json!({"name": "demo.status", "arguments": {}}),
        Some("demo.status"),
    );
    assert_eq!(called.status, 200, "{}", called.body);
    assert_eq!(called.body["result"]["isError"], false, "{}", called.body);
    assert_eq!(called.body["result"]["content"][0]["text"], "pong");

    // The stdio bridge must expose exactly the same surface and result.
    let stdio_listed = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/list",
        "params": {"_meta": modern_meta()},
    }));
    assert_eq!(
        tool_names(&stdio_listed["result"]["tools"]),
        http_tools,
        "{stdio_listed}"
    );
    let stdio_called = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "demo.status",
            "arguments": {},
            "_meta": modern_meta(),
        },
    }));
    assert_eq!(stdio_called["result"]["isError"], false, "{stdio_called}");
    assert_eq!(stdio_called["result"]["content"][0]["text"], "pong");

    // Config belongs to the same installed version.
    let config = raw_request(&address, "GET", "/api/v1/config/demo", "", b"");
    assert_eq!(config.status, 200);
    assert_eq!(config.body, b"initial config\n");
    let updated = raw_request(
        &address,
        "PUT",
        "/api/v1/config/demo",
        "text/plain",
        b"updated\n",
    );
    assert_eq!(
        updated.status,
        200,
        "{}",
        String::from_utf8_lossy(&updated.body)
    );
    let config = raw_request(&address, "GET", "/api/v1/config/demo", "", b"");
    assert_eq!(config.body, b"updated\n");
    let reset = raw_request(&address, "POST", "/api/v1/config/demo/reset", "", b"");
    assert_eq!(reset.status, 200);
    let config = raw_request(&address, "GET", "/api/v1/config/demo", "", b"");
    assert_eq!(config.body, b"initial config\n");

    // Web UI and logs are served from the same installation.
    let ui = raw_request(&address, "GET", &base, "", b"");
    assert_eq!(ui.status, 200);
    assert!(String::from_utf8_lossy(&ui.body).contains(&format!("<base href=\"{}\">", base)));
    let logs = raw_request(&address, "GET", "/api/v1/logs/demo?lines=10", "", b"");
    assert_eq!(logs.status, 200, "{}", String::from_utf8_lossy(&logs.body));

    // A stopped plugin only exposes start, on both transports.
    let stopped = request(&address, "POST", "/api/v1/services/demo/stop", "", b"");
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    let listed = mcp_request(&address, &token, "tools/list", json!({}), None);
    assert_eq!(
        tool_names(&listed.body["result"]["tools"]),
        vec!["demo.start", "svc.health", "svc.version"]
    );
    let stdio_listed = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/list",
        "params": {"_meta": modern_meta()},
    }));
    assert_eq!(
        tool_names(&stdio_listed["result"]["tools"]),
        vec!["demo.start", "svc.health", "svc.version"]
    );

    // Uninstall removes the service from every surface.
    let deleted = request(&address, "DELETE", "/api/v1/plugins/demo", "", b"");
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    let list = sandbox.run_svc(&["list"]);
    assert!(
        !String::from_utf8_lossy(&list.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some("demo")),
        "{}",
        String::from_utf8_lossy(&list.stdout)
    );
    let listed = mcp_request(&address, &token, "tools/list", json!({}), None);
    assert_eq!(listed.status, 200);
    assert_eq!(
        tool_names(&listed.body["result"]["tools"]),
        vec!["svc.health", "svc.version"]
    );
    let stdio_listed = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/list",
        "params": {"_meta": modern_meta()},
    }));
    assert_eq!(
        tool_names(&stdio_listed["result"]["tools"]),
        vec!["svc.health", "svc.version"]
    );
}

#[test]
fn minimal_svcd_conf_keeps_the_daemon_alive() {
    run_min_config_phase("floor", false);
    run_min_config_phase("usable", true);
}

#[cfg(unix)]
use super::checks::{wait_until, wait_until_within};
#[cfg(unix)]
use super::support::pid_alive;

/// A plugin that forks a child leaving the process group with `setsid`, the way
/// dropbear's per-connection child does. The child outlives the group signal,
/// so stopping the plugin has to take the tree, not the group (#332).
#[cfg(unix)]
const SETSID_AGENT: &[u8] = br#"#!/bin/sh
set -u
mkdir -p "$SVCD_PLUGIN_DIR/data"
setsid sleep 300 >/dev/null 2>&1 &
echo $! > "$SVCD_PLUGIN_DIR/data/child.pid"
while IFS= read -r line; do
  printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'
done
"#;

/// The same child, but it ignores SIGTERM: only the kill half of the stop
/// (`KillTimeout` after `STOP_TIMEOUT_MS`) can take it, so this is what proves
/// the service still converges to `Stopped` instead of hanging or going
/// unkillable (#334).
#[cfg(unix)]
const STUBBORN_AGENT: &[u8] = br#"#!/bin/sh
set -u
mkdir -p "$SVCD_PLUGIN_DIR/data"
setsid sh -c 'trap "" TERM; while :; do sleep 1; done' >/dev/null 2>&1 &
echo $! > "$SVCD_PLUGIN_DIR/data/child.pid"
while IFS= read -r line; do
  printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'
done
"#;

#[cfg(unix)]
fn child_pid(sandbox: &Sandbox) -> Option<u32> {
    let file = sandbox
        .data
        .join("plugins")
        .join("demo")
        .join("data")
        .join("child.pid");
    std::fs::read_to_string(file)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// Wait until the plugin has recorded the pid of its `setsid` child.
#[cfg(unix)]
fn started_child(sandbox: &Sandbox) -> u32 {
    wait_until(
        sandbox,
        "the plugin records the pid of its child",
        || child_pid(sandbox).is_some(),
        || "no data/child.pid".to_string(),
    );
    child_pid(sandbox).expect("child pid")
}

#[cfg(unix)]
fn wait_gone(sandbox: &Sandbox, pid: u32, within: std::time::Duration) {
    wait_until_within(
        sandbox,
        &format!("pid {pid} must be gone"),
        within,
        || !pid_alive(pid),
        || format!("pid {pid} is still alive"),
    );
}

/// Uninstall while the plugin runs: no stop first, the DELETE has to take the
/// whole tree (#332).
#[cfg(unix)]
#[test]
fn uninstalling_a_running_plugin_takes_the_tree_outside_its_group() {
    let (sandbox, address) = sandbox_with_agent(41, ("agent.sh", SETSID_AGENT.to_vec()));
    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);

    let child = started_child(&sandbox);
    let deleted = request(&address, "DELETE", "/api/v1/plugins/demo", "", b"");
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    wait_gone(&sandbox, child, Duration::from_secs(5));
}

/// The same promise for `svc stop`: the acceptance bullet was about stop, not
/// only about uninstall (#334).
#[cfg(unix)]
#[test]
fn stopping_a_plugin_takes_the_tree_outside_its_group() {
    let (sandbox, address) = sandbox_with_agent(42, ("agent.sh", SETSID_AGENT.to_vec()));
    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    let child = started_child(&sandbox);

    let stopped = sandbox.run_svc(&["stop", "demo"]);
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    wait_gone(&sandbox, child, Duration::from_secs(5));
}

/// A descendant that ignores SIGTERM is not killed by the polite half, so the
/// service has to stay `Stopping` until `KillTimeout` takes it, and then report
/// `Stopped` — not `Failed` (unkillable) (#334).
#[cfg(unix)]
#[test]
fn a_descendant_that_ignores_sigterm_still_converges_to_stopped() {
    let (sandbox, address) = sandbox_with_agent(43, ("agent.sh", STUBBORN_AGENT.to_vec()));
    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    let child = started_child(&sandbox);
    assert!(pid_alive(child), "the stubborn child is running");

    let deleted = request(&address, "DELETE", "/api/v1/plugins/demo", "", b"");
    assert_eq!(
        deleted.status, 200,
        "the stop must converge instead of going unkillable: {}",
        deleted.body
    );
    wait_gone(&sandbox, child, Duration::from_secs(5));
}
