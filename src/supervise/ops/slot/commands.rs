//! Building the OS command line for a service, a hook action or a payload
//! script. The layout-specific wrapping (interpreter, PowerShell, cmd) and the
//! parent-death configuration live here; `Slot` only spawns the result.

use std::collections::BTreeMap;
use std::process::{Child, Command};

use crate::kernel::registry::{Exec, Hook, Interpreter};
use crate::platform::paths::Paths;

pub(super) fn command_for(exec: &Exec) -> Command {
    match &exec.interpreter {
        Some(interpreter) => {
            command_for_interpreter(interpreter, &exec.path, &exec.args, &exec.env, false)
        }
        None => command_for_parts(&exec.path, &exec.args, &exec.env, false),
    }
}

pub(crate) fn hook_command(id: &str, action: &str, hook: &Hook, paths: &Paths) -> Command {
    build_action_command(id, action, hook, true, paths)
}

/// A `lifecycle.start` action *is* the supervised foreground process, so it is
/// adoptable after a daemon restart just like `exec`.
pub(super) fn service_hook_command(id: &str, action: &str, hook: &Hook, paths: &Paths) -> Command {
    build_action_command(id, action, hook, false, paths)
}

fn build_action_command(
    id: &str,
    action: &str,
    hook: &Hook,
    hook_lifetime: bool,
    paths: &Paths,
) -> Command {
    let mut command = match &hook.interpreter {
        Some(interpreter) => command_for_interpreter(
            interpreter,
            &hook.path,
            &hook.args,
            &hook.env,
            hook_lifetime,
        ),
        None => command_for_parts(&hook.path, &hook.args, &hook.env, hook_lifetime),
    };
    command
        .env("SVCD_SERVICE_ID", id)
        .env("SVCD_ACTION", action)
        .env("SVCD_DATA_DIR", &paths.data_dir)
        .env("SVCD_CONFIG_DIR", &paths.config_dir)
        .env("SVCD_LOG_DIR", &paths.logs_dir);
    command
}

/// A script payload is not executable on its own in every layout: the manifest
/// resolved an interpreter at install time, so the supervisor runs
/// `interp [interp args] script [action args]`.
fn command_for_interpreter(
    interpreter: &Interpreter,
    script: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    hook: bool,
) -> Command {
    let mut command = Command::new(&interpreter.path);
    command
        .args(&interpreter.args)
        .arg(script)
        .args(args)
        .envs(env);
    configure(command, hook)
}

fn command_for_parts(
    path: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    hook: bool,
) -> Command {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "ps1" {
        #[cfg(windows)]
        {
            let mut command = Command::new("powershell");
            command
                .arg("-NoProfile")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-File")
                .arg(path)
                .args(args)
                .envs(env);
            return configure(command, hook);
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("pwsh");
            command
                .arg("-NoProfile")
                .arg("-File")
                .arg(path)
                .args(args)
                .envs(env);
            return configure(command, hook);
        }
    }
    #[cfg(windows)]
    if extension == "cmd" || extension == "bat" {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command = Command::new(shell);
        command.arg("/C").arg(path).args(args).envs(env);
        return configure(command, hook);
    }
    let mut command = Command::new(path);
    command.args(args).envs(env);
    configure(command, hook)
}

/// Services are adoptable after a daemon restart, hooks are not.
fn configure(mut command: Command, hook: bool) -> Command {
    if hook {
        crate::platform::host::process::configure(
            &mut command,
            crate::platform::host::process::Kind::Hook,
        );
    } else {
        crate::platform::host::process::configure(
            &mut command,
            crate::platform::host::process::Kind::Service,
        );
    }
    command
}

/// Kill the child a failed stdio registration left behind, then surface the
/// reason. Shared by every registration failure in [`Slot::spawn`].
pub(super) fn abandon(child: &mut Child, reason: impl Into<String>) -> Result<u32, String> {
    let _ = child.kill();
    let _ = child.wait();
    Err(reason.into())
}
