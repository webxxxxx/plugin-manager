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
