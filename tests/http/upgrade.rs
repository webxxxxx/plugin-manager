//! Upgrade http tests (#264).

use super::*;

use std::path::PathBuf;

fn user_command(sandbox: &Sandbox) -> std::process::Command {
    let mut command = std::process::Command::new(&sandbox.svcd);
    sandbox.env(&mut command);
    command.env("SVCD_TEST_PRIVILEGE", "unprivileged");
    command
}

fn daemon_starts(sandbox: &Sandbox) -> usize {
    fs::read_to_string(sandbox.logs.join("svcd.log"))
        .map(|text| text.matches("svcd starting").count())
        .unwrap_or(0)
}

fn current_address(sandbox: &Sandbox) -> Option<String> {
    let text = fs::read_to_string(sandbox.data.join("run").join("http.port")).ok()?;
    let address = text.trim().to_string();
    TcpStream::connect(&address).ok().map(|_| address)
}

fn exe_suffix() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

fn target() -> &'static str {
    if cfg!(windows) {
        "x86_64-pc-windows-msvc"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

/// Lay out an install tree the way `svcd --install` would, and point the
/// document at it. The daemon then runs from that tree, so a Windows
/// self-replace renames the running image.
fn install_tree(sandbox: &Sandbox) -> PathBuf {
    let install = sandbox.root.join("install");
    fs::create_dir_all(&install).expect("install dir");
    for name in ["svcd", "svc", "plugin", "mcp"] {
        let target = install.join(format!("{}{}", name, exe_suffix()));
        fs::copy(&sandbox.svcd, &target).expect("install binary");
    }
    let conf = sandbox.config.join("svcd.conf");
    let mut text = fs::read_to_string(&conf).unwrap_or_default();
    if !text.contains("install_dir") {
        text.push_str(&format!("install_dir = {}\n", install.display()));
    }
    fs::write(&conf, text).expect("svcd.conf");
    install
}

fn installed_in(install: &std::path::Path, name: &str) -> PathBuf {
    install.join(format!("{}{}", name, exe_suffix()))
}

/// Build a release-shaped archive that carries this test binary plus a marker,
/// so the installed file can be told apart from the original without a second
/// build (appending bytes keeps the PE/ELF runnable).
fn build_archive(sandbox: &Sandbox, marker: &str) -> PathBuf {
    let version = env!("CARGO_PKG_VERSION");
    let stage = sandbox.root.join("upgrade-stage");
    let top = format!("svcd-{}-{}", version, target());
    let root = stage.join(&top);
    fs::create_dir_all(&root).expect("stage dir");
    let mut bytes = fs::read(&sandbox.svcd).expect("read binary");
    bytes.extend_from_slice(b"\n");
    bytes.extend_from_slice(marker.as_bytes());
    for name in ["svcd", "svc", "plugin", "mcp"] {
        fs::write(root.join(format!("{}{}", name, exe_suffix())), &bytes).expect("stage binary");
    }
    let archive = sandbox.root.join("upgrade.tar.gz");
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .arg(&top)
        .status()
        .expect("run tar");
    assert!(status.success(), "tar could not build the archive");
    archive
}

#[test]
fn version_reports_the_build() {
    let sandbox = Sandbox::new();
    let mut command = std::process::Command::new(&sandbox.svcd);
    command.arg("version");
    let output = command.output().expect("run svcd version");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "version line: {text}"
    );
    assert!(text.contains("protocol 3"), "{text}");
    assert!(text.contains("svcd "), "{text}");
}

#[test]
fn a_check_without_the_store_says_so() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();

    let check = request(&address, "GET", "/api/v1/upgrade/check", "", b"");
    assert_eq!(check.status, 200, "{:?}", check.body);
    assert_eq!(check.body["store"], false, "{:?}", check.body);
    assert_eq!(check.body["current"], env!("CARGO_PKG_VERSION"));

    let mut command = user_command(&sandbox);
    command.args(["upgrade", "--check", "--json"]);
    let output = command.output().expect("run svcd upgrade --check");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["store"], false, "{value}");

    let apply = request(&address, "POST", "/api/v1/upgrade/apply", "", b"");
    assert_eq!(apply.status, 400, "{:?}", apply.body);
    let message = apply.body["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("plugin-store"), "{message}");
    assert!(message.contains("--file"), "{message}");
}

#[test]
fn upgrade_routes_enforce_auth_and_methods() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();

    assert_eq!(
        raw_request(&address, "POST", "/api/v1/upgrade/check", "", b"").status,
        405
    );
    assert_eq!(
        raw_request(&address, "GET", "/api/v1/upgrade/apply", "", b"").status,
        405
    );

    let (address, _password) = enable_auth(&mut sandbox);
    assert_eq!(
        raw_request(&address, "GET", "/api/v1/upgrade/check", "", b"").status,
        401
    );
    assert_eq!(
        raw_request(&address, "POST", "/api/v1/upgrade/apply", "", b"").status,
        401
    );
}

#[test]
fn upgrading_from_a_file_replaces_the_binaries_and_restarts() {
    let mut sandbox = Sandbox::new();
    let install = install_tree(&sandbox);
    sandbox.start_daemon_from(
        &installed_in(&install, "svcd"),
        &[("SVCD_TEST_PRIVILEGE", "unprivileged")],
    );
    sandbox.wait_for_http();
    let before = daemon_starts(&sandbox);
    assert_eq!(before, 1);
    let marker = "UPGRADE-MARKER-264";
    let archive = build_archive(&sandbox, marker);

    let mut command = user_command(&sandbox);
    command.arg("upgrade").arg("--file").arg(&archive);
    let output = support::run_bounded(&mut command, Duration::from_secs(60))
        .expect("svcd upgrade --file must finish");
    assert!(
        output.status.success(),
        "upgrade: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The installed binaries are the archive's bytes (the marker is the tail).
    for name in ["svcd", "svc", "plugin", "mcp"] {
        let path = installed_in(&install, name);
        let bytes = fs::read(&path).unwrap_or_else(|error| panic!("{}: {}", path.display(), error));
        assert!(
            bytes.ends_with(marker.as_bytes()),
            "{} was not replaced",
            path.display()
        );
    }

    // The helper stops the old daemon and starts the installed one; the new
    // process removes the `.old` leftovers at startup.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if daemon_starts(&sandbox) > before {
            if let Some(address) = current_address(&sandbox) {
                if request(&address, "GET", "/api/v1/version", "", b"").status == 200 {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        daemon_starts(&sandbox) > before,
        "the daemon did not restart"
    );
    assert!(
        !install.join(format!("svcd{}.old", exe_suffix())).exists(),
        "the .old image survived the restart"
    );
}

#[test]
fn a_service_mode_upgrade_installs_but_does_not_restart() {
    let mut sandbox = Sandbox::new();
    let install = install_tree(&sandbox);
    sandbox.start_daemon_from(
        &installed_in(&install, "svcd"),
        &[("SVCD_TEST_PRIVILEGE", "privileged")],
    );
    sandbox.wait_for_http();
    assert_eq!(daemon_starts(&sandbox), 1);
    let marker = "UPGRADE-MARKER-SERVICE";
    let archive = build_archive(&sandbox, marker);

    let mut command = std::process::Command::new(&sandbox.svcd);
    sandbox.env(&mut command);
    command.env("SVCD_TEST_PRIVILEGE", "privileged");
    command.arg("upgrade").arg("--file").arg(&archive);
    let output = support::run_bounded(&mut command, Duration::from_secs(60))
        .expect("svcd upgrade --file must finish");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("service manager"), "{text}");
    let bytes = fs::read(installed_in(&install, "svcd")).expect("installed binary");
    assert!(bytes.ends_with(marker.as_bytes()));
    assert_eq!(
        daemon_starts(&sandbox),
        1,
        "a service-mode upgrade must not restart itself"
    );
}
