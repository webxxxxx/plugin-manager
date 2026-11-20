//! config http tests.

use super::*;

#[test]
fn compat_endpoint_reports_the_host_facts() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let response = request(&address, "GET", "/api/v1/compat", "", b"");
    assert_eq!(response.status, 200);
    assert!(response.body["platform"].is_string(), "{}", response.body);
    assert!(response.body["device"].is_null(), "{}", response.body);
    assert!(
        response.body["marker_path"].is_string(),
        "{}",
        response.body
    );

    fs::write(sandbox.config.join("compat.conf"), "device = sr6110\n").expect("marker");
    let response = request(&address, "GET", "/api/v1/compat", "", b"");
    assert_eq!(response.status, 200);
    assert_eq!(response.body["device"], "sr6110", "{}", response.body);
}

#[test]
fn explicit_conf_is_preferred_over_program_and_config() {
    let mut sandbox = Sandbox::new();
    let explicit = sandbox.root.join("override.conf");
    fs::write(
        &explicit,
        format!(
            "bind = 127.0.0.1:0\ndata_dir = {}\n",
            sandbox.root.join("explicit-data").display()
        ),
    )
    .expect("write explicit conf");
    // A broken program-dir document is the strongest decoy: choosing it would
    // fail bootstrap instead of merely loading a different tree.
    fs::write(sandbox.root.join("svcd.conf"), "bind = nope\n")
        .expect("write program-dir decoy conf");

    sandbox.start_daemon_with_conf(&explicit);
    let address = sandbox.wait_for_http();
    let response = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(
        response.status,
        200,
        "explicit --conf must win over the program-dir config: {}",
        sandbox.daemon_log()
    );
}

#[test]
fn default_conf_is_created_in_the_config_dir() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_http();

    let path = sandbox.config.join("svcd.conf");
    let text = fs::read_to_string(&path).expect("generated svcd.conf");
    assert!(text.contains("bind = 127.0.0.1:8787"), "{text}");
    assert!(text.contains("allow_insecure_install = false"), "{text}");
    assert!(
        text.contains(&format!("config_dir = {}", sandbox.config.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("data_dir = {}", sandbox.data.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("logs_dir = {}", sandbox.logs.display())),
        "{text}"
    );
    // Derived names are documented in comments, not written as keys.
    assert!(text.contains("#   plugins    = "), "{text}");
    assert!(text.contains("#   services.d = "), "{text}");
    assert!(text.contains("#   run        = "), "{text}");
    assert!(!text.contains("\nplugins_dir = "), "{text}");
    assert!(!text.contains("\nservices_dir = "), "{text}");
}

#[test]
fn existing_conf_without_roots_is_filled_and_used() {
    let mut sandbox = Sandbox::new();
    fs::write(
        sandbox.config.join("svcd.conf"),
        "bind = 127.0.0.1:0\nplugins_dir = /ignored/plugins\n",
    )
    .expect("write svcd.conf");

    sandbox.start_daemon();
    sandbox.wait_for_http();

    let text = fs::read_to_string(sandbox.config.join("svcd.conf")).expect("filled conf");
    assert!(
        text.contains(&format!("config_dir = {}\n", sandbox.config.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("data_dir = {}\n", sandbox.data.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("logs_dir = {}\n", sandbox.logs.display())),
        "{text}"
    );
    let log = sandbox.daemon_log();
    assert!(log.contains("plugins_dir"), "{log}");
    assert!(log.contains("ignored"), "{log}");
}

#[test]
fn configured_port_and_paths_come_from_svcd_conf() {
    let mut sandbox = Sandbox::new();
    let data = sandbox.root.join("configured-data");
    // The environment root wins over the document (and matches it here), so
    // the daemon, its port file and the installed plugin all live in one tree.
    sandbox.data = data.clone();
    let services = sandbox.config.join("services.d");
    fs::create_dir_all(&services).expect("configured services");
    let (exec, args) = if cfg!(windows) {
        (r"C:\Windows\System32\cmd.exe", json!(["/c", "exit", "0"]))
    } else {
        ("/bin/true", json!([]))
    };
    fs::write(
        services.join("custom.json"),
        serde_json::to_vec(&json!({
            "schema": 1,
            "id": "custom",
            "version": "1.0.0",
            "desc": "configured core service",
            "protocol": 1,
            "exec": {"path": exec, "args": args, "ready": "exec"}
        }))
        .expect("core definition"),
    )
    .expect("write core definition");
    fs::write(
        sandbox.config.join("svcd.conf"),
        format!("bind = 127.0.0.1:0\ndata_dir = {}\n", data.display()),
    )
    .expect("write svcd.conf");

    let key = SigningKey::from_bytes(&[23u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_package(&key);

    sandbox.start_daemon_without_http_port();
    let address = sandbox.wait_for_http();

    let services_response = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(services_response.status, 200);
    let ids: Vec<&str> = services_response.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .filter_map(|service| service["id"].as_str())
        .collect();
    assert!(ids.contains(&"custom"), "{ids:?}");

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins?strategy=replace",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    assert!(
        data.join("plugins").join("demo").join("1.0.0").is_dir(),
        "plugin did not use data_dir"
    );
    assert!(
        !sandbox
            .root
            .join("data")
            .join("plugins")
            .join("demo")
            .exists(),
        "plugin used the old default data directory"
    );
}

#[test]
fn config_endpoints_read_write_reset_and_reject_bad_checks() {
    let (sandbox, address) = daemon_with_demo_plugin(31);
    let directory = plugin_dir(&sandbox, "demo");
    let active = directory.join("config");
    assert_eq!(fs::read(&active).expect("config"), b"good template\n");

    let read = raw_request(&address, "GET", "/api/v1/config/demo", "", b"");
    assert_eq!(read.status, 200);
    assert_eq!(read.body, b"good template\n");
    assert_eq!(raw_header_values(&read, "content-type"), vec!["text/plain"]);

    let good = raw_request(
        &address,
        "PUT",
        "/api/v1/config/demo",
        "text/plain",
        b"good config\n",
    );
    assert_eq!(good.status, 200, "{}", String::from_utf8_lossy(&good.body));
    assert_eq!(fs::read(&active).expect("config"), b"good config\n");
    assert_eq!(
        fs::read(directory.join("config.last-good")).expect("last-good"),
        b"good template\n"
    );

    let bad = raw_request(
        &address,
        "PUT",
        "/api/v1/config/demo",
        "text/plain",
        b"bad config\n",
    );
    assert_eq!(bad.status, 409, "{}", String::from_utf8_lossy(&bad.body));
    assert_eq!(fs::read(&active).expect("config"), b"good config\n");

    let reset = raw_request(&address, "POST", "/api/v1/config/demo/reset", "", b"");
    assert_eq!(
        reset.status,
        200,
        "{}",
        String::from_utf8_lossy(&reset.body)
    );
    assert_eq!(fs::read(&active).expect("config"), b"good template\n");
    assert_eq!(
        fs::read(directory.join("config.last-good")).expect("last-good"),
        b"good config\n"
    );

    let bad_import = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"bad config\n",
    );
    assert_eq!(
        bad_import.status,
        409,
        "{}",
        String::from_utf8_lossy(&bad_import.body)
    );

    let too_large = vec![b'x'; 1024 * 1024 + 1];
    let large = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        &too_large,
    );
    assert_eq!(
        large.status,
        413,
        "{}",
        String::from_utf8_lossy(&large.body)
    );
}

#[test]
fn config_endpoints_are_plugin_only_and_enforce_methods() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    for (method, path) in [
        ("GET", "/api/v1/config/demo"),
        ("PUT", "/api/v1/config/demo"),
        ("POST", "/api/v1/config/demo/reset"),
        ("GET", "/api/v1/export/demo"),
        ("GET", "/api/v1/export/demo/config"),
        ("GET", "/api/v1/logs/demo"),
    ] {
        let response = raw_request(&address, method, path, "", b"");
        assert_eq!(
            response.status,
            404,
            "{} {} -> {}",
            method,
            path,
            String::from_utf8_lossy(&response.body)
        );
    }

    let import_core = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"good\n",
    );
    assert_eq!(
        import_core.status,
        404,
        "{}",
        String::from_utf8_lossy(&import_core.body)
    );

    let key = SigningKey::from_bytes(&[33u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let core_package = plugin_bundle(&key, "demo", "1.0.0", None, false);
    let core_import = raw_request(
        &address,
        "POST",
        "/api/v1/import",
        "application/octet-stream",
        &core_package,
    );
    assert_eq!(
        core_import.status,
        404,
        "{}",
        String::from_utf8_lossy(&core_import.body)
    );

    let reset_get = raw_request(&address, "GET", "/api/v1/config/demo/reset", "", b"");
    assert_eq!(reset_get.status, 405);
    assert_eq!(raw_header_values(&reset_get, "allow"), vec!["POST"]);

    let config_post = raw_request(&address, "POST", "/api/v1/config/demo", "", b"");
    assert_eq!(config_post.status, 405);
    assert_eq!(raw_header_values(&config_post, "allow"), vec!["GET, PUT"]);

    let export_post = raw_request(&address, "POST", "/api/v1/export/demo", "", b"");
    assert_eq!(export_post.status, 405);
    assert_eq!(raw_header_values(&export_post, "allow"), vec!["GET"]);

    let import_get = raw_request(&address, "GET", "/api/v1/import", "", b"");
    assert_eq!(import_get.status, 405);
}
