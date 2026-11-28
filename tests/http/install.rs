//! install http tests.

use super::*;

#[test]
fn package_install_and_import_accept_chunked_bodies() {
    let mut sandbox = Sandbox::new();
    let package = signed_demo_package(&sandbox, 43);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let installed = raw_request_chunked(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
        &[],
    );
    assert_eq!(
        installed.status,
        200,
        "{}",
        String::from_utf8_lossy(&installed.body)
    );
    assert_eq!(json_body(&installed)["id"], "demo");

    let imported = raw_request_chunked(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &package,
        &[],
    );
    assert_eq!(
        imported.status,
        200,
        "{}",
        String::from_utf8_lossy(&imported.body)
    );
    assert_eq!(json_body(&imported)["id"], "demo");

    let config = raw_request_chunked(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"chunked config\n",
        &[],
    );
    assert_eq!(
        config.status,
        200,
        "{}",
        String::from_utf8_lossy(&config.body)
    );
    assert_eq!(json_body(&config)["configApplied"], true);
    assert_eq!(
        fs::read(plugin_dir(&sandbox, "demo").join("config")).expect("config"),
        b"chunked config\n"
    );
}

#[test]
fn package_upload_honours_expect_100_continue() {
    let mut sandbox = Sandbox::new();
    let package = signed_demo_package(&sandbox, 44);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "POST /api/v1/plugins HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nExpect: 100-continue\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        package.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let mut interim = Vec::new();
    let mut byte = [0u8; 1];
    while !interim.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("100 continue");
        interim.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&interim).starts_with("HTTP/1.1 100"),
        "{:?}",
        String::from_utf8_lossy(&interim)
    );
    stream.write_all(&package).expect("write body");
    let installed = read_raw_response(&mut stream);
    assert_eq!(
        installed.status,
        200,
        "{}",
        String::from_utf8_lossy(&installed.body)
    );

    let body = b"continue config\n";
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "POST /api/v1/import?config_id=demo HTTP/1.1\r\nHost: {}\r\nContent-Type: text/plain\r\nExpect: 100-continue\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let mut interim = Vec::new();
    while !interim.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("100 continue");
        interim.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&interim).starts_with("HTTP/1.1 100"),
        "{:?}",
        String::from_utf8_lossy(&interim)
    );
    stream.write_all(body).expect("write body");
    let imported = read_raw_response(&mut stream);
    assert_eq!(
        imported.status,
        200,
        "{}",
        String::from_utf8_lossy(&imported.body)
    );
    assert_eq!(json_body(&imported)["configApplied"], true);
}

#[test]
fn a_disconnected_package_upload_removes_its_staging_directory() {
    let mut sandbox = Sandbox::new();
    let package = signed_demo_package(&sandbox, 45);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let plugins = sandbox.data.join("plugins");
    let installed = raw_request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(
        installed.status,
        200,
        "{}",
        String::from_utf8_lossy(&installed.body)
    );
    let backup = raw_request(&address, "GET", "/api/v1/export/demo", "", b"");
    assert_eq!(
        backup.status,
        200,
        "{}",
        String::from_utf8_lossy(&backup.body)
    );

    // The .spk install, the .spk import and the .spkb import each stage into
    // the plugin tree; a disconnect must remove that directory.
    for (path, content) in [
        ("/api/v1/plugins", package.as_slice()),
        ("/api/v1/import", package.as_slice()),
        // The package was installed above, so the .spkb import must opt into
        // force before its nested package is staged; otherwise BackupSink
        // rejects it right after staging and the staging dir is already gone
        // when the test looks for it.
        ("/api/v1/import?force=true", backup.body.as_slice()),
    ] {
        let initial = content[..content.len().min(64 * 1024)].to_vec();
        let stalled = start_stalled_upload(
            &address,
            "POST",
            path,
            // Just past the bytes already sent: enough to stall, but under
            // the 8 MiB low-profile package cap so the upload is not refused
            // before it can stage.
            content.len() + 1024 * 1024,
            &initial,
        );
        wait_for_package_staging_with(&sandbox, &plugins, || {
            let mut probe = stalled.try_clone().expect("probe clone");
            probe.set_nonblocking(true).expect("probe nonblocking");
            let mut bytes = [0u8; 2048];
            let read = std::io::Read::read(&mut probe, &mut bytes).unwrap_or(0);
            format!("response: {:?}", String::from_utf8_lossy(&bytes[..read]))
        });
        drop(stalled);
        wait_for_no_package_staging(&sandbox, &plugins);
    }
}

#[test]
fn a_failed_install_removes_its_staging_directory() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[46u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    // "svc" is the reserved id: the package is refused after the HTTP layer has
    // already written its staging directory, so the cleanup path still runs.
    let package = plugin_bundle(&key, "svc", "1.0.0", None, false);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let response = raw_request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(
        response.status,
        400,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    wait_for_no_package_staging(&sandbox, &sandbox.data.join("plugins"));

    // The reserved id is refused while parsing the manifest, inside the face:
    // no control request happened, so there is no call number to name (#346).
    assert!(
        !response
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-request-id")),
        "a face-local refusal names no call: {:?}",
        response.headers
    );
}

/// The install/import upload used to take the connection out of the loop and
/// spawn a per-request thread. Both are stream handlers now.
#[cfg(target_os = "linux")]
#[test]
fn unix_package_uploads_have_no_takeover_thread() {
    let mut sandbox = Sandbox::new();
    let package = signed_demo_package(&sandbox, 47);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let pid = sandbox.daemon.as_ref().expect("running daemon").id();
    let idle = stable_thread_count(pid);
    let plugins = sandbox.data.join("plugins");
    let initial = package[..package.len().min(64 * 1024)].to_vec();

    for path in ["/api/v1/plugins", "/api/v1/import"] {
        let stalled = start_stalled_upload(
            &address,
            "POST",
            path,
            package.len() + 1024 * 1024,
            &initial,
        );
        wait_for_package_staging(&sandbox, &plugins);
        assert_eq!(
            stable_thread_count(pid),
            idle,
            "a package upload must not add a takeover thread ({path})"
        );
        drop(stalled);
        wait_for_no_package_staging(&sandbox, &plugins);
    }
}

#[test]
fn plugin_upload_list_and_delete() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[21u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_package(&key);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins?strategy=keep-old",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    assert_eq!(installed.body["id"], "demo");
    assert_eq!(installed.body["version"], "1.0.0");

    let services = request(&address, "GET", "/api/v1/services", "", b"");
    let plugin = services.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .find(|service| service["id"] == "demo")
        .expect("plugin service");
    assert_eq!(plugin["kind"], "plugin");

    let disabled = request(&address, "POST", "/api/v1/services/demo/disable", "", b"");
    assert_eq!(disabled.status, 200, "{}", disabled.body);
    assert_eq!(disabled.body["summary"]["enabled"], false);
    let enabled = request(&address, "POST", "/api/v1/services/demo/enable", "", b"");
    assert_eq!(enabled.status, 200, "{}", enabled.body);
    assert_eq!(enabled.body["summary"]["enabled"], true);
    let core_toggle = request(&address, "POST", "/api/v1/services/svc/disable", "", b"");
    assert_eq!(core_toggle.status, 409, "{}", core_toggle.body);

    let deleted = request(&address, "DELETE", "/api/v1/plugins/demo", "", b"");
    assert_eq!(deleted.status, 200, "{}", deleted.body);

    let core_delete = request(&address, "DELETE", "/api/v1/plugins/svc", "", b"");
    assert_eq!(core_delete.status, 409, "{}", core_delete.body);
    assert_eq!(core_delete.body["error"]["code"], "conflict");

    let after = request(&address, "GET", "/api/v1/services", "", b"");
    assert!(!after.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .any(|service| service["id"] == "demo"));

    let mut bad = package.clone();
    let needle = b"\"demo\"";
    let position = bad
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("name");
    bad[position + 1] = b'D';
    let bad_upload = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &bad,
    );
    assert_eq!(bad_upload.status, 400, "{}", bad_upload.body);

    let wrong_type = request(&address, "POST", "/api/v1/plugins", "text/plain", &package);
    assert_eq!(wrong_type.status, 400);
    assert_eq!(wrong_type.body["error"]["code"], "bad_request");
}

#[test]
fn purge_delete_removes_the_plugin_tree_and_logs() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[24u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_bundle(&key, "demo", "1.0.0", None, false);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);

    let log = sandbox.logs.join("demo.log");
    assert!(log.is_file(), "the service log should exist after start");

    let deleted = request(
        &address,
        "DELETE",
        "/api/v1/plugins/demo?purge=true",
        "",
        b"",
    );
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    assert!(!plugin_dir(&sandbox, "demo").exists());
    for _ in 0..40 {
        if !log.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!log.exists(), "purge must remove the service log");
}

#[test]
fn trust_upload_enables_a_signed_install() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[41u8; 32]);
    let package = plugin_package(&key);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    // Without a trust root the upload is rejected with an actionable error.
    let untrusted = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(untrusted.status, 400, "{}", untrusted.body);
    assert!(
        untrusted.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("svc trust add"),
        "{}",
        untrusted.body
    );

    let pem = pem_from_public(&key.verifying_key().to_bytes());
    let added = request(
        &address,
        "POST",
        "/api/v1/trust?key_id=test",
        "application/octet-stream",
        &pem,
    );
    assert_eq!(added.status, 200, "{}", added.body);
    assert_eq!(added.body["keyId"], "test");
    assert!(sandbox.trust.join("test.pub").is_file());

    let listed = request(&address, "GET", "/api/v1/trust", "", b"");
    assert_eq!(listed.status, 200);
    assert!(listed.body["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .any(|key| key == "test"));

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);

    let removed = request(&address, "DELETE", "/api/v1/trust/test", "", b"");
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert!(!sandbox.trust.join("test.pub").exists());
}

#[test]
fn default_install_strategy_is_replace() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[25u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let v1 = plugin_bundle(&key, "demo", "1.0.0", None, false);
    let v2 = plugin_bundle(&key, "demo", "1.1.0", None, false);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let first = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &v1,
    );
    assert_eq!(first.status, 200, "{}", first.body);
    let plugin = sandbox.data.join("plugins").join("demo");
    assert!(plugin.join("1.0.0").is_dir());

    let second = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &v2,
    );
    assert_eq!(second.status, 200, "{}", second.body);
    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.1.0"
    );
    assert!(plugin.join("1.1.0").is_dir());
    assert!(
        !plugin.join("1.0.0").exists(),
        "the default API install must replace the old version"
    );
}

#[test]
fn untrusted_upload_returns_bad_request_not_404() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[22u8; 32]);
    // Deliberately leave the trust directory empty: the package is signed by
    // `test`, but the installer has no `trust.d/test.pub`.
    let package = plugin_package(&key);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let response = request(
        &address,
        "POST",
        "/api/v1/plugins?strategy=replace",
        "application/octet-stream",
        &package,
    );
    assert_eq!(response.status, 400, "{}", response.body);
    assert_eq!(response.body["error"]["code"], "bad_request");
    let message = response.body["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("unknown keyId test"), "{message}");
    assert!(message.contains("trust.d"), "{message}");
}

#[test]
fn unsigned_install_requires_the_conf_switch() {
    let mut sandbox = Sandbox::new();
    let package = unsigned_plugin_bundle("demo");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let rejected = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(rejected.status, 400, "{}", rejected.body);
    let message = rejected.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    // #309: the answer names the fact (unsigned) and both ways forward, not the
    // tar rule the installer happens to enforce.
    assert!(message.contains("package is not signed"), "{message}");
    assert!(message.contains("allow_insecure_install"), "{message}");
    assert!(message.contains("not signature.json"), "{message}");
    assert!(!message.contains("must be the second entry"), "{message}");
}

/// #312: a plugin installed on a second root is told that root, and it is not
/// handed the daemon's config/data trees.
#[test]
fn a_plugin_on_a_second_root_learns_its_own_directory() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[57u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = env_report_plugin_bundle(&key, "envreported");
    let default_root = sandbox.data.join("plugins");
    let second = sandbox.root.join("sd").join("plugins");
    fs::create_dir_all(&second).expect("second root");
    let log_dir = sandbox.logs.clone();
    let data_dir = sandbox.data.clone();
    let config_dir = sandbox.config.clone();
    fs::write(
        sandbox.config.join("svcd.conf"),
        format!(
            "bind = 127.0.0.1:0\nplugins_dir = {},{}\n",
            default_root.display(),
            second.display()
        ),
    )
    .expect("write svcd.conf");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let installed = raw_request(
        &address,
        "POST",
        &format!(
            "/api/v1/plugins?root={}",
            second.display().to_string().replace('/', "%2F")
        ),
        "application/octet-stream",
        &package,
    );
    assert_eq!(
        installed.status,
        200,
        "{}",
        String::from_utf8_lossy(&installed.body)
    );
    let started = request(
        &address,
        "POST",
        "/api/v1/services/envreported/start",
        "",
        b"",
    );
    assert_eq!(started.status, 200, "{:?}", started.body);

    let log = sandbox.logs.join("envreported.log");
    wait_until(
        &sandbox,
        "the plugin reports its environment",
        || {
            fs::read_to_string(&log)
                .map(|text| text.contains("env data_dir="))
                .unwrap_or(false)
        },
        || format!("log: {:?}", fs::read_to_string(&log)),
    );
    let text = fs::read_to_string(&log).expect("read log");

    assert!(
        text.contains(&format!(
            "env plugin_dir={}",
            second.join("envreported").display()
        )),
        "the plugin must learn the root it landed on: {text}"
    );
    assert!(
        text.contains(&format!("env log_dir={}", log_dir.display())),
        "the log tree stays available: {text}"
    );
    assert!(
        text.contains("env temp_dir="),
        "the temp tree stays available: {text}"
    );
    assert!(
        text.contains("env data_dir=none"),
        "no daemon data root: {text}"
    );
    assert!(
        text.contains("env config_dir=none"),
        "no daemon config root (svcd.conf, trust.d): {text}"
    );
    // `temp_dir` lives under `data_dir`, so compare the reported lines rather
    // than searching for the path as a substring.
    assert!(
        !text.contains(&format!("env data_dir={}", data_dir.display()))
            && !text.contains(&format!("env config_dir={}", config_dir.display())),
        "the plugin must not see the daemon roots: {text}"
    );
}

#[test]
fn installing_a_package_for_another_platform_is_400() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[31u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let (mut manifest, files) = plugin_parts("other", "1.0.0", None, false);
    manifest["platform"] = json!("other-triple");
    let package = signed_package(&key, manifest, &files);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 400, "{}", installed.body);
    assert!(
        installed.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("platform"),
        "{}",
        installed.body
    );
}

#[test]
fn unsigned_install_is_accepted_when_the_conf_switch_is_on() {
    let mut sandbox = Sandbox::new();
    let package = unsigned_plugin_bundle("demo");
    sandbox.allow_insecure_install();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    assert_eq!(installed.body["id"], "demo");
    assert_eq!(installed.body["version"], "1.0.0");
    let version_dir = sandbox.data.join("plugins").join("demo").join("1.0.0");
    assert!(version_dir.join("manifest.json").is_file());
    assert!(
        !version_dir.join("signature.json").exists(),
        "an insecure install must not invent a signature file"
    );

    // The version directory has no signature, so it cannot be exported as a
    // signed .spk; the error must say why instead of failing on a missing file.
    let exported = request(&address, "GET", "/api/v1/export/demo/spk", "", b"");
    assert_eq!(exported.status, 500, "{}", exported.body);
    let message = exported.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("no signature"), "{message}");
}

#[test]
fn insecure_install_accepts_an_unknown_signing_key() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[42u8; 32]);
    // Deliberately leave the trust directory empty: `test` is not a trust root.
    let package = plugin_package(&key);
    sandbox.allow_insecure_install();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    assert_eq!(installed.body["id"], "demo");
}

/// Re-importing the same backup version without force is a conflict (409), not
/// the 500 the message matcher used to guess: the raiser knows the category.
#[test]
fn importing_an_installed_backup_without_force_is_409() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let backup = unsigned_backup("demo");

    let first = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &backup,
    );
    assert_eq!(
        first.status,
        200,
        "{}",
        String::from_utf8_lossy(&first.body)
    );

    let again = raw_request(
        &address,
        "POST",
        "/api/v1/import",
        "application/octet-stream",
        &backup,
    );
    assert_eq!(
        again.status,
        409,
        "{}",
        String::from_utf8_lossy(&again.body)
    );
}

/// An unknown backup schema is the client's error (400), not a server fault.
#[test]
fn importing_a_backup_with_an_unknown_schema_is_400() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let backup = unsigned_backup_with_schema("demo", 2);

    let response = raw_request(
        &address,
        "POST",
        "/api/v1/import",
        "application/octet-stream",
        &backup,
    );
    assert_eq!(
        response.status,
        400,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
}

#[test]
fn unsigned_import_requires_the_conf_switch() {
    let mut sandbox = Sandbox::new();
    let backup = unsigned_backup("demo");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let rejected = request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &backup,
    );
    assert_eq!(rejected.status, 400, "{}", rejected.body);
    let message = rejected.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("signature.json"), "{message}");
}

#[test]
fn unsigned_import_is_accepted_when_the_conf_switch_is_on() {
    let mut sandbox = Sandbox::new();
    let backup = unsigned_backup("demo");
    sandbox.allow_insecure_install();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let imported = request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &backup,
    );
    assert_eq!(imported.status, 200, "{}", imported.body);
    assert_eq!(imported.body["id"], "demo");
    assert_eq!(imported.body["version"], "1.0.0");
}

#[test]
fn export_import_and_logs() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[32u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_bundle(&key, "demo", "1.0.0", Some(b"good template\n"), false);
    let plain = plugin_bundle(&key, "plain", "1.0.0", None, false);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    assert_eq!(
        request(
            &address,
            "POST",
            "/api/v1/plugins",
            "application/octet-stream",
            &package
        )
        .status,
        200
    );
    assert_eq!(
        request(
            &address,
            "POST",
            "/api/v1/plugins",
            "application/octet-stream",
            &plain
        )
        .status,
        200
    );

    let services = request(&address, "GET", "/api/v1/services", "", b"");
    let summaries = services.body["services"].as_array().expect("services");
    let demo = summaries
        .iter()
        .find(|service| service["id"] == "demo")
        .expect("demo");
    let plain = summaries
        .iter()
        .find(|service| service["id"] == "plain")
        .expect("plain");
    assert_eq!(demo["hasConfig"], true);
    assert_eq!(plain["hasConfig"], false);

    let directory = plugin_dir(&sandbox, "demo");
    let active = directory.join("config");

    let spkb = raw_request(&address, "GET", "/api/v1/export/demo", "", b"");
    assert_eq!(spkb.status, 200, "{}", String::from_utf8_lossy(&spkb.body));
    assert_eq!(
        raw_header_values(&spkb, "content-type"),
        vec!["application/octet-stream"]
    );
    assert!(raw_header_values(&spkb, "content-disposition")[0].contains("demo-1.0.0.spkb"));
    assert_eq!(
        raw_header_values(&spkb, "content-length"),
        vec![spkb.body.len().to_string()],
        "the export advertises the exact tar length"
    );
    assert_eq!(raw_header_values(&spkb, "cache-control"), vec!["no-store"]);
    let names = tar_names(&spkb.body);
    assert_eq!(names.first().map(String::as_str), Some("backup.json"));
    assert!(names.iter().any(|name| name == "plugin.spk"), "{names:?}");
    assert!(names.iter().any(|name| name == "config"), "{names:?}");

    let spk = raw_request(&address, "GET", "/api/v1/export/demo/spk", "", b"");
    assert_eq!(spk.status, 200, "{}", String::from_utf8_lossy(&spk.body));
    assert!(raw_header_values(&spk, "content-disposition")[0].contains("demo-1.0.0.spk"));
    let names = tar_names(&spk.body);
    assert_eq!(names.first().map(String::as_str), Some("manifest.json"));
    assert!(
        names.iter().any(|name| name == "signature.json"),
        "{names:?}"
    );

    let config_export = raw_request(&address, "GET", "/api/v1/export/demo/config", "", b"");
    assert_eq!(config_export.status, 200);
    assert_eq!(config_export.body, b"good template\n");

    let raw_import = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"imported good\n",
    );
    assert_eq!(
        raw_import.status,
        200,
        "{}",
        String::from_utf8_lossy(&raw_import.body)
    );
    assert_eq!(json_body(&raw_import)["configApplied"], true);
    assert_eq!(fs::read(&active).expect("config"), b"imported good\n");

    // A backup taken now must contain the active config, not the template.
    let active_backup = raw_request(&address, "GET", "/api/v1/export/demo", "", b"");
    assert_eq!(active_backup.status, 200);
    let changed = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"changed\n",
    );
    assert_eq!(changed.status, 200);
    assert_eq!(fs::read(&active).expect("config"), b"changed\n");
    let restored = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &active_backup.body,
    );
    assert_eq!(restored.status, 200);
    assert_eq!(json_body(&restored)["configApplied"], true);
    assert_eq!(fs::read(&active).expect("config"), b"imported good\n");

    let missing_id = raw_request(&address, "POST", "/api/v1/import", "text/plain", b"no id\n");
    assert_eq!(missing_id.status, 400);

    let backup_import = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &spkb.body,
    );
    assert_eq!(
        backup_import.status,
        200,
        "{}",
        String::from_utf8_lossy(&backup_import.body)
    );
    let backup_body = json_body(&backup_import);
    assert_eq!(backup_body["id"], "demo");
    assert_eq!(backup_body["configApplied"], true);
    assert_eq!(fs::read(&active).expect("config"), b"good template\n");

    let spk_import = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &package,
    );
    assert_eq!(
        spk_import.status,
        200,
        "{}",
        String::from_utf8_lossy(&spk_import.body)
    );
    assert_eq!(json_body(&spk_import)["id"], "demo");

    let mut bad = package.clone();
    let needle = b"\"demo\"";
    let position = bad
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("name");
    bad[position + 1] = b'D';
    let bad_import = raw_request(
        &address,
        "POST",
        "/api/v1/import",
        "application/octet-stream",
        &bad,
    );
    assert_eq!(
        bad_import.status,
        400,
        "{}",
        String::from_utf8_lossy(&bad_import.body)
    );

    let no_config = raw_request(&address, "GET", "/api/v1/config/plain", "", b"");
    assert_eq!(no_config.status, 404);

    fs::write(sandbox.logs.join("demo.log"), "one\ntwo\nthree\n").expect("write log");
    let tail = raw_request(&address, "GET", "/api/v1/logs/demo?lines=2", "", b"");
    assert_eq!(tail.status, 200);
    assert_eq!(tail.body, b"two\nthree\n");
    assert_eq!(
        raw_header_values(&tail, "content-type"),
        vec!["text/plain; charset=utf-8"]
    );
    let too_many = raw_request(&address, "GET", "/api/v1/logs/demo?lines=2001", "", b"");
    assert_eq!(too_many.status, 400);
    let no_log = raw_request(&address, "GET", "/api/v1/logs/plain", "", b"");
    assert_eq!(no_log.status, 404);

    fs::write(
        sandbox.logs.join("demo.log"),
        "2023-11-14T22:13:20.000Z INFO  svcd quiet\n\
         2023-11-14T22:13:21.000Z WARN  svcd careful\n\
         2023-11-14T22:13:22.000Z ERROR svcd boom\n\
         old format line\n",
    )
    .expect("write levelled log");
    let warn = raw_request(&address, "GET", "/api/v1/logs/demo?level=warn", "", b"");
    assert_eq!(warn.status, 200);
    assert_eq!(
        warn.body,
        b"2023-11-14T22:13:21.000Z WARN  svcd careful\n\
          2023-11-14T22:13:22.000Z ERROR svcd boom\n\
          old format line\n"
    );
    let bad_level = raw_request(&address, "GET", "/api/v1/logs/demo?level=loud", "", b"");
    assert_eq!(bad_level.status, 400);

    let big = "x".repeat(1024 * 1024 + 100);
    fs::write(sandbox.logs.join("demo.log"), &big).expect("write big log");
    let capped = raw_request(&address, "GET", "/api/v1/logs/demo?lines=200", "", b"");
    assert_eq!(capped.status, 200);
    assert!(
        capped.body.len() <= 1024 * 1024,
        "log response was {} bytes",
        capped.body.len()
    );
}

/// ADR-0030: the streamed upload decides the root before writing the first
/// payload byte, so the tree is staged where it will be renamed into.
#[test]
fn a_streamed_install_lands_on_the_root_the_request_names() {
    let mut sandbox = Sandbox::new();
    let package = signed_demo_package(&sandbox, 19);
    let default_root = sandbox.data.join("plugins");
    let second = sandbox.root.join("sd").join("plugins");
    fs::create_dir_all(&second).expect("second root");
    fs::write(
        sandbox.config.join("svcd.conf"),
        format!(
            "bind = 127.0.0.1:0\nplugins_dir = {},{}\n",
            default_root.display(),
            second.display()
        ),
    )
    .expect("write svcd.conf");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let installed = raw_request(
        &address,
        "POST",
        &format!(
            "/api/v1/plugins?root={}",
            second.display().to_string().replace('/', "%2F")
        ),
        "application/octet-stream",
        &package,
    );
    assert_eq!(
        installed.status,
        200,
        "{}",
        String::from_utf8_lossy(&installed.body)
    );
    assert!(
        second.join("demo/current").is_file(),
        "the plugin must land on the requested root"
    );
    assert!(
        !default_root.join("demo").exists(),
        "staging and install must not touch the other root"
    );
    let locations: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(sandbox.data.join("plugin-locations.json"))
            .expect("the root is recorded"),
    )
    .expect("locations json");
    assert_eq!(
        locations["plugins"]["demo"],
        serde_json::json!(second.display().to_string()),
        "the record names the root the plugin landed on"
    );

    // #307: the install answered from a service built before the root was
    // recorded, so the config path must come from the tree the package landed
    // in — otherwise the console hides the 配置 button for a second root.
    let services = request(&address, "GET", "/api/v1/services", "", b"");
    let demo = services.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .find(|item| item["id"] == "demo")
        .expect("demo is listed");
    assert_eq!(demo["hasConfig"], true, "{demo}");
    assert_eq!(
        demo["installDir"],
        serde_json::json!(second.join("demo").display().to_string()),
        "the list names the directory the plugin was installed into"
    );
    assert_eq!(
        demo["installRootIndex"], 2,
        "the console numbers the roots the way the settings list does: {demo}"
    );
    let config = raw_request(&address, "GET", "/api/v1/config/demo", "", b"");
    assert_eq!(
        config.status,
        200,
        "the active config is on the root the plugin was installed to: {}",
        String::from_utf8_lossy(&config.body)
    );

    // The same id cannot be installed into a different root: that is the state
    // the location index exists to prevent.
    let refused = raw_request(
        &address,
        "POST",
        &format!(
            "/api/v1/plugins?root={}",
            default_root.display().to_string().replace('/', "%2F")
        ),
        "application/octet-stream",
        &package,
    );
    assert_eq!(
        refused.status,
        409,
        "{}",
        String::from_utf8_lossy(&refused.body)
    );
    assert!(!default_root.join("demo").exists(), "no second copy");

    // A root that is not in plugins_dir is a typo, not a request to create it.
    let unknown = raw_request(
        &address,
        "POST",
        "/api/v1/plugins?root=%2Ftmp%2Fnowhere",
        "application/octet-stream",
        &package,
    );
    assert_eq!(
        unknown.status,
        400,
        "{}",
        String::from_utf8_lossy(&unknown.body)
    );
}

/// #294: a plugin whose volume is out is not "unknown". Export and uninstall
/// say which root it needs, and only an explicit `force` forgets the record —
/// without touching the volume.
#[test]
fn a_plugin_on_an_unavailable_root_is_a_conflict_not_a_missing_plugin() {
    let mut sandbox = Sandbox::new();
    let package = signed_demo_package(&sandbox, 23);
    let second = sandbox.root.join("sd").join("plugins");
    fs::create_dir_all(&second).expect("second root");
    fs::write(
        sandbox.config.join("svcd.conf"),
        format!(
            "bind = 127.0.0.1:0\nplugins_dir = {},{}\n",
            sandbox.data.join("plugins").display(),
            second.display()
        ),
    )
    .expect("write svcd.conf");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = raw_request(
        &address,
        "POST",
        &format!(
            "/api/v1/plugins?root={}",
            second.display().to_string().replace('/', "%2F")
        ),
        "application/octet-stream",
        &package,
    );
    assert_eq!(
        installed.status,
        200,
        "{}",
        String::from_utf8_lossy(&installed.body)
    );

    let pulled = sandbox.root.join("sd-pulled");
    fs::rename(&second, &pulled).expect("pull the card");

    let export = raw_request(&address, "GET", "/api/v1/export/demo/spk", "", b"");
    assert_eq!(
        export.status,
        409,
        "{}",
        String::from_utf8_lossy(&export.body)
    );
    let text = String::from_utf8_lossy(&export.body);
    assert!(text.contains("root_unavailable"), "{text}");
    assert!(text.contains("sd"), "{text}");

    let refused = raw_request(&address, "DELETE", "/api/v1/plugins/demo", "", b"");
    assert_eq!(
        refused.status,
        409,
        "{}",
        String::from_utf8_lossy(&refused.body)
    );
    let text = String::from_utf8_lossy(&refused.body);
    assert!(text.contains("not available"), "{text}");

    let forced = raw_request(
        &address,
        "DELETE",
        "/api/v1/plugins/demo?force=true",
        "",
        b"",
    );
    assert_eq!(
        forced.status,
        200,
        "{}",
        String::from_utf8_lossy(&forced.body)
    );
    assert!(
        pulled.join("demo/current").is_file(),
        "force must leave the volume alone"
    );
}
