//! The buffered (CLI `transfer::import`) and streaming (HTTP `BackupSink`)
//! `.spkb` imports share `manage::check::backup`; these tests drive both
//! surfaces with the same bytes and compare category, text and warnings.

use std::path::PathBuf;
use std::process::{Command, Output};

use super::*;

/// Add the `plugin` multicall entry to an HTTP sandbox (the harness links only
/// `svcd`, `svc` and `mcp`), so the buffered CLI path runs against the same
/// daemon and data directory as the streaming HTTP path.
fn plugin_entry(sandbox: &Sandbox) -> PathBuf {
    let plugin = sandbox.root.join(if cfg!(windows) {
        "plugin.exe"
    } else {
        "plugin"
    });
    if !plugin.exists() {
        super::support::install_multicall(&sandbox.svcd, &[&plugin]);
    }
    plugin
}

fn cli_import(sandbox: &Sandbox, name: &str, backup: &[u8]) -> Output {
    let path = sandbox.root.join(name);
    fs::write(&path, backup).expect("write backup");
    let mut command = Command::new(plugin_entry(sandbox));
    sandbox.env(&mut command);
    command.arg("import").arg(&path);
    command.output().expect("run plugin import")
}

fn cli_error(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .trim()
        .strip_prefix("plugin: ")
        .unwrap_or_default()
        .to_string()
}

fn http_error(response: &RawResponse) -> String {
    json_body(response)["error"]["message"]
        .as_str()
        .expect("error message")
        .to_string()
}

fn spkb(
    id: &str,
    version: &str,
    schema: u32,
    config: Option<&[u8]>,
    config_sha256: Option<&str>,
) -> Vec<u8> {
    let (manifest, files) = plugin_parts(id, version, None, false);
    let package = unsigned_package(manifest, &files);
    let mut metadata = json!({"schema": schema, "id": id, "version": version});
    if let Some(hash) = config_sha256 {
        metadata["configSha256"] = json!(hash);
    }
    wrap(
        &serde_json::to_vec(&metadata).expect("backup metadata"),
        &package,
        config,
    )
}

fn wrap(metadata: &[u8], package: &[u8], config: Option<&[u8]>) -> Vec<u8> {
    let mut tar = Vec::new();
    let mut entries: Vec<(&str, &[u8], u32)> = vec![
        ("backup.json", metadata, 0o644),
        ("plugin.spk", package, 0o644),
    ];
    if let Some(config) = config {
        entries.push(("config", config, 0o600));
    }
    for (path, data, mode) in entries {
        tar.extend_from_slice(&header(path, data.len(), mode));
        tar.extend_from_slice(data);
        tar.extend(std::iter::repeat_n(0, padding(data.len())));
    }
    tar.extend(std::iter::repeat_n(0, 1024));
    tar
}

/// Every validation failure must reach both surfaces with the same category
/// (HTTP status / CLI failure) and the same detail text.
#[test]
fn both_import_paths_reject_the_same_bad_backups() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let package = unsigned_plugin_bundle("demo");
    let bad_json = b"{ not json".as_slice();

    let cases: Vec<(&str, Vec<u8>, u16)> = vec![
        ("schema", spkb("demo", "1.0.0", 2, None, None), 400),
        ("json", wrap(bad_json, &package, None), 400),
        (
            "id",
            wrap(
                &serde_json::to_vec(&json!({"schema": 1, "id": "other", "version": "1.0.0"}))
                    .expect("backup metadata"),
                &package,
                None,
            ),
            400,
        ),
        (
            "version",
            wrap(
                &serde_json::to_vec(&json!({"schema": 1, "id": "demo", "version": "2.0.0"}))
                    .expect("backup metadata"),
                &package,
                None,
            ),
            400,
        ),
        (
            "config-hash",
            spkb(
                "demo",
                "1.0.0",
                1,
                Some(b"one"),
                Some("0".repeat(64).as_str()),
            ),
            400,
        ),
    ];

    for (name, backup, status) in cases {
        let http = raw_request(
            &address,
            "POST",
            "/api/v1/import",
            "application/octet-stream",
            &backup,
        );
        assert_eq!(
            http.status,
            status,
            "{name}: {}",
            String::from_utf8_lossy(&http.body)
        );
        let cli = cli_import(&sandbox, &format!("{name}.spkb"), &backup);
        assert_eq!(
            cli.status.code(),
            Some(1),
            "{name}: {}",
            String::from_utf8_lossy(&cli.stderr)
        );
        assert_eq!(cli_error(&cli), http_error(&http), "{name}");
    }
}

/// The same installed version without force is a 409 on the streaming path and
/// the same message on the buffered path.
#[test]
fn both_import_paths_report_the_same_conflict() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let backup = spkb("demo", "1.0.0", 1, None, None);

    let installed = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &backup,
    );
    assert_eq!(
        installed.status,
        200,
        "{}",
        String::from_utf8_lossy(&installed.body)
    );

    let http = raw_request(
        &address,
        "POST",
        "/api/v1/import",
        "application/octet-stream",
        &backup,
    );
    assert_eq!(http.status, 409, "{}", String::from_utf8_lossy(&http.body));

    let cli = cli_import(&sandbox, "conflict.spkb", &backup);
    assert_eq!(
        cli.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert_eq!(cli_error(&cli), http_error(&http));
}

/// Importing another version warns on both surfaces, with the same text.
#[test]
fn both_import_paths_report_the_same_version_warning() {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let v1 = spkb("demo", "1.0.0", 1, None, None);
    let older = spkb("demo", "0.9.0", 1, None, None);

    let installed = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &v1,
    );
    assert_eq!(
        installed.status,
        200,
        "{}",
        String::from_utf8_lossy(&installed.body)
    );

    let cli = cli_import(&sandbox, "older.spkb", &older);
    assert_eq!(
        cli.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_warning = String::from_utf8_lossy(&cli.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("plugin: warning: "))
        .expect("CLI warning")
        .to_string();

    let reinstalled = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &v1,
    );
    assert_eq!(
        reinstalled.status,
        200,
        "{}",
        String::from_utf8_lossy(&reinstalled.body)
    );

    let http = raw_request(
        &address,
        "POST",
        "/api/v1/import",
        "application/octet-stream",
        &older,
    );
    assert_eq!(http.status, 200, "{}", String::from_utf8_lossy(&http.body));
    let body = json_body(&http);
    let http_warnings = body["warnings"].as_array().expect("warnings");
    assert_eq!(http_warnings.len(), 1, "{body}");
    assert_eq!(http_warnings[0].as_str(), Some(cli_warning.as_str()));
}
