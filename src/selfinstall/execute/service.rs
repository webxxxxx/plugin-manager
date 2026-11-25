use std::path::PathBuf;

#[cfg(all(unix, not(target_os = "macos")))]
use std::path::Path;

use super::plan_exec::dispatch;
use super::*;

/// How this host starts and supervises the daemon.
///
/// The installer already chooses between these (`Target`); the runtime
/// `svcd start|stop|status` commands used to assume systemd on every Unix,
/// which is wrong on exactly the hosts the installer supports otherwise:
/// OpenWrt registers a procd script and `--manual` registers nothing at all
/// (#286).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(all(unix, not(target_os = "macos"))),
    allow(dead_code, reason = "only the Unix-non-macOS status path builds these")
)]
pub enum Supervisor {
    /// systemd: `systemctl <verb> svcd`.
    Systemd,
    /// OpenWrt procd: the rc.common script the installer wrote.
    Procd,
    /// A BusyBox `/etc/inittab` `::respawn:` line; init owns the lifetime.
    Busybox,
    /// No init registration (`svcd --install --manual`).
    Manual,
}

/// What answered a `svcd status`.
pub enum ServiceStatus {
    /// An init system answered; this is its exit code.
    Managed(i32),
    /// Nothing on this host can be asked. The caller owns the control-channel
    /// answer because it owns config discovery, so it prints the manager and
    /// the launcher instead of a `systemctl: No such file or directory` line.
    #[cfg_attr(
        not(all(unix, not(target_os = "macos"))),
        allow(dead_code, reason = "only the Unix-non-macOS status path builds this")
    )]
    Unmanaged(Unmanaged),
}

/// The facts a caller needs to report an unsupervised daemon.
pub struct Unmanaged {
    pub supervisor: Supervisor,
    /// The launcher `--manual` wrote next to the binary; absent for BusyBox.
    pub launcher: PathBuf,
}

/// `svcd start`: start the daemon through the host's init system. User mode is
/// handled by `main` before calling this.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn service_start() -> i32 {
    service_verb("start")
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
    service_verb("stop")
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
pub fn service_status() -> ServiceStatus {
    status_of(supervisor())
}

#[cfg(target_os = "macos")]
pub fn service_status() -> ServiceStatus {
    ServiceStatus::Managed(service_launchctl(&["print", "system/com.webxxxxx.svcd"]))
}

#[cfg(windows)]
pub fn service_status() -> ServiceStatus {
    ServiceStatus::Managed(service_sc(&["query", SCM_NAME]))
}

#[cfg(not(any(unix, windows)))]
pub fn service_status() -> ServiceStatus {
    eprintln!("svcd: service status is not supported on this platform");
    ServiceStatus::Managed(EXIT_ERROR)
}

/// The launcher `--manual` wrote next to the running binary.
#[cfg_attr(
    not(all(unix, not(target_os = "macos"))),
    allow(dead_code, reason = "only the Unix-non-macOS status path builds this")
)]
pub(crate) fn launcher_path() -> PathBuf {
    std::env::current_exe()
        .map(|exe| exe.with_file_name(launcher_name()))
        .unwrap_or_else(|_| PathBuf::from(launcher_name()))
}

/// Which init system this host actually has. Only the Unix-non-macOS branch
/// has more than one answer; the pure decision is below so the matrix is
/// testable without a host.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn supervisor() -> Supervisor {
    detect_supervisor(
        Path::new(PROCD_SCRIPT).is_file(),
        which("systemctl").is_some(),
        inittab_respawns_svcd(),
    )
}

/// Whether this host can bring the daemon back after a self-restart.
///
/// The three init systems all restart a process that exits on their own: procd
/// respawns the instance, an inittab `::respawn:` line respawns it, and
/// systemd's `Restart=on-failure` covers the explicit `systemctl start` the
/// helper issues afterwards. `Manual` registered nothing, so there is nobody to
/// bring it back.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
pub(crate) fn can_self_restart(supervisor: Supervisor) -> bool {
    matches!(
        supervisor,
        Supervisor::Systemd | Supervisor::Procd | Supervisor::Busybox
    )
}

/// Whether a service-mode save may restart the daemon itself.
///
/// `SVCD_TEST_NO_SERVICE_RESTART` pins the "no supervisor to lean on" path: the
/// integration sandbox runs on a host that has a `systemctl` binary but no
/// service to start, so the decision must not depend on what happens to be in
/// `PATH` there.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn service_can_restart_itself() -> bool {
    if std::env::var_os("SVCD_TEST_NO_SERVICE_RESTART").is_some() {
        return false;
    }
    can_self_restart(supervisor())
}

/// launchd keeps the daemon alive (`KeepAlive`), and `kickstart -k` is itself a
/// restart, so the helper has an init system to hand over to.
#[cfg(target_os = "macos")]
pub(crate) fn service_can_restart_itself() -> bool {
    true
}

/// Windows: the SCM owns the process lifetime and its recovery actions are not
/// ours to rely on, so a save still points at `sc stop svcd && sc start svcd`.
#[cfg(windows)]
pub(crate) fn service_can_restart_itself() -> bool {
    false
}

/// The order is the installer's: an existing init registration wins over a
/// binary that merely happens to be in `PATH`.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn detect_supervisor(procd_script: bool, systemctl: bool, inittab: bool) -> Supervisor {
    if procd_script {
        Supervisor::Procd
    } else if systemctl {
        Supervisor::Systemd
    } else if inittab {
        Supervisor::Busybox
    } else {
        Supervisor::Manual
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn status_of(supervisor: Supervisor) -> ServiceStatus {
    match supervisor {
        Supervisor::Systemd => ServiceStatus::Managed(service_systemctl("is-active")),
        Supervisor::Procd => ServiceStatus::Managed(run_command(Path::new(PROCD_SCRIPT), "status")),
        Supervisor::Busybox | Supervisor::Manual => ServiceStatus::Unmanaged(Unmanaged {
            supervisor,
            launcher: launcher_path(),
        }),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn service_verb(verb: &str) -> i32 {
    match supervisor() {
        Supervisor::Systemd => service_systemctl(verb),
        Supervisor::Procd => run_command(Path::new(PROCD_SCRIPT), verb),
        Supervisor::Busybox => {
            eprintln!(
                "svcd: the daemon is respawned from {}; edit that line and \
                 `kill -HUP 1` instead of running `svcd {}`",
                INITTAB, verb
            );
            EXIT_ERROR
        }
        Supervisor::Manual => {
            eprintln!(
                "svcd: no init system manages this install (svcd --install --manual);\n\
                 start it with {} or add it to the firmware init",
                launcher_path().display()
            );
            EXIT_ERROR
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn run_command(program: &Path, verb: &str) -> i32 {
    match std::process::Command::new(program).arg(verb).status() {
        Ok(status) if status.success() => EXIT_OK,
        Ok(_) => EXIT_ERROR,
        Err(error) => {
            eprintln!("svcd: {} {}: {}", program.display(), verb, error);
            EXIT_ERROR
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// Does `/etc/inittab` respawn this daemon? The installer's BusyBox plan writes
/// exactly one `::respawn:` line naming the program.
#[cfg(all(unix, not(target_os = "macos")))]
fn inittab_respawns_svcd() -> bool {
    let Ok(text) = std::fs::read_to_string(INITTAB) else {
        return false;
    };
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix("::respawn:")
            .is_some_and(|rest| rest.contains("svcd"))
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn service_systemctl(verb: &str) -> i32 {
    match std::process::Command::new("systemctl")
        .args([verb, "svcd"])
        .status()
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #286: the runtime commands must ask the init system the installer
    /// registered with, not assume systemd.
    #[test]
    fn the_supervisor_matrix_follows_the_installer() {
        assert_eq!(
            detect_supervisor(true, true, false),
            Supervisor::Procd,
            "a procd script is a registration and wins over a stray systemctl"
        );
        assert_eq!(
            detect_supervisor(false, true, true),
            Supervisor::Systemd,
            "systemd wins over an inittab line that is not ours"
        );
        assert_eq!(detect_supervisor(false, false, true), Supervisor::Busybox);
        assert_eq!(detect_supervisor(false, false, false), Supervisor::Manual);
    }

    /// #306: a save in service mode may restart the daemon only where an init
    /// system will bring it back; a `--manual` install has nobody to lean on.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn only_a_supervisor_that_respawns_may_restart_the_daemon() {
        assert!(can_self_restart(Supervisor::Systemd));
        assert!(can_self_restart(Supervisor::Procd));
        assert!(can_self_restart(Supervisor::Busybox));
        assert!(!can_self_restart(Supervisor::Manual));
    }

    /// The launcher a manual install tells the operator to start.
    #[cfg(unix)]
    #[test]
    fn the_manual_launcher_sits_next_to_the_binary() {
        let launcher = launcher_path();
        assert_eq!(
            launcher.file_name().and_then(|name| name.to_str()),
            Some(launcher_name())
        );
    }
}
