use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "svcd-install-cli-{}-{}-{}",
            std::process::id(),
            nanos,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create sandbox");
        Sandbox { root }
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_svcd"))
            .args(args)
            .env("SVCD_CONFIG_DIR", self.config_dir())
            .env("SVCD_DATA_DIR", self.data_dir())
            .env("SVCD_LOG_DIR", self.root.join("logs"))
            .env("SVCD_TEST_UNPRIVILEGED", "1")
            .output()
            .expect("run svcd")
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
    assert!(!prefix.exists());

    let installed = sandbox.run(&["--install", "--prefix", prefix_arg, "--no-service"]);
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));
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
    let listed = Command::new(&svc)
        .arg("list")
        .env("SVCD_CONFIG_DIR", sandbox.config_dir())
        .env("SVCD_DATA_DIR", sandbox.data_dir())
        .env("SVCD_LOG_DIR", sandbox.root.join("logs"))
        .output()
        .expect("run installed svc");
    assert_eq!(listed.status.code(), Some(0), "{}", stderr(&listed));
    let list_text = stdout(&listed);
    assert!(list_text.starts_with("ID"), "list:\n{}", list_text);
    assert!(
        !list_text
            .lines()
            .any(|line| line.split_whitespace().next() == Some("svc")),
        "list:\n{}",
        list_text
    );

    let removed = sandbox.run(&["--uninstall", "--prefix", prefix_arg, "--no-service"]);
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(!prefix.join(exe).exists());
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
fn self_install_warns_when_lan_default_has_no_password() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.root.join("prefix");
    let prefix_arg = prefix.to_str().expect("utf8");
    let conf = sandbox.root.join("svcd.conf");
    let text = format!(
        "# runtime settings\nhttp_port = 0\nlan_default = true\nplugins_dir = {}\nservices_dir = {}\n",
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
    assert!(
        stderr(&installed).contains("LAN not enabled"),
        "install must refuse to enable LAN without a password: {}",
        stderr(&installed)
    );
    assert!(!sandbox.config_dir().join("lan.json").exists());
    assert!(!sandbox.config_dir().join("lan-token").exists());
    let copied = sandbox.config_dir().join("svcd.conf");
    assert_eq!(
        fs::read_to_string(&copied).expect("copied conf"),
        format!("{text}install_dir = {}\n", prefix.display()),
        "the CLI prefix is recorded in the installed conf"
    );
}

#[test]
fn invalid_svcd_conf_is_rejected_without_overwriting_the_installed_one() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.root.join("prefix");
    let prefix_arg = prefix.to_str().expect("utf8");
    let conf = sandbox.root.join("svcd.conf");
    let conf_arg = conf.to_str().expect("utf8");
    let good = "http_port = 0\n";
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
    let text = format!("install_dir = {}\nhttp_port = 0\n", install_dir.display());
    fs::write(&conf, text).expect("write source conf");
    conf
}

fn installed_conf(sandbox: &Sandbox) -> String {
    fs::read_to_string(sandbox.config_dir().join("svcd.conf")).expect("read installed conf")
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
