use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod support;

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        Sandbox {
            root: support::sandbox_root("install-cli"),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_svcd"));
        command
            .env("SVCD_CONFIG_DIR", self.config_dir())
            .env("SVCD_DATA_DIR", self.data_dir())
            .env("SVCD_LOG_DIR", self.root.join("logs"))
            // Install tests pin the role instead of inheriting the runner's:
            // CI and the WSL runbook both exercise this suite as root.
            .env("SVCD_TEST_PRIVILEGE", "privileged")
            .env_remove("SVCD_TEST_UNPRIVILEGED")
            .env_remove("SVCD_USER")
            // #311: a system install edits the machine PATH. Point both forms
            // (profile directory, registry key) at the sandbox.
            .env("SVCD_TEST_SYSTEM_ROOT", self.root.join("system"))
            .env(
                "SVCD_TEST_SYSTEM_PATH_KEY",
                format!("HKCU:Software\\svcd-tests\\{}", self.key_name()),
            );
        command
    }

    /// One registry subkey per sandbox, so parallel tests never share a value.
    fn key_name(&self) -> String {
        self.root
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, |_| {})
    }

    fn run_with<F>(&self, args: &[&str], configure: F) -> Output
    where
        F: FnOnce(&mut Command),
    {
        let mut command = self.command();
        command.args(args);
        configure(&mut command);
        command.output().expect("run svcd")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn systemd_dry_run_prints_the_plan_without_touching_the_host() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["install-service", "--dry-run", "--target", "systemd"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    for needle in [
        "/etc/systemd/system/svcd.service",
        "Type=notify",
        "Restart=on-failure",
        "TimeoutStopSec=40",
        "systemctl daemon-reload",
        "systemctl enable --now svcd",
    ] {
        assert!(text.contains(needle), "missing {:?} in:\n{}", needle, text);
    }
    assert!(
        !sandbox.config_dir().exists() && !sandbox.data_dir().exists(),
        "dry run created state: {:?}",
        sandbox.root
    );
}

#[test]
fn every_target_dry_run_renders_its_plan() {
    let sandbox = Sandbox::new();
    let cases: [(&str, &[&str]); 5] = [
        ("systemd", &["Type=notify", "svcd.service"]),
        (
            "launchd",
            &[
                "com.webxxxxx.svcd",
                "launchctl bootstrap",
                "ThrottleInterval",
            ],
        ),
        ("scm", &["sc create", "sc failure", "restart/5000"]),
        (
            "busybox",
            &["::respawn:", "run --foreground", "kill -HUP 1"],
        ),
        (
            "procd",
            &["USE_PROCD=1", "respawn 3600 5 5", "term_timeout 30"],
        ),
    ];

    for (target, needles) in cases {
        let output = sandbox.run(&["install-service", "--dry-run", "--target", target]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{} stderr: {}",
            target,
            stderr(&output)
        );
        let text = stdout(&output);
        assert!(
            text.contains(&format!("install-service plan for {}", target)),
            "{} header missing in:\n{}",
            target,
            text
        );
        for needle in needles {
            assert!(
                text.contains(needle),
                "{} missing {:?} in:\n{}",
                target,
                needle,
                text
            );
        }
    }
}

#[test]
fn scm_plan_tolerates_an_existing_group_and_verifies_it_before_starting() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["install-service", "--dry-run", "--target", "scm"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    // `net.exe` reports every failure as exit 2; the Win32 code (1379,
    // ERROR_ALIAS_EXISTS) only appears in the message. A reinstall must not
    // fail just because the group survived the previous install, so the plan
    // tolerates exit 2 and then proves the group exists.
    assert!(
        text.contains("run net localgroup svcd /add (allow exit [0, 2])"),
        "the group add must tolerate net.exe's generic failure exit:\n{}",
        text
    );
    assert!(
        text.contains("\n  run net localgroup svcd\n"),
        "the plan must verify the group after adding it:\n{}",
        text
    );
    let add = text.find("net localgroup svcd /add").expect("group add");
    let verify = text
        .find("\n  run net localgroup svcd\n")
        .expect("group verify");
    let create = text.find("sc create").expect("service create");
    assert!(
        add < verify && verify < create,
        "the group must be proven before the service is created:\n{}",
        text
    );
}

#[test]
fn target_can_be_given_with_an_equals_sign() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["install-service", "--target=procd", "--dry-run"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("install-service plan for procd"));
}

#[test]
fn uninstall_keeps_data_by_default_and_purges_it_on_request() {
    let sandbox = Sandbox::new();

    let keep = sandbox.run(&["uninstall-service", "--dry-run", "--target", "systemd"]);
    assert_eq!(keep.status.code(), Some(0), "stderr: {}", stderr(&keep));
    assert!(!stdout(&keep).contains("remove directory"));

    let purge = sandbox.run(&[
        "uninstall-service",
        "--dry-run",
        "--target",
        "systemd",
        "--purge",
    ]);
    assert_eq!(purge.status.code(), Some(0), "stderr: {}", stderr(&purge));
    let text = stdout(&purge);
    assert!(
        text.contains(&format!(
            "remove directory {}",
            sandbox.data_dir().display()
        )),
        "data dir missing from:\n{}",
        text
    );
    assert!(
        text.contains(&format!(
            "remove directory {}",
            sandbox.config_dir().display()
        )),
        "config dir missing from:\n{}",
        text
    );
}

#[test]
fn usage_errors_exit_sixty_four() {
    let sandbox = Sandbox::new();
    let cases: &[&[&str]] = &[
        &["install-service", "--purge"],
        &["install-service", "--target"],
        &["install-service", "--target", "nope"],
        &["install-service", "--target="],
        &["install-service", "--target", "systemd", "extra"],
        &["install-service", "--dry-run", "--dry-run"],
        &[
            "install-service",
            "--target",
            "systemd",
            "--target",
            "procd",
        ],
        &["uninstall-service", "extra"],
        &["uninstall-service", "--purge", "--purge"],
        &["--uninstall", "--conf", "svcd.conf"],
        &["--uninstall", "--manual"],
        &["--install", "--conf"],
        &["--install", "--conf", "a", "--conf", "b"],
    ];

    for args in cases {
        let output = sandbox.run(args);
        assert_eq!(
            output.status.code(),
            Some(64),
            "{:?} stderr: {}",
            args,
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("usage:"),
            "{:?} stderr: {}",
            args,
            stderr(&output)
        );
    }
}

#[test]
fn no_arguments_prints_the_svcd_usage() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&[]);

    assert_eq!(
        output.status.code(),
        Some(64),
        "stderr: {}",
        stderr(&output)
    );
    let errors = stderr(&output);
    assert!(errors.contains("svcd run --foreground"), "{}", errors);
    assert!(errors.contains("install-service"), "{}", errors);
    assert!(errors.contains("uninstall-service"), "{}", errors);
}

#[test]
fn default_detection_produces_a_plan_without_writing_anything() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["install-service", "--dry-run"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("install-service plan for"));
    assert!(!sandbox.config_dir().exists());
    assert!(!sandbox.data_dir().exists());
}

#[cfg(windows)]
#[test]
fn windows_detection_defaults_to_scm() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["install-service", "--dry-run"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("install-service plan for scm"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_detection_defaults_to_launchd() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["install-service", "--dry-run"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("install-service plan for launchd"));
}

#[cfg(unix)]
fn unix_user_env(command: &mut Command, home: &Path, config: &Path, data: &Path, state: &Path) {
    command
        .env_remove("SVCD_CONFIG_DIR")
        .env_remove("SVCD_DATA_DIR")
        .env_remove("SVCD_LOG_DIR")
        .env_remove("SVCD_USER")
        // Auto-detection: a non-root process picks user mode without SVCD_USER.
        .env("SVCD_TEST_PRIVILEGE", "unprivileged")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_DATA_HOME", data)
        .env("XDG_STATE_HOME", state);
}

#[cfg(windows)]
fn windows_user_env(command: &mut Command, appdata: &Path, localappdata: &Path) {
    command
        .env_remove("SVCD_CONFIG_DIR")
        .env_remove("SVCD_DATA_DIR")
        .env_remove("SVCD_LOG_DIR")
        .env_remove("SVCD_USER")
        // Auto-detection: a non-elevated process picks user mode without SVCD_USER.
        .env("SVCD_TEST_PRIVILEGE", "unprivileged")
        .env("APPDATA", appdata)
        .env("LOCALAPPDATA", localappdata)
        // Never touch the developer's HKCU PATH from a test.
        .env("SVCD_TEST_NO_USER_PATH", "1");
}

#[cfg(windows)]
#[test]
fn the_service_account_and_an_elevated_admin_share_one_config_directory() {
    let sandbox = Sandbox::new();
    let program_data = sandbox.root.join("ProgramData");
    let system_profile = sandbox.root.join("systemprofile");
    let admin_profile = sandbox.root.join("adminprofile");
    let expected = program_data.join("svcd").join("config");

    let run_as = |profile: &Path| {
        sandbox.run_with(&["--install", "--dry-run"], |command| {
            command
                .env_remove("SVCD_CONFIG_DIR")
                .env_remove("SVCD_DATA_DIR")
                .env_remove("SVCD_LOG_DIR")
                .env("SVCD_USER", "0")
                .env("SVCD_TEST_PRIVILEGE", "privileged")
                .env("ProgramData", &program_data)
                .env("USERPROFILE", profile);
        })
    };

    for (role, output) in [
        ("LocalSystem", run_as(&system_profile)),
        ("Administrator", run_as(&admin_profile)),
    ] {
        assert_eq!(
            output.status.code(),
            Some(0),
            "{} stderr: {}",
            role,
            stderr(&output)
        );
        assert!(
            stdout(&output).contains(&format!("config:  {}", expected.display())),
            "{} must use the machine-wide config:\n{}",
            role,
            stdout(&output)
        );
    }
}

#[cfg(windows)]
#[test]
fn unreachable_user_cli_points_at_the_machine_wide_service() {
    let sandbox = Sandbox::new();
    let program_data = sandbox.root.join("ProgramData");
    let machine_config = program_data.join("svcd").join("config");
    fs::create_dir_all(&machine_config).expect("machine config dir");
    fs::write(machine_config.join("svcd.conf"), "bind = 127.0.0.1:0\n").expect("machine conf");
    let appdata = sandbox.root.join("appdata");
    fs::create_dir_all(&appdata).expect("appdata");
    let svc = sandbox.root.join("svc.exe");
    fs::copy(env!("CARGO_BIN_EXE_svcd"), &svc).expect("copy svc alias");

    let output = Command::new(&svc)
        .arg("list")
        .env_remove("SVCD_CONFIG_DIR")
        .env_remove("SVCD_DATA_DIR")
        .env_remove("SVCD_LOG_DIR")
        .env_remove("SVCD_USER")
        .env("SVCD_TEST_PRIVILEGE", "unprivileged")
        .env("ProgramData", &program_data)
        .env("APPDATA", &appdata)
        .output()
        .expect("run svc list");

    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    let errors = stderr(&output);
    assert!(errors.contains("svcd unreachable"), "{}", errors);
    assert!(
        errors.contains("elevated terminal"),
        "a non-elevated CLI must explain the machine-wide service:\n{}",
        errors
    );
    assert!(
        errors.contains("\"svcd\" group"),
        "read-only access through the group must be mentioned:\n{}",
        errors
    );
}

#[test]
fn normal_user_detection_selects_user_mode() {
    let sandbox = Sandbox::new();

    let output = sandbox.run_with(&["--install", "--dry-run"], |command| {
        command
            .env_remove("SVCD_USER")
            .env("SVCD_TEST_PRIVILEGE", "unprivileged");
    });

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("user mode install (no system service)"),
        "a normal user must not register a system service:\n{}",
        text
    );
    assert!(
        !text.contains("service: program"),
        "a normal user must not get a system service plan:\n{}",
        text
    );
}

#[test]
fn root_detection_selects_system_mode() {
    let sandbox = Sandbox::new();

    let output = sandbox.run_with(&["--install", "--dry-run"], |command| {
        command
            .env_remove("SVCD_USER")
            .env("SVCD_TEST_PRIVILEGE", "privileged");
    });

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("service: program"),
        "root installs the system service:\n{}",
        text
    );
    assert!(
        !text.contains("user mode install"),
        "root must not fall back to user mode:\n{}",
        text
    );
}

#[test]
fn svcd_user_overrides_the_detected_role_both_ways() {
    let sandbox = Sandbox::new();

    let forced_user = sandbox.run_with(&["--install", "--dry-run"], |command| {
        command
            .env("SVCD_USER", "1")
            .env("SVCD_TEST_PRIVILEGE", "privileged");
    });
    assert_eq!(
        forced_user.status.code(),
        Some(0),
        "{}",
        stderr(&forced_user)
    );
    assert!(
        stdout(&forced_user).contains("user mode install (no system service)"),
        "SVCD_USER=1 must force user mode for an Administrator:\n{}",
        stdout(&forced_user)
    );

    let forced_system = sandbox.run_with(&["--install", "--dry-run"], |command| {
        command
            .env("SVCD_USER", "0")
            .env("SVCD_TEST_PRIVILEGE", "unprivileged");
    });
    assert_eq!(
        forced_system.status.code(),
        Some(0),
        "{}",
        stderr(&forced_system)
    );
    assert!(
        stdout(&forced_system).contains("service: program"),
        "SVCD_USER=0 must plan the system install:\n{}",
        stdout(&forced_system)
    );
}

#[test]
fn system_install_is_refused_for_a_normal_user() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.root.join("system-prefix");

    let output = sandbox.run_with(
        &[
            "--install",
            "--prefix",
            prefix.to_str().expect("utf8"),
            "--no-service",
        ],
        |command| {
            command
                .env("SVCD_USER", "0")
                .env("SVCD_TEST_PRIVILEGE", "unprivileged");
        },
    );

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("requires root / Administrator"),
        "{}",
        stderr(&output)
    );
    assert!(
        !prefix.exists(),
        "the refused install must not create {}",
        prefix.display()
    );
    assert!(
        !sandbox.config_dir().exists(),
        "the refused install must not write config"
    );
}

#[test]
fn system_uninstall_is_refused_for_a_normal_user() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.root.join("system-prefix");
    fs::create_dir_all(&prefix).expect("create prefix");
    let binary = prefix.join(executable_name());
    fs::write(&binary, "installed").expect("seed installed binary");

    let output = sandbox.run_with(
        &[
            "--uninstall",
            "--prefix",
            prefix.to_str().expect("utf8"),
            "--no-service",
        ],
        |command| {
            command
                .env("SVCD_USER", "0")
                .env("SVCD_TEST_PRIVILEGE", "unprivileged");
        },
    );

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("--uninstall requires root / Administrator"),
        "{}",
        stderr(&output)
    );
    assert!(
        binary.is_file(),
        "the refused uninstall must leave the binary in place"
    );
}

#[test]
fn service_actions_are_refused_for_a_normal_user() {
    let sandbox = Sandbox::new();
    let target = if cfg!(windows) { "scm" } else { "systemd" };

    for action in ["install-service", "uninstall-service"] {
        let output = sandbox.run_with(&[action, "--target", target], |command| {
            command
                .env("SVCD_USER", "0")
                .env("SVCD_TEST_PRIVILEGE", "unprivileged");
        });
        assert_eq!(
            output.status.code(),
            Some(1),
            "{} stderr: {}",
            action,
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(&format!("{} requires root / Administrator", action)),
            "{} stderr: {}",
            action,
            stderr(&output)
        );
    }
}

#[test]
fn user_dry_run_reports_the_profile_paths_without_writing() {
    let sandbox = Sandbox::new();

    #[cfg(unix)]
    let config = sandbox.root.join("xdg-config");
    #[cfg(unix)]
    let data = sandbox.root.join("xdg-data");
    #[cfg(unix)]
    let state = sandbox.root.join("xdg-state");
    #[cfg(unix)]
    let home = sandbox.root.join("home");
    #[cfg(unix)]
    fs::create_dir_all(&home).expect("create home");

    #[cfg(windows)]
    let appdata = sandbox.root.join("appdata");
    #[cfg(windows)]
    let localappdata = sandbox.root.join("localappdata");

    let output = sandbox.run_with(&["--install", "--dry-run"], |command| {
        #[cfg(unix)]
        unix_user_env(command, &home, &config, &data, &state);
        #[cfg(windows)]
        windows_user_env(command, &appdata, &localappdata);
    });

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("user mode install (no system service)"),
        "{}",
        text
    );
    assert!(
        text.contains("service: skipped (user mode; use svcd user start or systemd --user)"),
        "{}",
        text
    );
    #[cfg(unix)]
    {
        assert!(
            text.contains(&format!(
                "path:    add {} to shell profile",
                data.join("svcd/bin").display()
            )),
            "{}",
            text
        );
        assert!(
            !data.join("svcd").exists() && !config.join("svcd").exists(),
            "dry run created user state"
        );
    }
    #[cfg(windows)]
    {
        assert!(
            text.contains(&format!(
                "path:    add {} to HKCU",
                localappdata.join("svcd").join("bin").display()
            )),
            "{}",
            text
        );
        assert!(
            !localappdata.join("svcd").exists() && !appdata.join("svcd").exists(),
            "dry run created user state"
        );
    }
}

#[cfg(unix)]
#[test]
fn unix_user_install_stays_in_the_profile_and_uninstall_removes_it() {
    let sandbox = Sandbox::new();
    let home = sandbox.root.join("home");
    let config = sandbox.root.join("xdg-config");
    let data = sandbox.root.join("xdg-data");
    let state = sandbox.root.join("xdg-state");
    fs::create_dir_all(&home).expect("create home");
    let profile = home.join(".profile");
    fs::write(&profile, "# user profile\n").expect("seed profile");
    let install_root = data.join("svcd").join("bin");

    let installed = sandbox.run_with(&["--install", "--no-service"], |command| {
        unix_user_env(command, &home, &config, &data, &state);
    });
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));
    let printed = stdout(&installed);
    assert!(
        printed.contains(&format!(
            "svcd installed (user mode): {}",
            install_root.join("svcd").display()
        )),
        "{}",
        printed
    );
    assert!(
        printed.contains("service: not installed (--no-service)"),
        "user install must not register a service:\n{}",
        printed
    );
    assert!(install_root.join("svcd").is_file());
    for alias in ["svc", "plugin", "mcp"] {
        assert!(
            install_root.join(alias).is_file(),
            "alias {} missing",
            alias
        );
    }
    assert!(
        config.join("svcd").join("svcd.conf").is_file(),
        "user config missing"
    );
    let profile_text = fs::read_to_string(&profile).expect("read profile");
    assert!(
        profile_text.contains("# svcd user path")
            && profile_text.contains(&install_root.display().to_string()),
        "profile must export the user bin directory:\n{}",
        profile_text
    );
    assert!(
        !sandbox.data_dir().exists() && !sandbox.config_dir().exists(),
        "user install must ignore the system directories"
    );

    let removed = sandbox.run_with(&["--uninstall", "--no-service", "--purge"], |command| {
        unix_user_env(command, &home, &config, &data, &state);
    });
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(!install_root.join("svcd").exists(), "binary left behind");
    for alias in ["svc", "plugin", "mcp"] {
        assert!(
            !install_root.join(alias).exists(),
            "alias {} left behind",
            alias
        );
    }
    let profile_text = fs::read_to_string(&profile).expect("read profile");
    assert!(
        !profile_text.contains("# svcd user path"),
        "uninstall must remove the PATH block:\n{}",
        profile_text
    );
    assert!(
        !config.join("svcd").exists() && !data.join("svcd").exists(),
        "purge left user config/data behind"
    );
}

#[cfg(windows)]
#[test]
fn windows_user_install_stays_in_localappdata_and_uninstall_removes_it() {
    let sandbox = Sandbox::new();
    let appdata = sandbox.root.join("appdata");
    let localappdata = sandbox.root.join("localappdata");
    let install_root = localappdata.join("svcd").join("bin");

    let installed = sandbox.run_with(&["--install", "--no-service"], |command| {
        windows_user_env(command, &appdata, &localappdata);
    });
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));
    let printed = stdout(&installed);
    assert!(
        printed.contains(&format!(
            "svcd installed (user mode): {}",
            install_root.join("svcd.exe").display()
        )),
        "{}",
        printed
    );
    assert!(
        printed.contains("service: not installed (--no-service)"),
        "user install must not register a service:\n{}",
        printed
    );
    assert!(
        printed.contains("path: skipped (test hook)"),
        "the test must not edit the host PATH:\n{}",
        printed
    );
    for name in [
        "svcd.exe",
        "svc.exe",
        "plugin.exe",
        "mcp.exe",
        "uninstall.exe",
    ] {
        assert!(
            install_root.join(name).is_file(),
            "{} missing from {}",
            name,
            install_root.display()
        );
    }
    assert!(
        appdata.join("svcd").join("svcd.conf").is_file(),
        "user config missing under %APPDATA%"
    );
    assert!(
        !sandbox.data_dir().exists() && !sandbox.config_dir().exists(),
        "user install must ignore the system directories"
    );

    let removed = sandbox.run_with(&["--uninstall", "--no-service"], |command| {
        windows_user_env(command, &appdata, &localappdata);
    });
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    for name in [
        "svcd.exe",
        "svc.exe",
        "plugin.exe",
        "mcp.exe",
        "uninstall.exe",
    ] {
        assert!(
            !install_root.join(name).exists(),
            "{} left behind by uninstall",
            name
        );
    }
}

/// Where a POSIX host keeps the profile entry the installer writes.
#[cfg(unix)]
fn system_path_file(sandbox: &Sandbox) -> PathBuf {
    if cfg!(target_os = "macos") {
        sandbox.root.join("system").join("paths.d").join("svcd")
    } else {
        sandbox
            .root
            .join("system")
            .join("profile.d")
            .join("svcd.sh")
    }
}

/// `--manual` starts from an init script; the launcher carries its own PATH, so
/// the machine environment stays untouched (#311).
#[test]
fn a_manual_install_leaves_the_machine_path_alone() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.root.join("manual-prefix");
    let output = sandbox.run(&[
        "--install",
        "--manual",
        "--prefix",
        prefix.to_str().expect("utf8"),
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        !stdout(&output).contains("system PATH"),
        "{}",
        stdout(&output)
    );
    #[cfg(unix)]
    assert!(!system_path_file(&sandbox).exists(), "no profile entry");
}

#[test]
fn self_install_copies_aliases_and_uninstalls_without_touching_the_host() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.root.join("prefix");
    let prefix_arg = prefix.to_str().expect("utf8");

    let dry = sandbox.run(&[
        "--install",
        "--prefix",
        prefix_arg,
        "--no-service",
        "--dry-run",
    ]);
    assert_eq!(dry.status.code(), Some(0), "{}", stderr(&dry));
    assert!(stdout(&dry).contains("aliases:"));
    assert!(!prefix.exists());

    let service_dry = sandbox.run(&["--install", "--prefix", prefix_arg, "--dry-run"]);
    assert_eq!(
        service_dry.status.code(),
        Some(0),
        "{}",
        stderr(&service_dry)
    );
    assert!(stdout(&service_dry).contains("service: program"));
    // #311: every mode except `--manual` puts the prefix on the machine PATH;
    // the dry run says so before touching anything.
    assert!(
        stdout(&service_dry).contains("to the system PATH"),
        "{}",
        stdout(&service_dry)
    );
    assert!(!prefix.exists());

    let installed = sandbox.run(&["--install", "--prefix", prefix_arg, "--no-service"]);
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));
    // The development sandbox denies registry writes from every process it
    // starts, so on Windows the installer may only be able to report the
    // failure; the stored value is asserted where the host lets it write.
    let install_out = stdout(&installed);
    let install_err = stderr(&installed);
    assert!(
        install_out.contains("path: added")
            || install_out.contains("path: updated")
            || install_err.contains("could not update the system PATH"),
        "{install_out}\nstderr: {install_err}"
    );
    #[cfg(unix)]
    {
        let file = system_path_file(&sandbox);
        let text = fs::read_to_string(&file).unwrap_or_else(|error| {
            panic!("{error}: {}", file.display());
        });
        assert!(text.contains(prefix_arg), "{text}");
    }
    let exe = if cfg!(windows) { "svcd.exe" } else { "svcd" };
    assert!(prefix.join(exe).is_file());
    for alias in ["svc", "plugin", "mcp"] {
        let name = if cfg!(windows) {
            format!("{}.exe", alias)
        } else {
            alias.to_string()
        };
        assert!(prefix.join(name).is_file(), "missing alias {}", alias);
    }

    let svc = prefix.join(if cfg!(windows) { "svc.exe" } else { "svc" });
    // `svc list` needs a reachable daemon now; `compat` still proves the
    // installed alias runs without one.
    let ran = Command::new(&svc)
        .arg("compat")
        .env("SVCD_CONFIG_DIR", sandbox.config_dir())
        .env("SVCD_DATA_DIR", sandbox.data_dir())
        .env("SVCD_LOG_DIR", sandbox.root.join("logs"))
        .output()
        .expect("run installed svc");
    assert_eq!(ran.status.code(), Some(0), "{}", stderr(&ran));
    assert!(
        stdout(&ran).contains("platform:"),
        "compat:\n{}",
        stdout(&ran)
    );

    let removed = sandbox.run(&["--uninstall", "--prefix", prefix_arg, "--no-service"]);
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(!prefix.join(exe).exists());
    #[cfg(unix)]
    {
        assert!(
            !system_path_file(&sandbox).exists(),
            "uninstall must take the entry back out"
        );
        assert!(
            stdout(&removed).contains("path: removed from"),
            "{}",
            stdout(&removed)
        );
    }
    assert!(
        sandbox.config_dir().exists(),
        "default uninstall keeps config"
    );

    let purged = sandbox.run(&[
        "--uninstall",
        "--prefix",
        prefix_arg,
        "--no-service",
        "--purge",
    ]);
    assert_eq!(purged.status.code(), Some(0), "{}", stderr(&purged));
    assert!(!sandbox.config_dir().exists());
    assert!(!sandbox.data_dir().exists());
}

#[test]
fn self_install_accepts_bind_in_conf() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.root.join("prefix");
    let prefix_arg = prefix.to_str().expect("utf8");
    let conf = sandbox.root.join("svcd.conf");
    let text = format!(
        "# runtime settings\nbind = 127.0.0.1:0\nplugins_dir = {}\nservices_dir = {}\n",
        sandbox.root.join("plugins").display(),
        sandbox.root.join("services").display()
    );
    fs::write(&conf, &text).expect("write source conf");
    let conf_arg = conf.to_str().expect("utf8");

    let dry = sandbox.run(&[
        "--install",
        "--prefix",
        prefix_arg,
        "--no-service",
        "--conf",
        conf_arg,
        "--dry-run",
    ]);
    assert_eq!(dry.status.code(), Some(0), "{}", stderr(&dry));
    assert!(stdout(&dry).contains("conf:"));
    assert!(!sandbox.config_dir().exists());

    let installed = sandbox.run(&[
        "--install",
        "--prefix",
        prefix_arg,
        "--no-service",
        "--conf",
        conf_arg,
    ]);
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));
    assert!(!sandbox.config_dir().join("lan.json").exists());
    assert!(!sandbox.config_dir().join("lan-token").exists());
    let copied = sandbox.config_dir().join("svcd.conf");
    let installed_text = fs::read_to_string(&copied).expect("copied conf");
    assert!(
        installed_text.starts_with(&text),
        "the source document and its comments must survive:\n{}",
        installed_text
    );
    assert!(
        installed_text.contains(&format!("install_dir = {}\n", prefix.display())),
        "the CLI prefix is recorded in the installed conf:\n{}",
        installed_text
    );
    assert!(
        installed_text.contains(&format!("data_dir = {}\n", prefix.join("data").display())),
        "the missing data root is materialized:\n{}",
        installed_text
    );
    assert!(
        installed_text.contains(&format!("logs_dir = {}\n", prefix.join("logs").display())),
        "the missing logs root is materialized:\n{}",
        installed_text
    );
}

#[test]
fn invalid_svcd_conf_is_rejected_without_overwriting_the_installed_one() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.root.join("prefix");
    let prefix_arg = prefix.to_str().expect("utf8");
    let conf = sandbox.root.join("svcd.conf");
    let conf_arg = conf.to_str().expect("utf8");
    let good = "bind = 127.0.0.1:0\n";
    fs::write(&conf, good).expect("write good conf");

    let installed = sandbox.run(&[
        "--install",
        "--prefix",
        prefix_arg,
        "--no-service",
        "--conf",
        conf_arg,
    ]);
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));
    let installed_text = fs::read_to_string(sandbox.config_dir().join("svcd.conf"))
        .expect("installed conf after the good install");
    assert!(
        installed_text.starts_with(good),
        "installed conf:\n{}",
        installed_text
    );

    fs::write(&conf, "unknown_key = 1\n").expect("write bad conf");
    let bad = sandbox.run(&[
        "--install",
        "--prefix",
        prefix_arg,
        "--no-service",
        "--conf",
        conf_arg,
    ]);
    assert_eq!(bad.status.code(), Some(1), "{}", stderr(&bad));
    assert!(stderr(&bad).contains("unknown key"), "{}", stderr(&bad));
    assert_eq!(
        fs::read_to_string(sandbox.config_dir().join("svcd.conf")).expect("installed conf"),
        installed_text,
        "a rejected config must not replace the installed one"
    );
}

/// An older svcd wrote `http_port`; this build rejects it (ADR-0010), so the
/// daemon exits 1 and systemd gives up after five retries. The installer must
/// name that document instead of upgrading around it and reporting only that
/// service registration failed.
#[test]
fn install_refuses_an_installed_conf_this_build_cannot_load() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.root.join("prefix");
    let prefix_arg = prefix.to_str().expect("utf8");

    let first = sandbox.run(&["--install", "--prefix", prefix_arg, "--no-service"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let conf = sandbox.config_dir().join("svcd.conf");
    let good = fs::read_to_string(&conf).expect("generated conf");
    assert!(good.contains("bind = "), "{good}");

    let stale = format!(
        "# written by a pre-ADR-0010 svcd\nhttp_port = 8787\nlan_default = false\nconfig_dir = {}\n",
        sandbox.config_dir().display()
    );
    fs::write(&conf, &stale).expect("write stale conf");

    let refused = sandbox.run(&["--install", "--prefix", prefix_arg, "--no-service"]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&refused),
        stderr(&refused)
    );
    let message = stderr(&refused);
    assert!(
        message.contains("http_port"),
        "the message names the key: {message}"
    );
    assert!(
        message.contains("bind"),
        "the message names the replacement: {message}"
    );
    assert_eq!(
        fs::read_to_string(&conf).expect("conf after the refusal"),
        stale,
        "a refused install must leave the document alone"
    );
}

#[test]
fn install_service_plan_passes_conf() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&[
        "install-service",
        "--dry-run",
        "--target",
        "systemd",
        "--conf",
        "/tmp/custom-svcd.conf",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("--conf \"/tmp/custom-svcd.conf\""),
        "{}",
        stdout(&output)
    );
}

#[test]
fn install_service_accepts_a_program_override() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&[
        "install-service",
        "--dry-run",
        "--target",
        "systemd",
        "--program",
        "/opt/svcd",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("/opt/svcd run --foreground"));
}

fn executable_name() -> &'static str {
    if cfg!(windows) {
        "svcd.exe"
    } else {
        "svcd"
    }
}

fn alias_file_names() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["svc.exe", "plugin.exe", "mcp.exe"]
    } else {
        vec!["svc", "plugin", "mcp"]
    }
}

fn launcher_name() -> &'static str {
    if cfg!(windows) {
        "svcd_daemon.bat"
    } else {
        "svcd_daemon.sh"
    }
}

fn install_dir_conf(sandbox: &Sandbox, install_dir: &std::path::Path) -> PathBuf {
    let conf = sandbox.root.join("svcd.conf");
    let text = format!(
        "install_dir = {}\nbind = 127.0.0.1:0\n",
        install_dir.display()
    );
    fs::write(&conf, text).expect("write source conf");
    conf
}

fn installed_conf(sandbox: &Sandbox) -> String {
    fs::read_to_string(sandbox.config_dir().join("svcd.conf")).expect("read installed conf")
}

#[test]
fn prefixed_install_places_all_three_roots_under_the_install_dir() {
    let sandbox = Sandbox::new();
    let install_root = sandbox.root.join("persist").join("svcd");

    let output = sandbox.run_with(
        &[
            "--install",
            "--prefix",
            install_root.to_str().expect("utf8"),
            "--no-service",
        ],
        |command| {
            // A prefix install derives all three roots when no document is
            // installed. A host that really runs svcd has one (`/etc/svcd` or
            // `%ProgramData%\svcd\config`), and its explicit roots win over
            // `--prefix` by design (ADR-0023 Amendment): point the machine-wide
            // document at the sandbox so this case is the same on every host.
            command
                .env(
                    "SVCD_TEST_SYSTEM_CONFIG_DIR",
                    sandbox.root.join("system-config"),
                )
                .env_remove("SVCD_CONFIG_DIR")
                .env_remove("SVCD_DATA_DIR")
                .env_remove("SVCD_LOG_DIR");
        },
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let config_dir = install_root.join("config");
    let data_dir = install_root.join("data");
    let logs_dir = install_root.join("logs");
    for dir in [&config_dir, &data_dir, &logs_dir] {
        assert!(dir.is_dir(), "{} was not created", dir.display());
    }
    let conf_path = config_dir.join("svcd.conf");
    let conf = fs::read_to_string(&conf_path).expect("installed conf");
    for (key, value) in [
        ("config_dir", &config_dir),
        ("data_dir", &data_dir),
        ("logs_dir", &logs_dir),
        ("install_dir", &install_root),
    ] {
        assert!(
            conf.contains(&format!("{} = {}\n", key, value.display())),
            "{} missing from:\n{}",
            key,
            conf
        );
    }

    // Uninstall must find the config and the recorded prefix the same way.
    let dry_uninstall = Command::new(install_root.join(executable_name()))
        .args(["--uninstall", "--no-service", "--purge", "--dry-run"])
        .env_remove("SVCD_CONFIG_DIR")
        .env_remove("SVCD_DATA_DIR")
        .env_remove("SVCD_LOG_DIR")
        .env_remove("SVCD_USER")
        .env("SVCD_TEST_PRIVILEGE", "privileged")
        .output()
        .expect("run installed svcd --uninstall --dry-run");
    assert_eq!(
        dry_uninstall.status.code(),
        Some(0),
        "{}",
        stderr(&dry_uninstall)
    );
    let planned = stdout(&dry_uninstall);
    assert!(
        planned.contains(&format!("uninstall {}", install_root.display())),
        "{}",
        planned
    );
    assert!(
        planned.contains(&format!(
            "purge: {}, {}, {}",
            config_dir.display(),
            data_dir.display(),
            logs_dir.display()
        )),
        "{}",
        planned
    );

    // The installed alias must find the config next to the program without
    // SVCD_CONFIG_DIR, and resolve logs to the prefixed root.
    let log = logs_dir.join("svcd.log");
    fs::write(&log, "hello-from-prefix\n").expect("seed log");
    let svc = install_root.join(alias_file_names()[0]);
    let listed = Command::new(&svc)
        .args(["logs", "svcd"])
        .env_remove("SVCD_CONFIG_DIR")
        .env_remove("SVCD_DATA_DIR")
        .env_remove("SVCD_LOG_DIR")
        .env_remove("SVCD_USER")
        .env("SVCD_TEST_PRIVILEGE", "privileged")
        .output()
        .expect("run installed svc");
    assert_eq!(listed.status.code(), Some(0), "{}", stderr(&listed));
    assert_eq!(stdout(&listed), "hello-from-prefix\n");
}

/// ADR-0023 Amendment: explicit roots in a document win over `--prefix`, so a
/// prefixed install on a host that already has a document puts its binaries
/// under the prefix but keeps the recorded roots. The machine-wide document is
/// the sandbox's `SVCD_TEST_SYSTEM_CONFIG_DIR`, which is also the only way to
/// exercise this without root or a host that was really installed.
#[test]
fn a_recorded_document_keeps_its_explicit_roots_over_a_prefix() {
    let sandbox = Sandbox::new();
    let machine_config = sandbox.root.join("system-config");
    fs::create_dir_all(&machine_config).expect("system config dir");
    let recorded = [
        sandbox.root.join("recorded-config"),
        sandbox.root.join("recorded-data"),
        sandbox.root.join("recorded-logs"),
    ];
    let machine_conf = machine_config.join("svcd.conf");
    let text = format!(
        "config_dir = {}\ndata_dir = {}\nlogs_dir = {}\n",
        recorded[0].display(),
        recorded[1].display(),
        recorded[2].display()
    );
    fs::write(&machine_conf, &text).expect("machine-wide document");

    let install_root = sandbox.root.join("persist").join("svcd");
    let output = sandbox.run_with(
        &[
            "--install",
            "--prefix",
            install_root.to_str().expect("utf8"),
            "--no-service",
        ],
        |command| {
            command
                .env("SVCD_TEST_SYSTEM_CONFIG_DIR", &machine_config)
                .env_remove("SVCD_CONFIG_DIR")
                .env_remove("SVCD_DATA_DIR")
                .env_remove("SVCD_LOG_DIR");
        },
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(install_root.join(executable_name()).is_file());
    assert!(
        !install_root.join("config").exists(),
        "the recorded config_dir wins, so the prefix owns no config"
    );
    assert_eq!(
        fs::read_to_string(&machine_conf).expect("machine-wide document"),
        text,
        "the discovered document must keep its roots"
    );
    let used = fs::read_to_string(recorded[0].join("svcd.conf")).expect("recorded document");
    assert!(
        used.contains(&format!("install_dir = {}", install_root.display())),
        "the prefix is recorded in the document the install actually uses:\n{used}"
    );
    for dir in [&recorded[1], &recorded[2]] {
        assert!(dir.is_dir(), "{} was not created", dir.display());
    }
}

#[test]
fn prefix_is_recorded_as_install_dir_and_owns_data_and_logs() {
    let sandbox = Sandbox::new();
    let install_root = sandbox.root.join("persist").join("svcd");

    let output = sandbox.run(&[
        "--install",
        "--prefix",
        install_root.to_str().expect("utf8"),
        "--no-service",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(install_root.join(executable_name()).is_file());
    for alias in alias_file_names() {
        assert!(
            install_root.join(alias).is_file(),
            "alias {} missing",
            alias
        );
    }
    assert!(
        !install_root.join(launcher_name()).exists(),
        "only --manual writes a launcher"
    );
    let conf = installed_conf(&sandbox);
    assert!(
        conf.contains(&format!("install_dir = {}", install_root.display())),
        "conf:\n{}",
        conf
    );
    let printed = stdout(&output);
    assert!(
        printed.contains(&format!("data:    {}", install_root.join("data").display())),
        "{}",
        printed
    );
    assert!(
        printed.contains(&format!("install_dir: {}", install_root.display())),
        "{}",
        printed
    );
}

#[test]
fn manual_writes_an_os_launcher_and_prints_the_exported_paths() {
    let sandbox = Sandbox::new();
    let install_root = sandbox.root.join("persist").join("svcd");

    let output = sandbox.run(&[
        "--install",
        "--prefix",
        install_root.to_str().expect("utf8"),
        "--manual",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let launcher = install_root.join(launcher_name());
    assert!(launcher.is_file(), "launcher missing");
    let text = fs::read_to_string(&launcher).expect("read launcher");
    for needle in [
        "SVCD_DATA_DIR",
        "SVCD_LOG_DIR",
        "run --foreground",
        "--conf",
    ] {
        assert!(text.contains(needle), "launcher:\n{}", text);
    }
    assert!(
        text.contains(&install_root.join("data").display().to_string()),
        "launcher:\n{}",
        text
    );
    assert!(
        text.contains(&install_root.join("logs").display().to_string()),
        "launcher:\n{}",
        text
    );
    assert!(
        text.contains(&install_root.join(executable_name()).display().to_string()),
        "launcher:\n{}",
        text
    );
    if cfg!(windows) {
        assert!(text.contains("@echo off"), "launcher:\n{}", text);
    } else {
        assert!(text.starts_with("#!/bin/sh\n"), "launcher:\n{}", text);
    }

    let printed = stdout(&output);
    assert!(
        printed.contains(&format!("launcher: {}", launcher.display())),
        "{}",
        printed
    );
    assert!(printed.contains("SVCD_DATA_DIR="), "{}", printed);
    assert!(printed.contains("SVCD_LOG_DIR="), "{}", printed);
    assert!(printed.contains("manual:"), "{}", printed);
    assert!(
        !printed.contains("service: program"),
        "--manual must not register an init service: {}",
        printed
    );
    assert!(
        installed_conf(&sandbox).contains("install_dir = "),
        "conf must record the install root"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&launcher)
            .expect("launcher metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "launcher must be executable");
    }
}

#[test]
fn manual_dry_run_reports_the_launcher_without_writing() {
    let sandbox = Sandbox::new();
    let install_root = sandbox.root.join("persist").join("svcd");

    let output = sandbox.run(&[
        "--install",
        "--prefix",
        install_root.to_str().expect("utf8"),
        "--manual",
        "--dry-run",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let printed = stdout(&output);
    let launcher = install_root.join(launcher_name());
    assert!(
        printed.contains(&format!("launcher: {}", launcher.display())),
        "{}",
        printed
    );
    assert!(
        printed.contains(&format!(
            "SVCD_DATA_DIR={}",
            install_root.join("data").display()
        )),
        "{}",
        printed
    );
    assert!(
        printed.contains(&format!("data:    {}", install_root.join("data").display())),
        "{}",
        printed
    );
    assert!(
        !install_root.exists(),
        "dry run created {}",
        install_root.display()
    );
}

#[test]
fn manual_reports_an_ignored_init_target() {
    let sandbox = Sandbox::new();
    let install_root = sandbox.root.join("persist").join("svcd");

    let output = sandbox.run(&[
        "--install",
        "--prefix",
        install_root.to_str().expect("utf8"),
        "--manual",
        "--target",
        "systemd",
        "--dry-run",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("--target systemd ignored"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn uninstall_uses_the_recorded_install_dir_and_removes_the_launcher() {
    let sandbox = Sandbox::new();
    let install_root = sandbox.root.join("persist").join("svcd");
    let installed = sandbox.run(&[
        "--install",
        "--prefix",
        install_root.to_str().expect("utf8"),
        "--manual",
    ]);
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));

    let removed = sandbox.run(&["--uninstall", "--no-service", "--purge"]);

    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(
        !install_root.join(executable_name()).exists(),
        "binary left behind"
    );
    assert!(
        !install_root.join(launcher_name()).exists(),
        "launcher left behind"
    );
    assert!(
        !install_root.join("data").exists() && !install_root.join("logs").exists(),
        "purge left install_dir data/logs behind"
    );
}

#[test]
fn prefix_overrides_conf_install_dir_and_is_written_back() {
    let sandbox = Sandbox::new();
    let from_conf = sandbox.root.join("persist").join("from-conf");
    let prefix = sandbox.root.join("command-line-prefix");
    let conf = install_dir_conf(&sandbox, &from_conf);

    let output = sandbox.run(&[
        "--install",
        "--prefix",
        prefix.to_str().expect("utf8"),
        "--conf",
        conf.to_str().expect("utf8"),
        "--no-service",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(prefix.join(executable_name()).is_file());
    assert!(
        !from_conf.join(executable_name()).exists(),
        "--prefix must win over the conf install_dir for the binary"
    );
    let conf = installed_conf(&sandbox);
    assert!(
        conf.contains(&format!("install_dir = {}", prefix.display())),
        "the CLI prefix must be written back:\n{}",
        conf
    );
    assert!(
        stdout(&output).contains(&format!("data:    {}", prefix.join("data").display())),
        "{}",
        stdout(&output)
    );
}

#[test]
fn reinstall_without_conf_reuses_the_recorded_install_dir() {
    let sandbox = Sandbox::new();
    let install_root = sandbox.root.join("persist").join("svcd");
    let first = sandbox.run(&[
        "--install",
        "--prefix",
        install_root.to_str().expect("utf8"),
        "--no-service",
    ]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));

    let second = sandbox.run(&["--install", "--no-service"]);

    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    let printed = stdout(&second);
    assert!(
        printed.contains(&format!(
            "svcd installed: {}",
            install_root.join(executable_name()).display()
        )),
        "{}",
        printed
    );
    assert!(install_root.join(executable_name()).is_file());
}

#[test]
fn svc_logs_follows_the_recorded_install_dir() {
    let sandbox = Sandbox::new();
    let install_root = sandbox.root.join("persist").join("svcd");
    let installed = sandbox.run(&[
        "--install",
        "--prefix",
        install_root.to_str().expect("utf8"),
        "--no-service",
    ]);
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));

    let log = install_root.join("logs").join("svcd.log");
    fs::write(&log, "hello-from-install-dir\n").expect("seed log");

    let svc = install_root.join(if cfg!(windows) { "svc.exe" } else { "svc" });
    let listed = Command::new(&svc)
        .args(["logs", "svcd"])
        .env("SVCD_CONFIG_DIR", sandbox.config_dir())
        .output()
        .expect("run installed svc");
    assert_eq!(listed.status.code(), Some(0), "{}", stderr(&listed));
    assert_eq!(stdout(&listed), "hello-from-install-dir\n");
}
