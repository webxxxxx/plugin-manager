//! Real Windows SCM integration for `svcd run --foreground`.
//!
//! The seam is the SCM start contract: a service binary must answer
//! `StartServiceCtrlDispatcher` and report `SERVICE_RUNNING` within the
//! 30-second start window. The test installs a throwaway service pointed at the
//! built binary, starts it with `sc.exe`, and asserts the SCM observes
//! `RUNNING`. Installer-side binPath mistakes are covered by the dry-run plan
//! tests in `tests/install.rs`.
//!
//! Creating and starting a service needs Administrator; without elevation the
//! test reports a skip rather than pretending to pass.

#![cfg(windows)]

use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// A throwaway SCM service that removes itself when the test ends, pass or fail.
struct TestService {
    name: String,
    root: PathBuf,
}

impl TestService {
    fn create(name: &str, root: &Path) -> Self {
        let exe = Path::new(env!("CARGO_BIN_EXE_svcd"));
        let bin_path = format!("\"{}\" run --foreground", exe.display());
        let created = run(&[
            "sc.exe",
            "create",
            name,
            "binPath=",
            &bin_path,
            "start=",
            "demand",
            "DisplayName=",
            "svcd SCM integration test",
        ]);
        assert!(
            created.status.success(),
            "sc create failed:\n{}",
            output_text(&created)
        );

        let service = TestService {
            name: name.to_string(),
            root: root.to_path_buf(),
        };

        // Services get the machine environment plus this REG_MULTI_SZ value.
        // Isolating the config dir also isolates the control pipe, whose name is
        // derived from the config path, from any svcd already running on the host.
        let environment = format!(
            "SVCD_CONFIG_DIR={}\\0SVCD_DATA_DIR={}\\0SVCD_LOG_DIR={}\\0SVCD_HTTP_PORT=0\\0SVCD_TEST_SCM_NAME={}",
            root.join("config").display(),
            root.join("data").display(),
            root.join("logs").display(),
            name,
        );
        let key = format!("HKLM\\SYSTEM\\CurrentControlSet\\Services\\{}", name);
        let env_set = run(&[
            "reg.exe",
            "add",
            &key,
            "/v",
            "Environment",
            "/t",
            "REG_MULTI_SZ",
            "/d",
            &environment,
            "/f",
        ]);
        assert!(
            env_set.status.success(),
            "reg add Environment failed:\n{}",
            output_text(&env_set)
        );

        service
    }
}

impl Drop for TestService {
    fn drop(&mut self) {
        let _ = Command::new("sc.exe").args(["stop", &self.name]).output();
        let _ = wait_for_state(&self.name, "STOPPED", Duration::from_secs(35));
        let _ = Command::new("sc.exe").args(["delete", &self.name]).output();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(args[0])
        .args(&args[1..])
        .output()
        .unwrap_or_else(|error| panic!("run {:?}: {}", args, error))
}

fn output_text(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn wait_for_state(name: &str, wanted: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let state = Command::new("sc.exe")
            .args(["query", name])
            .output()
            .ok()
            .map(|query| String::from_utf8_lossy(&query.stdout).to_string());
        if state.as_deref().is_some_and(|text| text.contains(wanted)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

fn unique_service_name() -> String {
    format!("svcd-scm-test-{}-{}", std::process::id(), nanos())
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}

fn unique_root() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join(format!("svcd-scm-test-{}-{}", std::process::id(), nanos()))
}

#[test]
fn starting_the_scm_service_reaches_running() {
    if !is_elevated() {
        eprintln!(
            "skipping starting_the_scm_service_reaches_running: run as Administrator to create and start a Windows service"
        );
        return;
    }

    let root = unique_root();
    fs::create_dir_all(root.join("config")).expect("create config dir");
    fs::create_dir_all(root.join("data")).expect("create data dir");
    fs::create_dir_all(root.join("logs")).expect("create logs dir");

    let service = TestService::create(&unique_service_name(), &root);

    let started = run(&["sc.exe", "start", &service.name]);
    assert!(
        started.status.success(),
        "sc start failed:\n{}",
        output_text(&started)
    );
    assert!(
        wait_for_state(&service.name, "RUNNING", Duration::from_secs(10)),
        "service {} did not reach RUNNING:\n{}",
        service.name,
        output_text(&run(&["sc.exe", "query", &service.name]))
    );
}
