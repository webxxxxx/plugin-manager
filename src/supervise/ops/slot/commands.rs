//! Building the OS command line for a service, a hook action or a payload
//! script. The layout-specific wrapping (interpreter, PowerShell, cmd) and the
//! parent-death configuration live here; `Slot` only spawns the result.

use std::collections::BTreeMap;
use std::path::Path;
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

/// A `lifecycle.start` action is a preparation step: it must exit, and the
/// supervised process only starts afterwards.
pub(crate) fn hook_command(
    id: &str,
    action: &str,
    hook: &Hook,
    paths: &Paths,
    plugin_dir: Option<&Path>,
) -> Command {
    build_action_command(id, action, hook, true, paths, plugin_dir)
}

#[allow(clippy::too_many_arguments)]
fn build_action_command(
    id: &str,
    action: &str,
    hook: &Hook,
    hook_lifetime: bool,
    paths: &Paths,
    plugin_dir: Option<&Path>,
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
    // A plugin is third-party code: it only learns the three directories it
    // needs to place its own files. `SVCD_CONFIG_DIR` would hand it
    // `svcd.conf` (password hash) and `trust.d`, and `SVCD_DATA_DIR` the
    // daemon's whole state tree. A core service is written by the operator and
    // keeps the old environment (#312).
    match plugin_dir {
        Some(_) => apply_plugin_child_env(&mut command, plugin_dir, paths),
        None => apply_core_child_env(&mut command, id, action, paths),
    }
    command
}

/// The `SVCD_*` surface a core service process receives. Core services are
/// written by the operator, so they keep the daemon's directory roots.
pub(crate) fn apply_core_child_env(command: &mut Command, id: &str, action: &str, paths: &Paths) {
    command
        .env("SVCD_SERVICE_ID", id)
        .env("SVCD_ACTION", action)
        .env("SVCD_DATA_DIR", &paths.data_dir)
        .env("SVCD_CONFIG_DIR", &paths.config_dir)
        .env("SVCD_LOG_DIR", &paths.logs_dir);
}

/// The `SVCD_*` surface a plugin child receives.
///
/// Plugins are third-party code: they learn where to put their own files, not
/// where the daemon keeps its config (`svcd.conf` holds the password hash,
/// `trust.d` the accepted keys) or its data. Removing the rest matters because
/// the daemon's own environment carries `SVCD_*` too — a service unit or the
/// `--manual` launcher exports them, and children inherit them unless they are
/// taken away here (#312).
pub(crate) fn apply_plugin_child_env(
    command: &mut Command,
    plugin_dir: Option<&Path>,
    paths: &Paths,
) {
    if let Some(plugin_dir) = plugin_dir {
        plugin_child_env(command, plugin_dir, paths);
    }
}

fn plugin_child_env(command: &mut Command, plugin_dir: &Path, paths: &Paths) {
    for key in [
        "SVCD_DATA_DIR",
        "SVCD_CONFIG_DIR",
        "SVCD_SERVICE_ID",
        "SVCD_ACTION",
    ] {
        command.env_remove(key);
    }
    command
        .env("SVCD_PLUGIN_DIR", plugin_dir)
        .env("SVCD_LOG_DIR", &paths.logs_dir)
        .env("SVCD_TEMP_DIR", &paths.temp_dir);
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
