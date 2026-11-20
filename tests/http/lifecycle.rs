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
