//! Settings screen http tests (#263).

use super::*;

fn user_command(sandbox: &Sandbox) -> std::process::Command {
    let mut command = std::process::Command::new(&sandbox.svcd);
    sandbox.env(&mut command);
    command.env("SVCD_TEST_PRIVILEGE", "unprivileged");
    command
}

/// The daemon appends one "svcd starting" line per start, to the same log both
/// the first process and a restarted one write.
fn daemon_starts(sandbox: &Sandbox) -> usize {
    fs::read_to_string(sandbox.logs.join("svcd.log"))
        .map(|text| text.matches("svcd starting").count())
        .unwrap_or(0)
}

fn write_conf(sandbox: &Sandbox, extra: &str) {
    fs::write(
        sandbox.config.join("svcd.conf"),
        format!("# hand-written comment stays\nlog_level = info\n{extra}"),
    )
    .expect("write svcd.conf");
}

/// A live HTTP address, if one is answering right now. Reading the port file
/// without panicking is what lets a poll span the restart gap.
fn current_address(sandbox: &Sandbox) -> Option<String> {
    let text = fs::read_to_string(sandbox.data.join("run").join("http.port")).ok()?;
    let address = text.trim().to_string();
    TcpStream::connect(&address).ok().map(|_| address)
}

/// Wait until a daemon answers again, identify it by the post-restart state the
/// caller expects, and return its address. "Some address answers" is not enough:
/// the old process is still serving while it shuts down, and the OS may hand the
/// replacement the same ephemeral port. The probe returns `None` while the
/// address cannot be reached yet (the old listener is being dropped, which
/// Windows reports as a refused connect or a reset): that is "not ready", not a
/// failure, and the deadline bounds the wait (#285).
fn wait_for_restarted<F>(sandbox: &Sandbox, before: usize, ready: F) -> String
where
    F: Fn(&str) -> Option<bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = String::from("no address");
    while Instant::now() < deadline {
        if daemon_starts(sandbox) > before {
            if let Some(address) = current_address(sandbox) {
                match ready(&address) {
                    Some(true) => return address,
                    Some(false) => {
                        let auth = try_request(&address, "GET", "/api/v1/auth", "", b"")
                            .map(|response| response.body["passwordSet"].clone());
                        seen = format!(
                            "{address}: log_level={:?} auth={auth:?}",
                            try_effective(&address, "log_level")
                        );
                    }
                    None => seen = format!("{address}: not answering yet"),
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "the daemon did not come back with the saved state ({seen})\ndaemon log:\n{}\nsvcd.log:\n{}",
        sandbox.daemon_log(),
        fs::read_to_string(sandbox.logs.join("svcd.log")).unwrap_or_default()
    );
}

/// The effective value of one key, or `None` while the daemon is not reachable:
/// a restart probe treats "no answer yet" as "not ready", not as a regression.
fn try_effective(address: &str, key: &str) -> Option<Value> {
    let settings = try_request(address, "GET", "/api/v1/settings", "", b"").ok()?;
    key_value(&settings.body, key)
}

fn key_value(body: &Value, key: &str) -> Option<Value> {
    body["keys"]
        .as_array()?
        .iter()
        .find(|entry| entry["key"] == key)
        .map(|entry| entry["effective"].clone())
}

#[test]
fn settings_get_reports_values_sources_and_metadata() {
    let mut sandbox = Sandbox::new();
    write_conf(&sandbox, "mem_budget_bytes = auto\n");
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();

    let response = request(&address, "GET", "/api/v1/settings", "", b"");
    assert_eq!(response.status, 200, "{:?}", response.body);
    let keys = response.body["keys"].as_array().expect("keys");
    // 55 live keys; `config_doc`'s own guard keeps the table honest, this pins
    // that the route projects all of them.
    assert_eq!(keys.len(), 55, "the screen shows every documented key");
    let by_key = |key: &str| {
        keys.iter()
            .find(|entry| entry["key"] == key)
            .unwrap_or_else(|| panic!("missing {key}"))
            .clone()
    };
    let log_level = by_key("log_level");
    assert_eq!(log_level["source"], "file");
    assert_eq!(log_level["value"], "info");
    assert_eq!(log_level["kind"], "enum");
    assert_eq!(log_level["editable"], true);
    assert_eq!(log_level["label"], "日志级别");
    assert!(log_level["doc"].as_str().unwrap_or("").contains("级别"));
    // The console renders a Chinese label and puts the English key in the
    // tooltip, so every row has to carry both.
    assert!(
        keys.iter()
            .all(|entry| !entry["label"].as_str().unwrap_or("").is_empty()),
        "every key carries its console label"
    );

    // An environment override wins over the file and says so.
    assert_eq!(by_key("rescan_interval_ms")["source"], "env");
    // The http sandbox pins the memory probe (`Sandbox::env`), so the derived
    // budget — and `max_package_bytes` with it — does not follow the host's
    // free memory. Removing the pin makes the export-transfer cases 413 on a
    // loaded host (#285).
    let probe = by_key("probe_available_memory");
    assert_eq!(probe["source"], "env");
    assert_eq!(probe["effective"], json!(8589934592u64));
    // Derived keys have no value of their own but do have a running number.
    let streams = by_key("max_streams");
    assert_eq!(streams["source"], "derived");
    assert!(streams["effective"].as_u64().unwrap_or(0) > 0, "{streams}");

    // The three roots (and the install prefix) are read-only facts.
    for root in ["config_dir", "data_dir", "logs_dir"] {
        let entry = by_key(root);
        assert_eq!(entry["kind"], "path", "{entry}");
        assert_eq!(entry["editable"], false, "{entry}");
        assert!(entry["effective"].is_string(), "{entry}");
    }
    // The install prefix and the static-site root are optional: null until set.
    let install = by_key("install_dir");
    assert_eq!(install["kind"], "path");
    assert_eq!(install["editable"], false);
    let static_root = by_key("static_root");
    assert_eq!(static_root["kind"], "path");
    assert_eq!(static_root["editable"], false);
    assert!(static_root["effective"].is_null(), "{static_root}");
    let password = by_key("auth_passwd");
    assert_eq!(password["kind"], "password-hash");
    assert_eq!(password["editable"], true);
}

#[test]
fn a_save_rewrites_only_the_named_key_and_restarts_the_daemon() {
    let mut sandbox = Sandbox::new();
    write_conf(&sandbox, "mem_budget_bytes = auto\n");
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();
    let before = daemon_starts(&sandbox);
    assert_eq!(before, 1);

    let response = request(
        &address,
        "PUT",
        "/api/v1/settings",
        "application/json",
        br#"{"values":{"log_level":"debug"}}"#,
    );
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_eq!(response.body["saved"], json!(["log_level"]));
    assert_eq!(response.body["restarting"], true, "{:?}", response.body);

    let text = fs::read_to_string(sandbox.config.join("svcd.conf")).expect("conf");
    assert!(text.contains("# hand-written comment stays"), "{text}");
    assert!(text.contains("log_level = debug"), "{text}");
    assert!(!text.contains("log_level = info"), "{text}");
    assert_eq!(text.matches("log_level").count(), 1, "{text}");

    let address = wait_for_restarted(&sandbox, before, |address| {
        try_effective(address, "log_level").map(|value| value == json!("debug"))
    });
    let settings = request(&address, "GET", "/api/v1/settings", "", b"");
    let debug = settings.body["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .find(|entry| entry["key"] == "log_level")
        .expect("log_level")
        .clone();
    assert_eq!(debug["value"], "debug");
    assert_eq!(debug["source"], "file");
}

#[test]
fn invalid_read_only_and_conflicting_values_are_rejected_without_writing() {
    let mut sandbox = Sandbox::new();
    write_conf(&sandbox, "mem_budget_percent = 50\n");
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();
    let path = sandbox.config.join("svcd.conf");
    let before = fs::read_to_string(&path).expect("conf");

    for (body, needle) in [
        (r#"{"values":{"log_level":"loud"}}"#, "level"),
        (r#"{"values":{"config_dir":"/tmp/elsewhere"}}"#, "read-only"),
        (r#"{"values":{"not_a_key":"1"}}"#, "unknown key"),
        (
            r#"{"values":{"mem_budget_bytes":"1"}}"#,
            "mutually exclusive",
        ),
        (r#"{"values":{"log_level":true}}"#, "string or null"),
        (r#"{"values":{"max_streams":"48"}}"#, "read-only"),
        (
            r#"{"values":{"log_level":"info\ninstall_dir = /tmp/x"}}"#,
            "single line",
        ),
    ] {
        let response = request(
            &address,
            "PUT",
            "/api/v1/settings",
            "application/json",
            body.as_bytes(),
        );
        assert_eq!(response.status, 400, "{body}: {:?}", response.body);
        let message = response.body["error"]["message"].as_str().unwrap_or("");
        assert!(message.contains(needle), "{body}: {message}");
        assert_eq!(
            fs::read_to_string(&path).expect("conf"),
            before,
            "a rejected save must not touch the document: {body}"
        );
    }

    // Clearing the conflicting key is allowed and then the other one may be set.
    let cleared = request(
        &address,
        "PUT",
        "/api/v1/settings",
        "application/json",
        br#"{"values":{"mem_budget_percent":null}}"#,
    );
    assert_eq!(cleared.status, 200, "{:?}", cleared.body);
}

#[test]
fn settings_routes_enforce_auth_methods_and_json() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();

    assert_eq!(
        raw_request(&address, "DELETE", "/api/v1/settings", "", b"").status,
        405
    );
    let not_json = request(
        &address,
        "PUT",
        "/api/v1/settings",
        "application/json",
        b"not json",
    );
    assert_eq!(not_json.status, 400);
    let no_values = request(
        &address,
        "PUT",
        "/api/v1/settings",
        "application/json",
        b"{}",
    );
    assert_eq!(no_values.status, 400);

    let (address, _password) = enable_auth(&mut sandbox);
    assert_eq!(
        raw_request(&address, "GET", "/api/v1/settings", "", b"").status,
        401
    );
    assert_eq!(
        raw_request(
            &address,
            "PUT",
            "/api/v1/settings",
            "application/json",
            b"{}"
        )
        .status,
        401
    );
}

#[test]
fn the_password_can_be_set_and_cleared_over_the_settings_route() {
    let mut sandbox = Sandbox::new();
    write_conf(&sandbox, "");
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();

    let set = request_when_ready(
        &address,
        "PUT",
        "/api/v1/settings",
        "application/json",
        br#"{"values":{"auth_passwd":"hunter2hunter2"}}"#,
    );
    assert_eq!(set.status, 200, "{:?}", set.body);
    let text = fs::read_to_string(sandbox.config.join("svcd.conf")).expect("conf");
    assert!(text.contains("auth_passwd = pbkdf2-sha256$"), "{text}");
    assert!(
        !text.contains("hunter2"),
        "the hash is the only form on disk"
    );
    let address = wait_for_restarted(&sandbox, 1, |address| {
        try_request(address, "GET", "/api/v1/auth", "", b"")
            .ok()
            .map(|response| response.body["passwordSet"] == json!(true))
    });

    let cleared = request_when_ready(
        &address,
        "PUT",
        "/api/v1/settings",
        "application/json",
        br#"{"values":{"auth_passwd":null}}"#,
    );
    assert_eq!(cleared.status, 200, "{:?}", cleared.body);
    let text = fs::read_to_string(sandbox.config.join("svcd.conf")).expect("conf");
    assert!(!text.contains("auth_passwd"), "{text}");
    wait_for_restarted(&sandbox, 2, |address| {
        try_request(address, "GET", "/api/v1/auth", "", b"")
            .ok()
            .map(|response| response.body["passwordSet"] == json!(false))
    });
}

#[test]
fn svcd_restart_replaces_the_user_daemon() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_as_user();
    sandbox.wait_for_http();
    assert_eq!(daemon_starts(&sandbox), 1);

    eprintln!("CHECKPOINT before restart");
    let mut command = user_command(&sandbox);
    command.arg("restart");
    let output = support::run_bounded(&mut command, Duration::from_secs(30))
        .expect("svcd restart must finish: the old daemon stops and a new one starts");
    assert!(
        output.status.success(),
        "svcd restart: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let address = wait_for_restarted(&sandbox, 1, |address| {
        try_request(address, "GET", "/api/v1/version", "", b"")
            .ok()
            .map(|response| response.status == 200)
    });
    let version = request(&address, "GET", "/api/v1/version", "", b"");
    assert_eq!(version.status, 200, "{:?}", version.body);
}

#[test]
fn a_save_in_service_mode_saves_without_restarting() {
    let mut sandbox = Sandbox::new();
    write_conf(&sandbox, "");
    sandbox.start_daemon_with_env("127.0.0.1:0", &[("SVCD_TEST_PRIVILEGE", "privileged")]);
    let address = sandbox.wait_for_http();

    let response = request(
        &address,
        "PUT",
        "/api/v1/settings",
        "application/json",
        br#"{"values":{"log_level":"debug"}}"#,
    );
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_eq!(response.body["restarting"], false, "{:?}", response.body);
    let hint = response.body["hint"].as_str().unwrap_or("");
    assert!(hint.contains("service manager"), "{hint}");
    assert!(fs::read_to_string(sandbox.config.join("svcd.conf"))
        .expect("conf")
        .contains("log_level = debug"));
    // The daemon that is answering is still the original one.
    assert_eq!(daemon_starts(&sandbox), 1);
    assert_eq!(
        request(&address, "GET", "/api/v1/version", "", b"").status,
        200
    );
}

#[test]
fn the_effective_budget_matches_the_running_snapshot() {
    let mut sandbox = Sandbox::new();
    write_conf(&sandbox, "");
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();

    let saved = request(
        &address,
        "PUT",
        "/api/v1/settings",
        "application/json",
        br#"{"values":{"mem_budget_percent":"90"}}"#,
    );
    assert_eq!(saved.status, 200, "{:?}", saved.body);
    let address = wait_for_restarted(&sandbox, 1, |address| {
        try_effective(address, "mem_budget_bytes").map(|value| value.as_u64().unwrap_or(0) > 0)
    });

    let settings = request(&address, "GET", "/api/v1/settings", "", b"");
    let budget = settings.body["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .find(|entry| entry["key"] == "mem_budget_bytes")
        .expect("mem_budget_bytes")["effective"]
        .as_u64()
        .expect("a number");

    // `svc status --resources --json` prints the snapshot the daemon wrote, so
    // the two projections of one derivation must agree.
    let resources = sandbox.run_svc(&["status", "--resources", "--json"]);
    assert_eq!(resources.status.code(), Some(0));
    let snapshot: Value = serde_json::from_slice(&resources.stdout).expect("resources json");
    assert_eq!(
        snapshot["budget_quantized"].as_u64(),
        Some(budget),
        "{snapshot}"
    );
}
