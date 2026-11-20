//! 插件配置：读模板、校验候选、原子替换与跨进程锁。

use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;
use crate::manage::model::layout::{current_version, plugin_dir};
use crate::manage::ops::scan::read_manifest;
use crate::platform::paths::Paths;

pub fn lock_config(paths: &Paths, id: &str) -> Result<fs::File, ProblemError> {
    let path = plugin_dir(paths, id).join("config.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| Problem::Internal.error(format!("open {}: {}", path.display(), error)))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Problem::Internal.error(format!(
                "lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(file)
}

pub fn write_config(path: &Path, data: &[u8]) -> Result<(), ProblemError> {
    fs::write(path, data)
        .map_err(|error| Problem::Internal.error(format!("write {}: {}", path.display(), error)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o640)).map_err(|error| {
            Problem::Internal.error(format!("chmod {}: {}", path.display(), error))
        })?;
    }
    Ok(())
}

pub fn config_template(paths: &Paths, id: &str) -> Option<(Vec<u8>, package::Manifest)> {
    let version = current_version(paths, id)?;
    let version_dir = plugin_dir(paths, id).join(version);
    let manifest = read_manifest(&version_dir).ok()?;
    let config = manifest.config.as_deref().unwrap_or("config/config");
    let data = fs::read(version_dir.join(config)).ok()?;
    Some((data, manifest))
}

pub fn config_check(paths: &Paths, id: &str, candidate: &Path) -> Result<(), ProblemError> {
    let Some(version) = current_version(paths, id) else {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    };
    let version_dir = plugin_dir(paths, id).join(&version);
    let manifest = read_manifest(&version_dir)?;
    let Some(check) = manifest.config_check else {
        return Ok(());
    };
    package::check_path(&check.path)?;
    let program = version_dir.join(&check.path);
    if !program.is_file() {
        return Err(
            Problem::Conflict.error(format!("configCheck is missing: {}", program.display()))
        );
    }
    let mut command = check_command(&program);
    command
        .args(&check.args)
        .current_dir(&version_dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "SystemRoot",
            std::env::var_os("SystemRoot").unwrap_or_default(),
        )
        .env("WINDIR", std::env::var_os("WINDIR").unwrap_or_default())
        .env("SVCD_PLUGIN_ID", id)
        .env("SVCD_PLUGIN_VERSION", &version)
        .env("SVCD_DATA_DIR", &paths.data_dir)
        .env("SVCD_CONFIG_DIR", &paths.config_dir)
        .env("SVCD_CONFIG", candidate)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| Problem::Conflict.error(format!("spawn configCheck: {}", error)))?;
    let _ = crate::platform::host::process::adopt(&child);
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let mut stderr = String::new();
                if let Some(mut handle) = child.stderr.take() {
                    use std::io::Read;
                    let _ = handle.read_to_string(&mut stderr);
                }
                return Err(Problem::Conflict.error(format!(
                    "configCheck failed ({}): {}",
                    status,
                    stderr.trim()
                )));
            }
            Ok(None) => {}
            Err(error) => return Err(Problem::Conflict.error(format!("configCheck: {}", error))),
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Problem::Conflict.error("configCheck timed out after 10s"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn replace_config(
    paths: &Paths,
    id: &str,
    candidate: &[u8],
    check: bool,
) -> Result<(), ProblemError> {
    let directory = plugin_dir(paths, id);
    if !directory.is_dir() {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    }
    if candidate.len() > 1024 * 1024 {
        return Err(Problem::TooLarge.error("config exceeds 1 MiB"));
    }
    let target = directory.join("config");
    let temporary = directory.join(format!("config.tmp.{}", std::process::id()));
    write_config(&temporary, candidate)?;
    if check {
        if let Err(error) = config_check(paths, id, &temporary) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    }
    if target.exists() {
        fs::copy(&target, directory.join("config.last-good"))
            .map_err(|error| Problem::Internal.error(format!("save last-good: {}", error)))?;
    }
    fs::rename(&temporary, &target)
        .map_err(|error| Problem::Internal.error(format!("replace config: {}", error)))?;
    Ok(())
}

pub(crate) fn write_hash(path: &Path, hash: &str) -> Result<(), ProblemError> {
    fs::write(path, hash)
        .map_err(|error| Problem::Internal.error(format!("write {}: {}", path.display(), error)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o640));
    }
    Ok(())
}

#[cfg(windows)]
fn check_command(program: &Path) -> Command {
    let extension = program
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "cmd" || extension == "bat" {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command = Command::new(shell);
        command.arg("/C").arg(program);
        return command;
    }
    Command::new(program)
}

#[cfg(not(windows))]
fn check_command(program: &Path) -> Command {
    Command::new(program)
}

/// Serialise config writes inside this process. The on-disk `config.lock`
/// covers other processes on Unix; this mutex also covers Windows, where
/// `flock` is a no-op, and keeps two threads from sharing the same
/// `config.tmp.<pid>` path.
pub(crate) static CONFIG_WRITES: Mutex<()> = Mutex::new(());

pub(crate) fn config_lock() -> MutexGuard<'static, ()> {
    CONFIG_WRITES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::failure::Problem;
    use crate::manage::model::layout::{active_config, plugin_dir};
    use crate::manage::test_support::*;

    #[test]
    fn replace_config_writes_last_good_and_checks_candidates() {
        make_plugin("plain", "old", false);
        let active = active_config(paths(), "plain").expect("active");
        assert_eq!(std::fs::read(&active).expect("read"), b"old");
        replace_config(paths(), "plain", b"new", false).expect("replace");
        assert_eq!(std::fs::read(&active).expect("read"), b"new");
        assert_eq!(
            std::fs::read(plugin_dir(paths(), "plain").join("config.last-good"))
                .expect("last-good"),
            b"old"
        );
    }

    #[test]
    fn config_check_rejects_a_bad_candidate_and_keeps_the_active_one() {
        make_plugin("checked", "old", true);
        replace_config(paths(), "checked", b"good config", true).expect("good");
        let error = replace_config(paths(), "checked", b"bad config", true).expect_err("bad");
        assert!(error.detail().contains("configCheck"), "{error}");
        assert_eq!(error.problem, Problem::Conflict);
        assert_eq!(
            std::fs::read(active_config(paths(), "checked").expect("active")).expect("read"),
            b"good config"
        );
    }

    #[test]
    fn a_config_refusal_carries_its_status_family() {
        make_plugin("families", "old", false);

        let error =
            replace_config(paths(), "no-such-plugin", b"x", false).expect_err("unknown plugin");
        assert_eq!(error.problem, Problem::UnknownPlugin);

        let oversized = vec![b'x'; 1024 * 1024 + 1];
        let error = replace_config(paths(), "families", &oversized, false).expect_err("too large");
        assert_eq!(error.problem, Problem::TooLarge);
    }
}
