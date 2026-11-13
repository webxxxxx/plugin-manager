//! Windows SCM integration for `svcd run --foreground`.
//!
//! The SCM starts the service binary and then waits for it to connect with
//! `StartServiceCtrlDispatcherW`. A console-style daemon never answers, so the
//! start request times out with 1053 (`ERROR_SERVICE_REQUEST_TIMEOUT`) even
//! though the daemon itself is healthy. The same binary therefore probes for
//! SCM ownership first: a console run gets
//! `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`, which means "not started by the
//! SCM" and falls back to [`crate::daemon::run`] exactly as before.
//!
//! Once attached, the dispatcher owns the main thread and the daemon runs on a
//! worker thread so `ServiceMain` can report `START_PENDING`, wait until the
//! control channel really answers, and only then report `RUNNING`. A stop
//! request only flips the daemon's shutdown flag; the actual teardown happens
//! on the daemon thread, and `ServiceMain` reports `STOPPED` when it returns.

use std::ffi::c_void;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_SERVICE_SPECIFIC_ERROR,
};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS,
};

use crate::channel;
use crate::install::SCM_NAME;
use crate::paths;

/// The SCM gives a start request 30 seconds; report the same hint and probe for
/// readiness well inside it.
const START_WAIT_HINT_MS: u32 = 30_000;
const READY_TIMEOUT_MS: u64 = 20_000;
/// Leave headroom for the stop request the SCM allows; the daemon's own
/// shutdown budget is 30 seconds.
const STOP_WAIT_HINT_MS: u32 = 30_000;

static STATUS_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// Last reported state, so `SERVICE_CONTROL_INTERROGATE` can answer without
/// waking the daemon.
static STATE: AtomicU32 = AtomicU32::new(SERVICE_START_PENDING);
/// Exit code of the daemon thread, returned once the dispatcher returns.
static EXIT_CODE: AtomicI32 = AtomicI32::new(1);

/// The service name registered by the install plan. Tests start a throwaway
/// service under a unique name and pass it through the service environment, so
/// the dispatcher and the control handler register the name the SCM is actually
/// starting.
fn service_name() -> String {
    std::env::var("SVCD_TEST_SCM_NAME").unwrap_or_else(|_| SCM_NAME.to_string())
}

static SERVICE_CONF: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Attach to the SCM and run the daemon there.
///
/// `None` means the process was not started by the SCM (for example
/// `svcd run --foreground` from a console) and the caller should run the
/// daemon directly. `Some(code)` is the service exit code after the SCM has
/// stopped the service.
pub fn run_if_service(conf: Option<PathBuf>) -> Option<i32> {
    let _ = SERVICE_CONF.set(conf);
    let mut name = wide(&service_name());
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: null_mut(),
            lpServiceProc: None,
        },
    ];

    // Blocks on the main thread until the last service reports STOPPED.
    let attached = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    if attached != 0 {
        return Some(EXIT_CODE.load(Ordering::SeqCst));
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT {
        return None;
    }
    eprintln!(
        "svcd: cannot connect to the service control manager: error {}",
        error
    );
    Some(1)
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name = wide(&service_name());
    let handle = RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), null_mut());
    if handle.is_null() {
        EXIT_CODE.store(1, Ordering::SeqCst);
        return;
    }
    STATUS_HANDLE.store(handle, Ordering::SeqCst);
    report(SERVICE_START_PENDING, 0, 1, START_WAIT_HINT_MS, 0, 0);

    let daemon = thread::Builder::new()
        .name("svcd-daemon".into())
        .spawn(move || crate::daemon::run(SERVICE_CONF.get().cloned().flatten()));
    let daemon = match daemon {
        Ok(daemon) => daemon,
        Err(error) => {
            eprintln!("svcd: cannot start the daemon thread: {}", error);
            finish(1);
            return;
        }
    };

    // `sc start` returns once RUNNING is reported, so only claim it after the
    // control channel answers; a daemon that dies during init fails the start
    // instead of looking healthy for an instant.
    if wait_until_ready(&daemon) {
        report(
            SERVICE_RUNNING,
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
            0,
            0,
            0,
            0,
        );
    }

    let code = daemon.join().unwrap_or(1);
    finish(code);
}

fn finish(code: i32) {
    EXIT_CODE.store(code, Ordering::SeqCst);
    if code == 0 {
        report(SERVICE_STOPPED, 0, 0, 0, 0, 0);
    } else {
        report(
            SERVICE_STOPPED,
            0,
            0,
            0,
            ERROR_SERVICE_SPECIFIC_ERROR,
            code as u32,
        );
    }
}

/// Poll the management channel until the daemon answers. Returns `false` when
/// the daemon thread exited first, `true` when it is reachable or the probe
/// deadline expired (a slow start must not block the SCM forever).
fn wait_until_ready(daemon: &JoinHandle<i32>) -> bool {
    let deadline = Instant::now() + Duration::from_millis(READY_TIMEOUT_MS);
    loop {
        if daemon.is_finished() {
            return false;
        }
        if channel::request(&paths::config_dir(), "svcd.ping", serde_json::json!({})).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            // Return to the SCM immediately: the daemon thread performs the
            // bounded shutdown and `ServiceMain` reports STOPPED afterwards.
            report(SERVICE_STOP_PENDING, 0, 1, STOP_WAIT_HINT_MS, 0, 0);
            crate::daemon::request_shutdown();
        }
        SERVICE_CONTROL_INTERROGATE => {
            let state = STATE.load(Ordering::SeqCst);
            report(state, accepted_controls(state), 0, 0, 0, 0);
        }
        _ => {}
    }
    0
}

fn accepted_controls(state: u32) -> u32 {
    if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
    } else {
        0
    }
}

fn report(
    state: u32,
    controls: u32,
    checkpoint: u32,
    wait_hint: u32,
    win32_exit_code: u32,
    service_exit_code: u32,
) {
    let handle = STATUS_HANDLE.load(Ordering::SeqCst);
    if handle.is_null() {
        return;
    }
    STATE.store(state, Ordering::SeqCst);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: controls,
        dwWin32ExitCode: win32_exit_code,
        dwServiceSpecificExitCode: service_exit_code,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint,
    };
    unsafe { SetServiceStatus(handle, &status) };
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}
