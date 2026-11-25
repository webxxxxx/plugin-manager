//! One restart route for every caller (#317).
//!
//! A settings save, `svcd restart` and an upgrade all end the same way: this
//! process has to stop and somebody has to start it again. Who that somebody is
//! depends only on the host, so the decision lives here once instead of in each
//! caller:
//!
//! * a user-mode daemon is started again by the detached `svcd restart-helper`;
//! * a service daemon belongs to its init system, which respawns it (systemd,
//!   OpenWrt procd, a BusyBox `::respawn:` line, launchd);
//! * a `--manual` install or a Windows SCM service has nobody to lean on, and
//!   the operator gets the exact command to run by hand.
//!
//! Restarting is the last step of work that has already succeeded, so it never
//! turns a save or an upgrade into an error: the worst case is the manual hint.

use std::path::{Path, PathBuf};

use super::service::{manual_restart_command, service_can_restart_itself};
use crate::supervise::ops::restart::{spawn_helper_from, spawn_service_helper_from};

/// Who can bring the daemon back on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// User mode: the detached helper starts a fresh `svcd run --foreground`.
    User,
    /// Service mode with an init system that respawns the daemon.
    Service,
    /// Nothing here can: the operator has to run the command in the hint.
    Manual,
}

/// The decision, split from the host so the matrix is testable.
pub(crate) fn route_for(user_mode: bool, can_self_restart: bool) -> Route {
    if user_mode {
        Route::User
    } else if can_self_restart {
        Route::Service
    } else {
        Route::Manual
    }
}

/// What this host can do right now.
pub(crate) fn route() -> Route {
    route_for(
        crate::platform::paths::user_mode(),
        service_can_restart_itself(),
    )
}

/// The sentence that tells the operator to do it by hand, command included.
///
/// A user-mode install starts its daemon from the command line; a service
/// install names the command its init system answers to.
pub(crate) fn manual_hint() -> String {
    hint_for(route())
}

fn hint_for(route: Route) -> String {
    match route {
        Route::User => "restart the daemon yourself: svcd restart".to_string(),
        Route::Service | Route::Manual => format!(
            "restart the svcd service yourself: {}",
            manual_restart_command()
        ),
    }
}

/// What a restart request produced.
#[derive(Debug)]
pub(crate) struct Restarted {
    /// The detached helper's pid, when this host could start one.
    pub(crate) helper: Option<u32>,
    /// The sentence for the operator when it could not.
    pub(crate) hint: Option<String>,
}

impl Restarted {
    fn helper(pid: u32) -> Self {
        Self {
            helper: Some(pid),
            hint: None,
        }
    }

    fn manual(route: Route, detail: Option<String>) -> Self {
        let mut hint = hint_for(route);
        if let Some(detail) = detail {
            hint = format!("{} ({})", hint, detail);
        }
        Self {
            helper: None,
            hint: Some(hint),
        }
    }
}

/// Ask this host to bring the daemon back.
///
/// `executable` is the binary the helper must run when the running image is not
/// the one to start: an upgrade passes the freshly installed file, everything
/// else passes `None` for the running one.
pub(crate) fn restart_now(executable: Option<&Path>) -> Restarted {
    let route = route();
    if route == Route::Manual {
        return Restarted::manual(route, None);
    }
    match executable_of(executable).and_then(|exe| spawn(route, &exe)) {
        Ok(pid) => Restarted::helper(pid),
        Err(error) => Restarted::manual(
            route,
            Some(format!("the restart helper could not start: {}", error)),
        ),
    }
}

/// `Route::Manual` never reaches this; the `User` arm is the fallback so the
/// match has no unreachable arm to panic on.
fn spawn(route: Route, executable: &Path) -> std::io::Result<u32> {
    match route {
        Route::Service => spawn_service_helper_from(executable),
        Route::User | Route::Manual => spawn_helper_from(executable),
    }
}

fn executable_of(explicit: Option<&Path>) -> std::io::Result<PathBuf> {
    match explicit {
        Some(path) => Ok(path.to_path_buf()),
        None => std::env::current_exe(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user-mode daemon owns its own lifecycle; a service daemon is only
    /// restartable when an init system will respawn it.
    #[test]
    fn the_route_follows_the_install_mode_and_the_init_system() {
        assert_eq!(route_for(true, false), Route::User);
        assert_eq!(
            route_for(true, true),
            Route::User,
            "user mode never hands the daemon to an init system"
        );
        assert_eq!(route_for(false, true), Route::Service);
        assert_eq!(
            route_for(false, false),
            Route::Manual,
            "no init system means nobody can bring it back"
        );
    }

    /// The hint is the one place the operator reads; it must name a command this
    /// host actually has.
    #[test]
    fn the_manual_hint_names_a_command_this_host_has() {
        let user = hint_for(Route::User);
        assert!(user.contains("svcd restart"), "{user}");

        let service = hint_for(Route::Service);
        assert!(
            service.contains("restart the svcd service yourself"),
            "{service}"
        );
        assert!(service.contains(&manual_restart_command()), "{service}");

        // The `--manual` install and the Windows SCM service use the same
        // sentence: their command is the only difference, and it comes from the
        // platform, not from the route.
        assert_eq!(hint_for(Route::Manual), service);
    }

    /// A helper that cannot start is the same situation as a host with no init
    /// system: the work is done, the operator needs the command.
    #[test]
    fn a_helper_that_could_not_start_still_says_what_to_run() {
        let restarted = Restarted::manual(Route::Service, Some("spawn: permission denied".into()));
        assert!(restarted.helper.is_none());
        let hint = restarted.hint.unwrap_or_default();
        assert!(hint.contains("restart the svcd service yourself"), "{hint}");
        assert!(hint.contains("spawn: permission denied"), "{hint}");

        let user = Restarted::manual(Route::User, None);
        assert!(
            user.hint.unwrap_or_default().contains("svcd restart"),
            "a user install is restarted from the command line, not by an init system"
        );
    }
}
