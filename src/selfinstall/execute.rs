//! Installation of `svcd` itself with the host init system.
//!
//! Each target is a data-only [`InstallPlan`]: a list of file and command
//! operations that the small executor below applies in order. Plans are pure
//! functions of an [`Layout`], so tests can assert their exact contents without
//! touching the host. `--dry-run` prints a plan and never executes it.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::platform::config::ambient_paths;

use super::plan::*;

mod path_env;
mod plan_exec;
mod restart_route;
mod self_install;
mod self_uninstall;
mod service;
mod system_path;
mod user_path;

use plan_exec::execute;
pub(crate) use restart_route::{manual_hint, restart_now};
pub use self_install::self_install;
pub use self_uninstall::self_uninstall;
#[cfg(windows)]
pub(crate) use self_uninstall::wait_for_process;
pub use service::{
    install_service, service_start, service_status, service_stop, uninstall_service, ServiceStatus,
    Supervisor, Unmanaged,
};

impl Probe {
    pub fn current() -> Self {
        Probe {
            os: current_os(),
            systemd_runtime: Path::new("/run/systemd/system").is_dir(),
            rc_common: Path::new("/etc/rc.common").exists(),
            initd_writable: path_writable(Path::new("/etc/init.d")),
        }
    }
}

#[cfg(unix)]
fn path_writable(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(not(unix))]
fn path_writable(path: &Path) -> bool {
    path.is_dir()
}

impl Layout {
    pub fn current() -> Self {
        Layout::from_paths(&ambient_paths(None))
    }

    /// The same layout from an already-resolved directory set, so the daemon
    /// does not resolve the document again just to build `ensure_conf`'s view.
    fn from_paths(paths: &crate::platform::paths::Paths) -> Self {
        Layout {
            program: default_program(),
            data_dir: paths.data_dir.clone(),
            config_dir: paths.config_dir.clone(),
            conf: None,
        }
    }
}

/// Resolve the config used by the daemon service.
///
/// Precedence: the path passed at install/startup, then
/// `<program dir>/config/svcd.conf`, then `<program dir>/svcd.conf`, then
/// `<config>/svcd.conf` (legacy), then a generated default written into the
/// config directory (falling back to the program directory when the config
/// directory is not writable).
///
/// A chosen document is the one this process runs with: missing roots are
/// filled in memory and written back, and a generated document is used for
/// this start, not only the next one (ADR-0023).
pub fn ensure_conf(
    explicit: Option<&Path>,
    settings: &mut crate::platform::settings::Settings,
    paths: &crate::platform::paths::Paths,
) -> Result<PathBuf, String> {
    let program = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let program_conf = program.as_ref().map(|parent| parent.join("svcd.conf"));
    let program_config_conf = program
        .as_ref()
        .map(|parent| parent.join("config").join("svcd.conf"));
    let config_conf = paths.config_dir.join("svcd.conf");
    // The initial paths already applied the SVCD_* overrides. Prefer them when
    // the variable is set; otherwise an install prefix supplies the missing
    // roots (ADR-0023 folding), and finally the platform default.
    let (config_default, data_default, logs_default) =
        crate::platform::config_file::defaults_from_paths(settings, paths, |name| {
            std::env::var_os(name)
        });

    let chosen = if let Some(path) = explicit {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        if path.is_file() {
            Some(path)
        } else {
            eprintln!(
                "svcd: config {} not found; falling back to the service default",
                path.display()
            );
            None
        }
    } else {
        None
    }
    .or_else(|| {
        program_config_conf
            .as_ref()
            .filter(|path| path.is_file())
            .cloned()
    })
    .or_else(|| program_conf.as_ref().filter(|path| path.is_file()).cloned())
    .or_else(|| config_conf.is_file().then_some(config_conf.clone()));

    if let Some(path) = chosen {
        crate::platform::config_file::fill_missing_with(
            settings,
            config_default,
            data_default,
            logs_default,
        );
        let missing = crate::platform::config_file::missing_roots(&path);
        for warning in crate::platform::config_file::update_filled(&path, settings, &missing) {
            eprintln!("svcd: warning: {}", warning);
        }
        return Ok(path);
    }
    crate::platform::config_file::fill_missing_with(
        settings,
        config_default,
        data_default,
        logs_default,
    );
    let text = crate::platform::config_file::generated(settings);
    let mut failures = Vec::new();
    match write_conf(&config_conf, &text) {
        Ok(()) => {
            eprintln!("svcd: created config {}", config_conf.display());
            return Ok(config_conf);
        }
        Err(error) => failures.push(format!("{}: {}", config_conf.display(), error)),
    }
    if let Some(path) = &program_conf {
        match write_conf(path, &text) {
            Ok(()) => {
                eprintln!("svcd: created config {}", path.display());
                return Ok(path.clone());
            }
            Err(error) => failures.push(format!("{}: {}", path.display(), error)),
        }
    }
    Err(format!("cannot create svcd.conf: {}", failures.join("; ")))
}

fn write_conf(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, text).map_err(|error| format!("write {}: {}", path.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

fn default_program() -> PathBuf {
    if cfg!(windows) {
        return std::env::current_exe().unwrap_or_else(|_| PathBuf::from("svcd.exe"));
    }
    // Packaging installs the multicall binary in /usr/sbin; a development
    // build falls back to the executable that is running right now.
    let installed = PathBuf::from("/usr/sbin/svcd");
    if installed.is_file() {
        installed
    } else {
        std::env::current_exe().unwrap_or(installed)
    }
}

/// The directories the installed daemon will use, resolved from the target
/// `svcd.conf` with the same [`Paths::resolve_with`] the daemon uses.
///
/// The installer targets the clean boot environment, so only
/// `SVCD_CONFIG_DIR` is read here: it decides where the installer writes.
/// Data and logs come from the materialized roots in the document, never from
/// the operator's shell (ADR-0023).
fn current_runtime_dirs(settings: Option<&crate::platform::settings::Settings>) -> RuntimeDirs {
    let settings = settings.cloned().unwrap_or_default();
    let paths = crate::platform::paths::Paths::resolve_with(&settings, |name| {
        (name == "SVCD_CONFIG_DIR")
            .then(|| std::env::var_os(name))
            .flatten()
    });
    RuntimeDirs::from_paths(&paths)
}

/// Record the effective install root in the installed `svcd.conf`, keeping
/// comments and the normal 0644 mode.
fn record_install_dir(path: &Path, install_dir: &Path) -> Result<(), String> {
    crate::platform::settings::update_value(
        path,
        "install_dir",
        &install_dir.display().to_string(),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

/// The document an install reuses when `--conf` is not passed, including the
/// reason it cannot be read.
///
/// `--install` must not upgrade the binary and register a service around a
/// document the daemon refuses: systemd would retry five times, land in
/// `failed`, and the installer would only report that registration failed
/// (ADR-0010 makes removed keys errors, not warnings).
pub(super) enum InstalledConf {
    /// No document exists in any discovered location.
    Absent,
    Loaded(Box<crate::platform::settings::Settings>),
    /// A document exists but this binary will not load it.
    Invalid(String),
}

/// Read the installed document as the daemon will read it.
///
/// The environment overrides are deliberately not applied: the service starts
/// without the installer's shell, so a `SVCD_*` variable must not make a
/// document look valid here that the unit cannot load.
pub(super) fn installed_conf() -> InstalledConf {
    let path = crate::platform::settings::discovered_conf();
    if !path.is_file() {
        return InstalledConf::Absent;
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return InstalledConf::Invalid(format!("cannot read {}: {}", path.display(), error))
        }
    };
    match crate::platform::settings::parse(&text, path.parent().unwrap_or(Path::new("."))) {
        Ok(mut settings) => {
            crate::platform::config_file::fill_missing(&mut settings);
            InstalledConf::Loaded(Box::new(settings))
        }
        Err(error) => InstalledConf::Invalid(format!("{}: {}", path.display(), error)),
    }
}

/// The installed document's recorded settings, when one exists and parses.
///
/// Reinstall and `--uninstall` use it to find an `install_dir` without a
/// `--conf` argument (uninstall does not accept one). Discovery covers
/// `<program dir>/config/svcd.conf`, so a prefixed install is found without
/// environment variables.
fn installed_conf_settings() -> Option<crate::platform::settings::Settings> {
    // A document this binary cannot load is "no usable document" for callers
    // that only need the recorded roots (uninstall must still work on a broken
    // install); `--install` uses `installed_conf` and refuses it instead.
    match installed_conf() {
        InstalledConf::Loaded(settings) => Some(*settings),
        InstalledConf::Absent | InstalledConf::Invalid(_) => None,
    }
}

/// The installed document for a known prefix.
///
/// The Windows uninstall helper runs from the temp directory, so
/// current_exe() no longer points at the install tree. The caller passes
/// --prefix to the helper; read the document from the prefixed layout directly
/// before falling back to the normal discovery.
fn installed_conf_settings_for(
    prefix: Option<&Path>,
) -> Option<crate::platform::settings::Settings> {
    if let Some(prefix) = prefix {
        for path in [
            prefix.join("config").join("svcd.conf"),
            prefix.join("svcd.conf"),
        ] {
            if !path.is_file() {
                continue;
            }
            let text = fs::read_to_string(&path).ok()?;
            let mut settings =
                crate::platform::settings::parse(&text, path.parent().unwrap_or(Path::new(".")))
                    .ok()?;
            crate::platform::config_file::fill_missing(&mut settings);
            return Some(settings);
        }
    }
    installed_conf_settings()
}

/// Keep a discovered install prefix across the Windows helper process.
#[cfg(windows)]
fn helper_args(args: &[String], prefix: Option<&Path>) -> Vec<String> {
    let mut helper = args.to_vec();
    let has_prefix = args
        .iter()
        .any(|arg| arg == "--prefix" || arg.starts_with("--prefix="));
    if !has_prefix {
        if let Some(prefix) = prefix {
            helper.push("--prefix".into());
            helper.push(prefix.display().to_string());
        }
    }
    helper
}

#[derive(Default)]
struct SelfRequest {
    prefix: Option<PathBuf>,
    conf: Option<PathBuf>,
    no_service: bool,
    manual: bool,
    purge: bool,
    dry_run: bool,
    target: Option<Target>,
}

fn parse_self(args: &[String], uninstall: bool) -> Result<SelfRequest, String> {
    let mut request = SelfRequest::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--no-service" => request.no_service = true,
            "--manual" if uninstall => return Err("--manual only applies to --install".into()),
            "--manual" => request.manual = true,
            "--dry-run" => request.dry_run = true,
            "--purge" if uninstall => request.purge = true,
            "--purge" => return Err("--purge only applies to --uninstall".into()),
            "--prefix" => {
                index += 1;
                let value = args.get(index).ok_or("--prefix requires a value")?;
                request.prefix = Some(PathBuf::from(value));
            }
            "--conf" | "--config" if uninstall => {
                return Err("--conf/--config only applies to --install".into())
            }
            "--conf" | "--config" => {
                index += 1;
                let value = args.get(index).ok_or("--conf requires a value")?;
                if request.conf.replace(PathBuf::from(value)).is_some() {
                    return Err("--conf specified more than once".into());
                }
            }
            "--target" => {
                index += 1;
                let value = args.get(index).ok_or("--target requires a value")?;
                request.target = Some(Target::parse(value).ok_or("unknown target")?);
            }
            other => {
                if let Some(value) = other.strip_prefix("--prefix=") {
                    if value.is_empty() {
                        return Err("--prefix requires a value".into());
                    }
                    request.prefix = Some(PathBuf::from(value));
                } else if let Some(value) = other
                    .strip_prefix("--conf=")
                    .or_else(|| other.strip_prefix("--config="))
                {
                    if uninstall {
                        return Err("--conf/--config only applies to --install".into());
                    }
                    if value.is_empty() {
                        return Err("--conf requires a value".into());
                    }
                    if request.conf.replace(PathBuf::from(value)).is_some() {
                        return Err("--conf specified more than once".into());
                    }
                } else if let Some(value) = other.strip_prefix("--target=") {
                    request.target = Some(Target::parse(value).ok_or("unknown target")?);
                } else {
                    return Err(format!("unexpected argument: {}", other));
                }
            }
        }
        index += 1;
    }
    Ok(request)
}

fn default_install_prefix() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        base.join("svcd")
    } else {
        PathBuf::from("/usr/sbin")
    }
}

/// Replace the installed binary without ever writing through the live file.
///
/// Copy to a temporary sibling and rename it into place: on Unix the running
/// daemon keeps the old inode, so upgrading an `install_dir` deployment does
/// not need to stop the service first (a direct overwrite would fail ETXTBSY).
fn copy_executable(source: &Path, target: &Path) -> io::Result<()> {
    let temporary = temporary_path(target)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let result = fs::copy(source, &temporary).and_then(|_| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))?;
        }
        replace_file(&temporary, target).map_err(io::Error::other)
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Give `target` a directory entry for `source`.
///
/// Unix links, so one copy of the binary serves all four names: that is the
/// shape the release archive ships, and the only one that fits a small `/data`
/// (#304). A file system without hard links falls back to a copy, and Windows
/// always copies because it cannot unlink the last hard link of a running
/// image. The link is built under a temporary name and renamed into place, so
/// a name never disappears while an upgrade is replacing it.
pub(super) fn install_alias(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        if let Ok(staged) = temporary_path(target) {
            let _ = fs::remove_file(&staged);
            if fs::hard_link(source, &staged).is_ok() {
                if replace_file(&staged, target).is_ok() {
                    return Ok(());
                }
                let _ = fs::remove_file(&staged);
            }
        }
    }
    copy_executable(source, target)
}

/// The store's release key, embedded in the installer so a fresh svcd
/// --install seeds <config>/trust.d/signing.pub before any package is
/// installed. An existing file wins: the operator may have rotated the root.
const STORE_KEY_ID: &str = "signing";
const STORE_PUBLIC_KEY: &str = include_str!("../assets/signing.pub");

/// Validate the embedded key, then write it unless the operator already put a
/// key with the same id there. Returns the path written, or `None` when an
/// existing file won.
fn seed_store_trust(config_dir: &Path) -> Result<Option<PathBuf>, String> {
    crate::manage::validate_public_key(STORE_PUBLIC_KEY.as_bytes())
        .map_err(|error| format!("embedded store key is invalid: {}", error))?;
    let target = config_dir
        .join("trust.d")
        .join(format!("{STORE_KEY_ID}.pub"));
    if target.exists() {
        return Ok(None);
    }
    write_file(&target, STORE_PUBLIC_KEY, 0o644)?;
    Ok(Some(target))
}

fn write_file(path: &Path, content: &str, mode: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {}", parent.display(), error))?;
        }
    }
    let temporary = temporary_path(path)?;
    let result =
        write_temporary(&temporary, content, mode).and_then(|()| replace_file(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(path: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .ok_or_else(|| format!("invalid path: {}", path.display()))?;
    Ok(path.with_file_name(format!(
        ".{}.tmp.{}",
        name.to_string_lossy(),
        std::process::id()
    )))
}

fn write_temporary(path: &Path, content: &str, mode: u32) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| format!("cannot open {}: {}", path.display(), error))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("cannot write {}: {}", path.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| format!("cannot set mode on {}: {}", path.display(), error))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    file.sync_all()
        .map_err(|error| format!("cannot sync {}: {}", path.display(), error))?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(temporary: &Path, path: &Path) -> Result<(), String> {
    // Windows cannot rename over an existing file; the replace is not atomic
    // there, but the destination is recreated immediately.
    if path.exists() {
        fs::remove_file(path)
            .map_err(|error| format!("cannot remove {}: {}", path.display(), error))?;
    }
    fs::rename(temporary, path)
        .map_err(|error| format!("cannot replace {}: {}", path.display(), error))
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temporary, path)
        .map_err(|error| format!("cannot replace {}: {}", path.display(), error))
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {}", path.display(), error)),
    }
}

fn existing_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o644)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0o644
    }
}

fn ensure_line(path: &Path, line: &str) -> Result<(), String> {
    let existing = read_text(path)?;
    if let Some(text) = &existing {
        if text.lines().any(|candidate| candidate == line) {
            return Ok(());
        }
    }
    let mut content = existing.unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(line);
    content.push('\n');
    write_file(path, &content, existing_mode(path))
}

fn remove_line(path: &Path, line: &str) -> Result<(), String> {
    let Some(text) = read_text(path)? else {
        return Ok(());
    };
    if !text.lines().any(|candidate| candidate == line) {
        return Ok(());
    }
    let kept: Vec<&str> = text
        .lines()
        .filter(|candidate| *candidate != line)
        .collect();
    let mut content = kept.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    write_file(path, &content, existing_mode(path))
}

fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {}", path.display(), error)),
    }
}

fn remove_dir(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {}", path.display(), error)),
    }
}

fn assert_file_contains(path: &Path, needle: &str) -> Result<(), String> {
    let content = read_text(path)?
        .ok_or_else(|| format!("{} does not exist after the write", path.display()))?;
    if content.contains(needle) {
        Ok(())
    } else {
        Err(format!("{} does not contain {:?}", path.display(), needle))
    }
}

/// Does this step already accept "the init system says there is nothing here"?
///
/// A step that allows a non-zero exit code — `procd`'s stop answering 1, `sc`'s
/// 1060 "no such service" — means the same thing when the program is absent
/// entirely, which is the `--manual` case: no init script was ever written, so
/// `--uninstall` has nothing to talk to (#303). A strict step (only exit 0
/// accepted) still reports a missing program.
fn tolerates_absence(allow_codes: Option<&[i32]>) -> bool {
    match allow_codes {
        None => true,
        Some(codes) => codes.iter().any(|code| *code != 0),
    }
}

fn run_command(argv: &[String], allow_codes: Option<&[i32]>) -> Result<(), String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "empty command".to_string())?;
    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound && tolerates_absence(allow_codes) => {
            return Ok(());
        }
        Err(error) => return Err(format!("cannot run {}: {}", argv.join(" "), error)),
    };
    if output.status.success() {
        return Ok(());
    }
    if allow_codes.is_none() {
        return Ok(());
    }
    if let (Some(allowed), Some(code)) = (allow_codes, output.status.code()) {
        if allowed.contains(&code) {
            return Ok(());
        }
    }
    let status = match output.status.code() {
        Some(code) => format!("exit {}", code),
        None => "a signal".to_string(),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        Err(format!("`{}` failed with {}", argv.join(" "), status))
    } else {
        Err(format!(
            "`{}` failed with {}: {}",
            argv.join(" "),
            status,
            detail
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_support::TempDir;

    /// #303: `--uninstall` on a `--manual` host has no init script to stop.
    /// The step already tolerates the init system answering "not running", so a
    /// program that is not there at all is the same answer.
    #[test]
    fn a_tolerant_step_skips_a_program_that_is_not_there() {
        let absent = [
            "/nonexistent/svcd-init-script".to_string(),
            "stop".to_string(),
        ];
        assert!(
            run_command(&absent, Some(&[0, 1])).is_ok(),
            "an accepted non-zero exit code also accepts an absent program"
        );
        assert!(run_command(&absent, None).is_ok(), "best effort");
    }

    /// The other half of the rule: a step that only accepts exit 0 is asking
    /// for something that must work, so a missing program stays a failure.
    #[test]
    fn a_strict_step_still_reports_a_program_that_is_not_there() {
        let absent = [
            "/nonexistent/svcd-init-script".to_string(),
            "enable".to_string(),
        ];
        let error = run_command(&absent, Some(&[0])).expect_err("missing program");
        assert!(
            error.starts_with("cannot run /nonexistent/svcd-init-script enable"),
            "{error}"
        );
    }

    #[test]
    fn file_operations_are_idempotent_and_remove_is_optional() {
        let directory = TempDir::new();
        let inittab = directory.0.join("inittab");
        let line = "::respawn:/usr/sbin/svcd run --foreground";

        let install = InstallPlan {
            target: Target::Busybox,
            action: Action::Install,
            ops: vec![
                Op::EnsureLine {
                    path: inittab.clone(),
                    line: line.into(),
                },
                Op::EnsureLine {
                    path: inittab.clone(),
                    line: line.into(),
                },
                Op::AssertFileContains {
                    path: inittab.clone(),
                    needle: line.into(),
                },
            ],
        };
        execute(&install).expect("install executes");
        execute(&install).expect("reinstall executes");
        assert_eq!(
            fs::read_to_string(&inittab).expect("read inittab"),
            format!("{}\n", line)
        );

        let uninstall = InstallPlan {
            target: Target::Busybox,
            action: Action::Uninstall,
            ops: vec![
                Op::RemoveLine {
                    path: inittab.clone(),
                    line: line.into(),
                },
                Op::RemoveDir {
                    path: directory.0.join("absent/data"),
                },
                Op::RemoveFile {
                    path: directory.0.join("absent/file"),
                },
            ],
        };
        execute(&uninstall).expect("uninstall executes");
        execute(&uninstall).expect("uninstall is idempotent");
        assert_eq!(fs::read_to_string(&inittab).expect("read inittab"), "");
    }

    #[test]
    fn write_file_replaces_atomically_and_keeps_no_temporary_files() {
        let directory = TempDir::new();
        let path = directory.0.join("unit");

        execute(&InstallPlan {
            target: Target::Systemd,
            action: Action::Install,
            ops: vec![Op::WriteFile {
                path: path.clone(),
                content: "one\n".into(),
                mode: 0o644,
            }],
        })
        .expect("first write");

        execute(&InstallPlan {
            target: Target::Systemd,
            action: Action::Install,
            ops: vec![Op::WriteFile {
                path: path.clone(),
                content: "two\n".into(),
                mode: 0o644,
            }],
        })
        .expect("second write");

        assert_eq!(fs::read_to_string(&path).expect("read unit"), "two\n");
        let leftovers: Vec<_> = fs::read_dir(&directory.0)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left: {:?}",
            leftovers
        );
    }

    #[test]
    fn a_failed_assertion_stops_the_plan() {
        let directory = TempDir::new();
        let path = directory.0.join("unit");
        let plan = InstallPlan {
            target: Target::Systemd,
            action: Action::Install,
            ops: vec![
                Op::WriteFile {
                    path: path.clone(),
                    content: "Type=simple\n".into(),
                    mode: 0o644,
                },
                Op::AssertFileContains {
                    path,
                    needle: "Type=notify".into(),
                },
            ],
        };

        let error = execute(&plan).expect_err("assertion fails");
        assert!(error.contains("Type=notify"), "{}", error);
    }

    #[test]
    fn store_trust_key_is_seeded_once() {
        let directory = TempDir::new();
        let key = directory.0.join("trust.d").join("signing.pub");
        assert_eq!(
            seed_store_trust(&directory.0).expect("seed"),
            Some(key.clone())
        );
        assert_eq!(
            fs::read_to_string(&key).expect("read key"),
            STORE_PUBLIC_KEY
        );
        assert_eq!(seed_store_trust(&directory.0).expect("second seed"), None);
    }

    #[test]
    fn an_existing_store_key_is_not_overwritten() {
        let directory = TempDir::new();
        seed_store_trust(&directory.0).expect("seed");
        let key = directory.0.join("trust.d").join("signing.pub");
        fs::write(&key, "operator key").expect("replace key");
        assert_eq!(seed_store_trust(&directory.0).expect("keep existing"), None);
        assert_eq!(fs::read_to_string(&key).expect("read key"), "operator key");
    }

    #[test]
    fn the_embedded_store_key_is_an_ed25519_spki() {
        crate::manage::validate_public_key(STORE_PUBLIC_KEY.as_bytes())
            .expect("the embedded store key must parse");
    }
}
