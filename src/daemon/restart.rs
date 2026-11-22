//! Daemon self-restart (#263).
//!
//! The daemon cannot rebind its control socket or HTTP port while it is still
//! running, so it spawns a detached `svcd restart-helper <pid>` and then shuts
//! itself down. The helper waits for that pid to exit and starts
//! `svcd run --foreground` with the inherited environment, which is how a
//! user-mode daemon comes back on the same config/data/log tree.

use std::process::{Command, Stdio};

/// Spawn the detached helper and return its pid. The caller still has to stop
/// the daemon; the helper waits for exactly that.
pub(crate) fn spawn_helper() -> std::io::Result<u32> {
    // The helper and the daemon it starts must not hold this process's stdio.
    crate::platform::host::process::detach_stdio();
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .args(["restart-helper", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    command.spawn().map(|child| child.id())
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
    unsafe {
        command.pre_exec(|| {
            // A new session detaches the helper from this daemon's process
            // group, so a service manager stopping the group does not kill the
            // process that is about to start the replacement.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}
