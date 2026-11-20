use super::plan_exec::dispatch;
use super::*;

/// `svcd start`: start the installed system service. User mode is handled by
/// `main` before calling this.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn service_start() -> i32 {
    service_systemctl("start")
}

#[cfg(target_os = "macos")]
pub fn service_start() -> i32 {
    service_launchctl(&["kickstart", "-k", "system/com.webxxxxx.svcd"])
}

#[cfg(windows)]
pub fn service_start() -> i32 {
    service_sc(&["start", SCM_NAME])
}

#[cfg(not(any(unix, windows)))]
pub fn service_start() -> i32 {
    eprintln!("svcd: service start is not supported on this platform");
    EXIT_ERROR
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn service_stop() -> i32 {
    service_systemctl("stop")
}

#[cfg(target_os = "macos")]
pub fn service_stop() -> i32 {
    service_launchctl(&["kill", "SIGTERM", "system/com.webxxxxx.svcd"])
}

#[cfg(windows)]
pub fn service_stop() -> i32 {
    service_sc(&["stop", SCM_NAME])
}

#[cfg(not(any(unix, windows)))]
pub fn service_stop() -> i32 {
    eprintln!("svcd: service stop is not supported on this platform");
    EXIT_ERROR
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn service_status() -> i32 {
    service_systemctl("is-active")
}

#[cfg(target_os = "macos")]
pub fn service_status() -> i32 {
    service_launchctl(&["print", "system/com.webxxxxx.svcd"])
}

#[cfg(windows)]
pub fn service_status() -> i32 {
    service_sc(&["query", SCM_NAME])
}

#[cfg(not(any(unix, windows)))]
pub fn service_status() -> i32 {
    eprintln!("svcd: service status is not supported on this platform");
    EXIT_ERROR
}

#[cfg(all(unix, not(target_os = "macos")))]
fn service_systemctl(verb: &str) -> i32 {
    let status = std::process::Command::new("systemctl")
        .args([verb, "svcd"])
        .status();
    match status {
        Ok(status) if status.success() => EXIT_OK,
        Ok(_) => EXIT_ERROR,
        Err(error) => {
            eprintln!("svcd: systemctl {} svcd: {}", verb, error);
            EXIT_ERROR
        }
    }
}

#[cfg(target_os = "macos")]
fn service_launchctl(args: &[&str]) -> i32 {
    let status = std::process::Command::new("launchctl").args(args).status();
    match status {
        Ok(status) if status.success() => EXIT_OK,
        Ok(_) => EXIT_ERROR,
        Err(error) => {
            eprintln!("svcd: launchctl: {}", error);
            EXIT_ERROR
        }
    }
}

#[cfg(windows)]
fn service_sc(args: &[&str]) -> i32 {
    let status = std::process::Command::new("sc").args(args).status();
    match status {
        Ok(status) if status.success() => EXIT_OK,
        Ok(_) => EXIT_ERROR,
        Err(error) => {
            eprintln!("svcd: sc: {}", error);
            EXIT_ERROR
        }
    }
}

pub fn install_service(args: &[String]) -> i32 {
    dispatch(Action::Install, args)
}

pub fn uninstall_service(args: &[String]) -> i32 {
    dispatch(Action::Uninstall, args)
}
