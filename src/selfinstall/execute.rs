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
/// Precedence: the path passed at install/startup, then `<program dir>/svcd.conf`,
/// then `<config>/svcd.conf` (legacy), then a generated platform-aware default
/// written into the config directory (falling back to the program directory when
/// the config directory is not writable).
pub fn ensure_conf(
    explicit: Option<&Path>,
    paths: &crate::platform::paths::Paths,
) -> Result<PathBuf, String> {
    // The caller already resolved the directory set once; reusing it keeps the
    // daemon from parsing the same document a second time.
    let layout = Layout::from_paths(paths);
    let program_conf = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("svcd.conf")));
    let config_conf = layout.config_dir.join("svcd.conf");

    if let Some(path) = explicit {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        if path.is_file() {
            return Ok(path);
        }
        eprintln!(
            "svcd: config {} not found; falling back to the service default",
            path.display()
        );
    }
    if let Some(path) = &program_conf {
        if path.is_file() {
            return Ok(path.clone());
        }
    }
    if config_conf.is_file() {
        return Ok(config_conf);
    }

    let text = default_conf_text(&layout);
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
/// `svcd.conf` and this process's environment (#207: the resolver is pure, the
/// reads are here).
fn current_runtime_dirs(settings: Option<&crate::platform::settings::Settings>) -> RuntimeDirs {
    // Precedence differs from `ambient_paths`: the installer targets the clean
    // boot environment, so the target file's `install_dir` wins over an
    // ambient `SVCD_DATA_DIR`/`SVCD_LOG_DIR` (which still lose to an explicit
    // key in the file). `SVCD_CONFIG_DIR` keeps its usual priority.
    RuntimeDirs::resolve(
        settings,
        &ambient_paths(None),
        &RuntimeOverrides {
            config: std::env::var_os("SVCD_CONFIG_DIR").map(PathBuf::from),
            data: std::env::var_os("SVCD_DATA_DIR").map(PathBuf::from),
            logs: std::env::var_os("SVCD_LOG_DIR").map(PathBuf::from),
        },
    )
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

/// Parse the installed `svcd.conf`, if there is one, so `--uninstall` can find
/// an `install_dir` without a `--conf` argument (uninstall does not accept one).
fn installed_conf_settings() -> Option<crate::platform::settings::Settings> {
    let path = ambient_paths(None).config_dir.join("svcd.conf");
    let text = fs::read_to_string(&path).ok()?;
    crate::platform::settings::parse(&text, path.parent().unwrap_or(Path::new("."))).ok()
}

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

pub fn self_install(args: &[String]) -> i32 {
    let request = match parse_self(args, false) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("svcd: {}", message);
            usage_self(false);
            return EXIT_USAGE;
        }
    };
    if crate::platform::paths::user_mode() {
        return self_install_user(request);
    }
    let prepared = match &request.conf {
        Some(path) => {
            let text = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!("svcd: cannot read {}: {}", path.display(), error);
                    return EXIT_ERROR;
                }
            };
            let source = std::path::absolute(path).unwrap_or_else(|_| path.clone());
            let base = source
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| ambient_paths(None).config_dir);
            let settings = match crate::platform::settings::parse(&text, &base) {
                Ok(settings) => settings,
                Err(error) => {
                    eprintln!("svcd: {}: {}", path.display(), error);
                    return EXIT_ERROR;
                }
            };
            Some((text, settings, source))
        }
        None => None,
    };
    // Without `--conf`, reuse the installed config so a reinstall keeps the
    // recorded `install_dir` instead of falling back to /usr/sbin.
    let installed = if prepared.is_none() {
        installed_conf_settings()
    } else {
        None
    };
    let target_settings = prepared
        .as_ref()
        .map(|(_, settings, _)| settings)
        .or(installed.as_ref());
    let prefix_requested = request.prefix.is_some();
    let install_root = resolve_install_root(
        request.prefix.as_deref(),
        target_settings.and_then(|settings| settings.install_dir.as_deref()),
        default_install_prefix(),
    );
    let install_root = if install_root.is_absolute() {
        install_root
    } else {
        std::path::absolute(&install_root).unwrap_or(install_root)
    };
    // A CLI `--prefix` becomes the recorded `install_dir`, so later reinstalls,
    // runtime paths and `--uninstall` all agree with where the files went.
    let runtime_settings = match (target_settings, prefix_requested) {
        (None, false) => None,
        (settings, true) => {
            let mut settings = settings.cloned().unwrap_or_default();
            settings.install_dir = Some(install_root.clone());
            Some(settings)
        }
        (Some(settings), false) => Some(settings.clone()),
    };
    let runtime = current_runtime_dirs(runtime_settings.as_ref());
    let record_root = prefix_requested
        || (prepared.is_some()
            && target_settings
                .and_then(|settings| settings.install_dir.as_ref())
                .is_some());
    // One plan decides for both the dry run and the real run (#207).
    let plan = plan_self_install(SelfInstallInputs {
        install_root: &install_root,
        runtime,
        config_source: prepared.as_ref().map(|(_, _, source)| source.as_path()),
        manual: request.manual,
        no_service: request.no_service,
        record_install_dir: record_root,
        service_target: request.target.unwrap_or_else(|| detect(&Probe::current())),
    });
    let SelfInstallPlan {
        install_root: planned_root,
        target,
        aliases,
        runtime,
        record_install_dir: record_root,
        launcher,
        service,
        previous_service,
    } = plan;
    // The dry run and the real run read the plan's own values from here on.
    let install_root = planned_root;
    let register_service = service.is_some();
    let current = std::env::current_exe().unwrap_or_else(|_| target.clone());
    if request.dry_run {
        println!("install {} -> {}", current.display(), target.display());
        println!("  aliases: {}", alias_names().join(", "));
        println!("  config:  {}", runtime.config.display());
        println!("  data:    {}", runtime.data.display());
        println!("  logs:    {}", runtime.logs.display());
        if prepared.is_some() {
            println!(
                "  conf:    {} -> {}",
                request
                    .conf
                    .as_ref()
                    .expect("prepared config has a source")
                    .display(),
                runtime.conf().display()
            );
        }
        if record_root {
            println!("  install_dir: {}", install_root.display());
        }
        if let Some(launcher) = &launcher {
            println!("  launcher: {}", launcher.display());
            println!(
                "  manual:   no init service; start it yourself (e.g. ::respawn:{})",
                launcher.display()
            );
            println!("  export:   SVCD_DATA_DIR={}", runtime.data.display());
            println!("  export:   SVCD_LOG_DIR={}", runtime.logs.display());
            if let Some(target) = request.target {
                println!("  note:     --target {} ignored (--manual)", target);
            }
        }
        if register_service {
            println!("  service: program {}", target.display());
        } else if request.manual {
            println!("  service:  skipped (--manual)");
        }
        return EXIT_OK;
    }
    if !crate::platform::is_privileged() {
        eprintln!("svcd: --install requires root / Administrator");
        return EXIT_ERROR;
    }

    if let Err(error) = fs::create_dir_all(&install_root) {
        eprintln!("svcd: cannot create {}: {}", install_root.display(), error);
        return EXIT_ERROR;
    }
    for dir in [&runtime.config, &runtime.data, &runtime.logs] {
        if let Err(error) = fs::create_dir_all(dir) {
            eprintln!("svcd: cannot create {}: {}", dir.display(), error);
            return EXIT_ERROR;
        }
    }

    if let Some((text, _, _)) = &prepared {
        let path = runtime.conf();
        if let Err(error) = write_file(&path, text, 0o644) {
            eprintln!("svcd: cannot write {}: {}", path.display(), error);
            return EXIT_ERROR;
        }
    }

    if record_root {
        if !runtime.conf().is_file() {
            let layout = Layout {
                program: target.clone(),
                data_dir: runtime.data.clone(),
                config_dir: runtime.config.clone(),
                conf: None,
            };
            let text = default_conf_text(&layout);
            if let Err(error) = write_conf(&runtime.conf(), &text) {
                eprintln!("svcd: cannot write {}: {}", runtime.conf().display(), error);
                return EXIT_ERROR;
            }
        }
        if let Err(error) = record_install_dir(&runtime.conf(), &install_root) {
            eprintln!(
                "svcd: cannot update {}: {}",
                runtime.conf().display(),
                error
            );
            return EXIT_ERROR;
        }
    }

    if target.exists() && register_service {
        if let Some(previous) = &previous_service {
            if let Err(error) = execute(previous) {
                eprintln!("svcd: uninstall failed: {}", error);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    if current != target {
        if let Err(error) = copy_executable(&current, &target) {
            eprintln!("svcd: cannot install {}: {}", target.display(), error);
            return EXIT_ERROR;
        }
    }
    for alias in &aliases {
        let path = install_root.join(alias);
        if let Err(error) = install_alias(&target, &path) {
            eprintln!("svcd: cannot install {}: {}", path.display(), error);
            return EXIT_ERROR;
        }
    }

    if let Some(launcher) = &launcher {
        if let Err(error) = write_file(launcher, &daemon_launcher(&target, &runtime), 0o755) {
            eprintln!("svcd: cannot write {}: {}", launcher.display(), error);
            return EXIT_ERROR;
        }
    }

    if let Some(service) = &service {
        if let Err(error) = execute(service) {
            eprintln!("svcd: install failed: {}", error);
            eprintln!("svcd: files installed, but service registration failed");
            return EXIT_ERROR;
        }
    }

    println!("svcd installed: {}", target.display());
    println!("aliases: {}", alias_names().join(", "));
    println!("config:  {}", runtime.config.display());
    println!("data:    {}", runtime.data.display());
    println!("logs:    {}", runtime.logs.display());
    if record_root {
        println!("install_dir: {}", install_root.display());
    }
    if let Some(launcher) = &launcher {
        println!("launcher: {}", launcher.display());
        println!(
            "manual: no init service registered; add the launcher to your init or firmware yourself"
        );
        println!(
            "manual: start it directly, or export these variables before starting svcd by hand:"
        );
        println!("  SVCD_DATA_DIR={}", runtime.data.display());
        println!("  SVCD_LOG_DIR={}", runtime.logs.display());
    }
    if request.no_service {
        println!("service: not installed (--no-service)");
    } else if request.manual {
        println!("service: skipped (--manual)");
    }
    if request.manual {
        if let Some(target) = request.target {
            println!("note: --target {} ignored (--manual)", target);
        }
    }
    println!("try: svc list");
    EXIT_OK
}

/// Mode 1 installation: install into the current user's data directory and
/// never touch system service managers. The same `--prefix`, `--conf`,
/// `--manual`, `--no-service` and `--dry-run` flags are accepted.
#[cfg(windows)]
fn add_user_path(dir: &Path) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ,
    };

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let subkey = wide("Environment");
    let value_name = wide("Path");
    let mut key: HKEY = std::ptr::null_mut();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_READ | KEY_SET_VALUE,
            &mut key,
        )
    };
    if opened != 0 {
        return Err(format!("open HKCU\\Environment: code {}", opened));
    }
    let mut kind = 0u32;
    let mut size = 0u32;
    let queried = unsafe {
        RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    let mut current = Vec::new();
    if queried == 0 && size > 0 {
        current.resize(size as usize, 0u8);
        let mut read_kind = 0u32;
        let mut read_size = size;
        let read = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                &mut read_kind,
                current.as_mut_ptr(),
                &mut read_size,
            )
        };
        if read != 0 {
            unsafe { RegCloseKey(key) };
            return Err(format!("read HKCU\\Environment\\Path: code {}", read));
        }
        current.truncate(read_size as usize);
    }
    let existing = String::from_utf16_lossy(
        &current
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|unit| *unit != 0)
            .collect::<Vec<_>>(),
    );
    let directory = dir.display().to_string();
    let already = existing
        .split(';')
        .any(|entry| entry.trim().eq_ignore_ascii_case(&directory));
    if already {
        unsafe { RegCloseKey(key) };
        return Ok(false);
    }
    let updated = if existing.trim().is_empty() {
        directory
    } else {
        format!("{};{}", existing.trim_end_matches(';'), directory)
    };
    let encoded: Vec<u16> = updated.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes: Vec<u8> = encoded.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    let written = unsafe {
        RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_EXPAND_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if written != 0 {
        return Err(format!("write HKCU\\Environment\\Path: code {}", written));
    }
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
        };
        let parameter = wide("Environment");
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            parameter.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            1000,
            std::ptr::null_mut(),
        );
    }
    Ok(true)
}

#[cfg(windows)]
fn remove_user_path(dir: &Path) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ,
    };

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let subkey = wide("Environment");
    let value_name = wide("Path");
    let mut key: HKEY = std::ptr::null_mut();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_READ | KEY_SET_VALUE,
            &mut key,
        )
    };
    if opened != 0 {
        return Err(format!("open HKCU\\Environment: code {}", opened));
    }
    let mut kind = 0u32;
    let mut size = 0u32;
    let queried = unsafe {
        RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if queried != 0 || size == 0 {
        unsafe { RegCloseKey(key) };
        return Ok(false);
    }
    let mut current = vec![0u8; size as usize];
    let mut read_size = size;
    let read = unsafe {
        RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            current.as_mut_ptr(),
            &mut read_size,
        )
    };
    if read != 0 {
        unsafe { RegCloseKey(key) };
        return Err(format!("read HKCU\\Environment\\Path: code {}", read));
    }
    current.truncate(read_size as usize);
    let existing = String::from_utf16_lossy(
        &current
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|unit| *unit != 0)
            .collect::<Vec<_>>(),
    );
    let directory = dir.display().to_string();
    let entries: Vec<&str> = existing
        .split(';')
        .filter(|entry| !entry.trim().eq_ignore_ascii_case(&directory))
        .collect();
    let updated = entries.join(";");
    if updated == existing {
        unsafe { RegCloseKey(key) };
        return Ok(false);
    }
    let encoded: Vec<u16> = updated.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes: Vec<u8> = encoded.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    let written = unsafe {
        RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_EXPAND_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if written != 0 {
        return Err(format!("write HKCU\\Environment\\Path: code {}", written));
    }
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
        };
        let parameter = wide("Environment");
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            parameter.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            1000,
            std::ptr::null_mut(),
        );
    }
    Ok(true)
}

#[cfg(unix)]
fn add_user_path(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    let escaped = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    let block = format!(
        "# svcd user path\n\
         if [ -d \"{}\" ]; then\n\
           case \":$PATH:\" in\n\
             *\":{}\"*) ;;\n\
             *) export PATH=\"{}:$PATH\" ;;\n\
           esac\n\
         fi\n",
        escaped, escaped, escaped
    );
    let mut files = Vec::new();
    for name in [".profile", ".bashrc", ".zshrc"] {
        let path = home.join(name);
        if !path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {}", path.display(), error))?;
        if text.contains(&format!("# svcd user path\n")) && text.contains(&escaped) {
            files.push(path);
            continue;
        }
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|error| format!("open {}: {}", path.display(), error))?;
        use std::io::Write;
        file.write_all(b"\n")
            .and_then(|_| file.write_all(block.as_bytes()))
            .map_err(|error| format!("write {}: {}", path.display(), error))?;
        files.push(path);
    }
    if files.is_empty() {
        let path = home.join(".profile");
        fs::write(&path, format!("\n{}", block))
            .map_err(|error| format!("write {}: {}", path.display(), error))?;
        files.push(path);
    }
    Ok(files)
}

#[cfg(unix)]
fn remove_user_path(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    let escaped = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    let block = format!(
        "# svcd user path\n\
         if [ -d \"{}\" ]; then\n\
           case \":$PATH:\" in\n\
             *\":{}\"*) ;;\n\
             *) export PATH=\"{}:$PATH\" ;;\n\
           esac\n\
         fi\n",
        escaped, escaped, escaped
    );
    let mut changed = Vec::new();
    for name in [".profile", ".bashrc", ".zshrc"] {
        let path = home.join(name);
        if !path.is_file() {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        if !text.contains(&block) {
            continue;
        }
        let mut updated = text.replace(&format!("\n{}", block), "");
        updated = updated.replace(&block, "");
        if let Err(error) = fs::write(&path, updated) {
            return Err(format!("write {}: {}", path.display(), error));
        }
        changed.push(path);
    }
    Ok(changed)
}

/// Debug-build seam: a user-mode install edits the profile of whoever runs the
/// tests (HKCU PATH on Windows, shell profiles on Unix). Tests that only assert
/// the installed files set this to keep the host profile untouched.
#[cfg(debug_assertions)]
fn user_path_update_skipped() -> bool {
    std::env::var_os("SVCD_TEST_NO_USER_PATH").is_some()
}

#[cfg(not(debug_assertions))]
fn user_path_update_skipped() -> bool {
    false
}

fn self_install_user(request: SelfRequest) -> i32 {
    let ambient = ambient_paths(None);
    let runtime = RuntimeDirs {
        config: std::env::var_os("SVCD_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or(ambient.config_dir),
        data: std::env::var_os("SVCD_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or(ambient.data_dir),
        logs: std::env::var_os("SVCD_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or(ambient.logs_dir),
    };
    let install_root = request
        .prefix
        .clone()
        .unwrap_or_else(|| runtime.data.join("bin"));
    let install_root = if install_root.is_absolute() {
        install_root
    } else {
        std::path::absolute(&install_root).unwrap_or(install_root)
    };
    let target_name = if cfg!(windows) { "svcd.exe" } else { "svcd" };
    let target = install_root.join(target_name);
    let current = std::env::current_exe().unwrap_or_else(|_| target.clone());
    let launcher = request.manual.then(|| install_root.join(launcher_name()));

    let prepared = match &request.conf {
        Some(path) => match fs::read_to_string(path) {
            Ok(text) => {
                let base = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| runtime.config.clone());
                if let Err(error) = crate::platform::settings::parse(&text, &base) {
                    eprintln!("svcd: {}: {}", path.display(), error);
                    return EXIT_ERROR;
                }
                Some(text)
            }
            Err(error) => {
                eprintln!("svcd: cannot read {}: {}", path.display(), error);
                return EXIT_ERROR;
            }
        },
        None => None,
    };

    if request.dry_run {
        println!("user mode install (no system service)");
        println!("install {} -> {}", current.display(), target.display());
        println!("  aliases: {}", alias_names().join(", "));
        println!("  config:  {}", runtime.config.display());
        println!("  data:    {}", runtime.data.display());
        println!("  logs:    {}", runtime.logs.display());
        if let Some(path) = &request.conf {
            println!(
                "  conf:    {} -> {}",
                path.display(),
                runtime.conf().display()
            );
        }
        if let Some(launcher) = &launcher {
            println!("  launcher: {}", launcher.display());
        }
        #[cfg(windows)]
        println!(
            "  path:    add {} to HKCU\\Environment\\Path",
            install_root.display()
        );
        #[cfg(unix)]
        println!("  path:    add {} to shell profile", install_root.display());
        if request.no_service {
            println!("  service: not installed (--no-service)");
        } else {
            println!("  service: skipped (user mode; use svcd user start or systemd --user)");
        }
        if let Some(target) = request.target {
            println!("  note: --target {} ignored in user mode", target);
        }
        return EXIT_OK;
    }

    for dir in [&install_root, &runtime.config, &runtime.data, &runtime.logs] {
        if let Err(error) = fs::create_dir_all(dir) {
            eprintln!("svcd: cannot create {}: {}", dir.display(), error);
            if cfg!(windows) && error.kind() == std::io::ErrorKind::PermissionDenied {
                eprintln!(
                    "svcd: hint: if this binary runs from a low-integrity/sandboxed folder, copy it to a normal user directory such as %LOCALAPPDATA%\\Temp and rerun --install"
                );
            }
            return EXIT_ERROR;
        }
    }

    if let Some(text) = prepared {
        if let Err(error) = write_file(&runtime.conf(), &text, 0o644) {
            eprintln!("svcd: cannot write {}: {}", runtime.conf().display(), error);
            return EXIT_ERROR;
        }
    } else if runtime.conf().is_file() {
        // An existing conf is kept as-is; bind is a plain config key now.
    } else {
        let layout = Layout {
            program: target.clone(),
            data_dir: runtime.data.clone(),
            config_dir: runtime.config.clone(),
            conf: None,
        };
        let text = default_conf_text(&layout);
        if let Err(error) = write_conf(&runtime.conf(), &text) {
            eprintln!("svcd: cannot write {}: {}", runtime.conf().display(), error);
            return EXIT_ERROR;
        }
    }

    if current != target {
        if let Err(error) = copy_executable(&current, &target) {
            eprintln!("svcd: cannot install {}: {}", target.display(), error);
            return EXIT_ERROR;
        }
    }
    for alias in alias_names() {
        let path = install_root.join(alias);
        if let Err(error) = install_alias(&target, &path) {
            eprintln!("svcd: cannot install {}: {}", path.display(), error);
            return EXIT_ERROR;
        }
    }
    #[cfg(windows)]
    {
        let uninstaller = install_root.join("uninstall.exe");
        if current != uninstaller {
            if let Err(error) = copy_executable(&target, &uninstaller) {
                eprintln!("svcd: cannot install {}: {}", uninstaller.display(), error);
                return EXIT_ERROR;
            }
        }
    }
    #[cfg(windows)]
    {
        if user_path_update_skipped() {
            println!("path: skipped (test hook)");
        } else {
            match add_user_path(&install_root) {
                Ok(true) => println!("path: added {} to the user PATH", install_root.display()),
                Ok(false) => println!(
                    "path: {} is already in the user PATH",
                    install_root.display()
                ),
                Err(error) => eprintln!("svcd: could not update the user PATH: {}", error),
            }
        }
    }
    #[cfg(unix)]
    {
        if user_path_update_skipped() {
            println!("path: skipped (test hook)");
        } else {
            match add_user_path(&install_root) {
                Ok(files) => {
                    let names: Vec<String> = files
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect();
                    println!("path: updated {}", names.join(", "));
                }
                Err(error) => eprintln!("svcd: could not update the shell PATH: {}", error),
            }
        }
    }
    if let Some(launcher) = &launcher {
        if let Err(error) = write_file(launcher, &daemon_launcher(&target, &runtime), 0o755) {
            eprintln!("svcd: cannot write {}: {}", launcher.display(), error);
            return EXIT_ERROR;
        }
    }

    println!("svcd installed (user mode): {}", target.display());
    println!("aliases: {}", alias_names().join(", "));
    println!("config:  {}", runtime.config.display());
    println!("data:    {}", runtime.data.display());
    println!("logs:    {}", runtime.logs.display());
    if let Some(launcher) = &launcher {
        println!("launcher: {}", launcher.display());
    }
    if request.no_service {
        println!("service: not installed (--no-service)");
    } else {
        println!("service: skipped (user mode; use `svcd user start` or systemd --user)");
    }
    if let Some(target) = request.target {
        println!("note: --target {} ignored in user mode", target);
    }
    println!("try: {}", install_root.join(alias_names()[0]).display());
    EXIT_OK
}

fn remove_file_retry(path: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..20 {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => last = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(last
        .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "cannot remove file")))
}

#[cfg(windows)]
pub(crate) fn wait_for_process(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const INFINITE: u32 = 0xFFFF_FFFF;
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if !handle.is_null() {
        unsafe {
            WaitForSingleObject(handle, INFINITE);
            CloseHandle(handle);
        }
    } else {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[cfg(windows)]
fn self_uninstall_current(request: &SelfRequest) -> bool {
    if std::env::var_os("SVCD_UNINSTALL_HELPER").is_some() {
        return false;
    }
    let Some(current) = std::env::current_exe().ok() else {
        return false;
    };
    let data = std::env::var_os("SVCD_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ambient_paths(None).data_dir);
    let root = request.prefix.clone().unwrap_or_else(|| data.join("bin"));
    let current = current.to_string_lossy().to_ascii_lowercase();
    let root = root.to_string_lossy().to_ascii_lowercase();
    current.starts_with(&root)
}

#[cfg(windows)]
fn uninstaller_path(request: &SelfRequest) -> PathBuf {
    let data = std::env::var_os("SVCD_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ambient_paths(None).data_dir);
    request
        .prefix
        .clone()
        .unwrap_or_else(|| data.join("bin"))
        .join("uninstall.exe")
}

#[cfg(windows)]
fn current_is_uninstaller(request: &SelfRequest) -> bool {
    std::env::current_exe()
        .ok()
        .map(|path| {
            path.to_string_lossy()
                .eq_ignore_ascii_case(uninstaller_path(request).to_string_lossy().as_ref())
        })
        .unwrap_or(false)
}

#[cfg(windows)]
fn spawn_dedicated_uninstaller(request: &SelfRequest, args: &[String]) -> Option<i32> {
    let uninstaller = uninstaller_path(request);
    if !uninstaller.is_file() || current_is_uninstaller(request) {
        return None;
    }
    let mut command = std::process::Command::new(&uninstaller);
    command
        .arg("--uninstall")
        .args(args)
        .env("SVCD_UNINSTALL_HELPER", "1")
        .env("SVCD_UNINSTALL_PARENT", std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    match command.spawn() {
        Ok(_) => {
            println!(
                "uninstall continues in the background with {}",
                uninstaller.display()
            );
            Some(EXIT_OK)
        }
        Err(error) => {
            eprintln!("svcd: cannot start uninstaller: {}", error);
            Some(EXIT_ERROR)
        }
    }
}

#[cfg(windows)]
fn schedule_delete_on_reboot(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MoveFileExW(wide.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT);
    }
}

#[cfg(windows)]
fn cleanup_current_uninstaller() {
    if std::env::var_os("SVCD_UNINSTALL_HELPER").is_none() {
        return;
    }
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    // Best-effort immediate cleanup; if the process is still mapped, register
    // the file for deletion on the next reboot.
    schedule_delete_on_reboot(&current);
    let script = format!(
        "ping -n 2 127.0.0.1 >nul & del /f /q \"{}\"",
        current.display()
    );
    let mut command = std::process::Command::new("cmd");
    command
        .args(["/C", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0000_0008); // DETACHED_PROCESS
    let _ = command.spawn();
}

#[cfg(windows)]
fn spawn_uninstall_helper(args: &[String]) -> i32 {
    let current = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("svcd: current executable: {}", error);
            return EXIT_ERROR;
        }
    };
    let helper = std::env::temp_dir().join(format!("svcd-uninstall-{}.exe", std::process::id()));
    if let Err(error) = fs::copy(&current, &helper) {
        eprintln!("svcd: cannot create uninstall helper: {}", error);
        return EXIT_ERROR;
    }
    let mut command = std::process::Command::new(&helper);
    command
        .arg("--uninstall")
        .args(args)
        .env("SVCD_UNINSTALL_HELPER", "1")
        .env("SVCD_UNINSTALL_PARENT", std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    match command.spawn() {
        Ok(_) => {
            println!("uninstall continues in the background; the install directory will be removed after this process exits");
            EXIT_OK
        }
        Err(error) => {
            eprintln!("svcd: cannot start uninstall helper: {}", error);
            EXIT_ERROR
        }
    }
}

fn self_uninstall_user(request: SelfRequest, stop_daemon: &dyn Fn()) -> i32 {
    let ambient = ambient_paths(None);
    let data = std::env::var_os("SVCD_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or(ambient.data_dir.clone());
    let install_root = request.prefix.clone().unwrap_or_else(|| data.join("bin"));
    let target_name = if cfg!(windows) { "svcd.exe" } else { "svcd" };
    let target = install_root.join(target_name);
    if request.dry_run {
        println!("uninstall user mode: {}", target.display());
        if request.purge {
            println!("purge config/data/logs");
        }
        return EXIT_OK;
    }
    // The installed aliases can be hard links to the running daemon binary.
    // Stop the user daemon first so Windows can release the shared file. The
    // composition root supplies the callback: a self-install action returns
    // what it needs done instead of reaching up into the daemon itself (§6).
    stop_daemon();
    std::thread::sleep(std::time::Duration::from_millis(500));
    for name in std::iter::once(target_name).chain(alias_names()) {
        let path = install_root.join(name);
        if !path.exists() {
            continue;
        }
        match remove_file_retry(&path) {
            Ok(()) => println!("removed {}", path.display()),
            Err(error) => {
                eprintln!(
                    "svcd: warning: cannot remove {}: {} (close any running svc/plugin/mcp process)",
                    path.display(),
                    error
                );
            }
        }
    }
    #[cfg(windows)]
    {
        let uninstaller = uninstaller_path(&request);
        if uninstaller.is_file() && !current_is_uninstaller(&request) {
            match remove_file_retry(&uninstaller) {
                Ok(()) => println!("removed {}", uninstaller.display()),
                Err(error) => eprintln!(
                    "svcd: warning: cannot remove {}: {}",
                    uninstaller.display(),
                    error
                ),
            }
        }
    }
    #[cfg(windows)]
    {
        if user_path_update_skipped() {
            println!("path: skipped (test hook)");
        } else {
            match remove_user_path(&install_root) {
                Ok(true) => println!(
                    "path: removed {} from the user PATH",
                    install_root.display()
                ),
                Ok(false) => {}
                Err(error) => eprintln!("svcd: warning: could not update the user PATH: {}", error),
            }
        }
    }
    #[cfg(unix)]
    {
        if user_path_update_skipped() {
            println!("path: skipped (test hook)");
        } else {
            match remove_user_path(&install_root) {
                Ok(files) if !files.is_empty() => {
                    let names: Vec<String> = files
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect();
                    println!("path: removed from {}", names.join(", "));
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("svcd: warning: could not update the shell PATH: {}", error)
                }
            }
        }
    }
    let launcher = install_root.join(launcher_name());
    let _ = fs::remove_file(launcher);
    if request.purge {
        for dir in [
            std::env::var_os("SVCD_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or(ambient.config_dir),
            data,
            std::env::var_os("SVCD_LOG_DIR")
                .map(PathBuf::from)
                .unwrap_or(ambient.logs_dir),
        ] {
            if dir.exists() {
                let _ = fs::remove_dir_all(&dir);
                println!("removed {}", dir.display());
            }
        }
    }
    #[cfg(windows)]
    cleanup_current_uninstaller();
    EXIT_OK
}

pub fn self_uninstall(args: &[String], stop_daemon: &dyn Fn()) -> i32 {
    let request = match parse_self(args, true) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("svcd: {}", message);
            usage_self(true);
            return EXIT_USAGE;
        }
    };
    #[cfg(windows)]
    if crate::platform::paths::user_mode()
        && !request.dry_run
        && self_uninstall_current(&request)
        && !current_is_uninstaller(&request)
    {
        if let Some(code) = spawn_dedicated_uninstaller(&request, args) {
            return code;
        }
        return spawn_uninstall_helper(args);
    }
    if crate::platform::paths::user_mode() {
        return self_uninstall_user(request, stop_daemon);
    }
    let installed = installed_conf_settings();
    let install_root = request
        .prefix
        .clone()
        .or_else(|| {
            installed
                .as_ref()
                .and_then(|settings| settings.install_dir.clone())
        })
        .unwrap_or_else(default_install_prefix);
    let runtime = current_runtime_dirs(installed.as_ref());
    let target_name = if cfg!(windows) { "svcd.exe" } else { "svcd" };
    let target = install_root.join(target_name);
    let launcher = install_root.join(launcher_name());
    if request.dry_run {
        println!("uninstall {}", install_root.display());
        println!("  remove: {}", target.display());
        println!("  aliases: {}", alias_names().join(", "));
        println!("  launcher: {}", launcher.display());
        if !request.no_service {
            println!("  service: remove");
        }
        if request.purge {
            println!(
                "  purge: {}, {}, {}",
                runtime.config.display(),
                runtime.data.display(),
                runtime.logs.display()
            );
        }
        return EXIT_OK;
    }
    if !crate::platform::is_privileged() {
        eprintln!("svcd: --uninstall requires root / Administrator");
        return EXIT_ERROR;
    }

    if !request.no_service {
        let mut args = Vec::new();
        if let Some(target) = request.target {
            args.push("--target".to_string());
            args.push(target.as_str().to_string());
        }
        let _ = uninstall_service(&args);
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    let current = std::env::current_exe().ok();
    for path in std::iter::once(target.clone())
        .chain(
            alias_names()
                .into_iter()
                .map(|name| install_root.join(name)),
        )
        .chain(["svcd_daemon.sh", "svcd_daemon.bat"].map(|name| install_root.join(name)))
    {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                if current.as_ref() == Some(&path) {
                    println!(
                        "svcd: {} is running; remove it after exit: {}",
                        path.display(),
                        error
                    );
                } else {
                    eprintln!("svcd: cannot remove {}: {}", path.display(), error);
                }
            }
        }
    }

    if request.purge {
        for dir in [runtime.config, runtime.data, runtime.logs] {
            let _ = fs::remove_dir_all(dir);
        }
    }
    println!("svcd uninstalled from {}", install_root.display());
    if !request.purge {
        println!("config/data/logs kept; use --purge to remove");
    }
    EXIT_OK
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
        replace_file(&temporary, target)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error))
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn install_alias(source: &Path, target: &Path) -> io::Result<()> {
    // Windows cannot unlink the last hard link while the daemon is running.
    // Copies keep aliases removable independently of the running executable.
    #[cfg(windows)]
    {
        return copy_executable(source, target);
    }
    #[cfg(not(windows))]
    {
        if target.exists() {
            fs::remove_file(target)?;
        }
        match fs::hard_link(source, target) {
            Ok(()) => {}
            Err(_) => copy_executable(source, target)?,
        }
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }
}

fn usage_self(uninstall: bool) {
    if uninstall {
        eprintln!("usage: svcd --uninstall [--prefix <dir>] [--no-service] [--purge] [--dry-run]");
    } else {
        eprintln!(
            "usage: svcd --install [--prefix <dir>] [--conf|--config <file>] [--manual] [--no-service] [--target <init>] [--dry-run]"
        );
        eprintln!(
            "       --prefix is recorded as install_dir; --manual skips init registration and writes svcd_daemon.sh/.bat"
        );
    }
}

fn dispatch(action: Action, args: &[String]) -> i32 {
    let request = match parse_request(action, args) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("svcd: {}", message);
            usage(action);
            return EXIT_USAGE;
        }
    };

    let target = request.target.unwrap_or_else(|| detect(&Probe::current()));
    let mut layout = Layout::current();
    if let Some(program) = request.program {
        layout.program = program;
    }
    layout.conf = request.conf;
    let plan = match action {
        Action::Install => install_plan(target, &layout),
        Action::Uninstall => uninstall_plan(target, &layout, request.purge),
    };

    if request.dry_run {
        print!("{}", plan);
        return EXIT_OK;
    }

    if !crate::platform::is_privileged() {
        eprintln!("svcd: {} requires root / Administrator", action.as_str());
        return EXIT_ERROR;
    }

    match execute(&plan) {
        Ok(()) => {
            println!("svcd: {} {}", action.past(), target);
            EXIT_OK
        }
        Err(error) => {
            eprintln!("svcd: {} failed: {}", action.as_str(), error);
            EXIT_ERROR
        }
    }
}

fn usage(action: Action) {
    match action {
        Action::Install => eprintln!(
            "usage: svcd install-service [--target systemd|launchd|scm|busybox|procd] [--program <path>] [--conf <file>] [--dry-run]"
        ),
        Action::Uninstall => eprintln!(
            "usage: svcd uninstall-service [--target systemd|launchd|scm|busybox|procd] [--program <path>] [--purge] [--dry-run]"
        ),
    }
}

fn execute(plan: &InstallPlan) -> Result<(), String> {
    for op in &plan.ops {
        execute_op(op).map_err(|error| format!("{}: {}", describe(op), error))?;
    }
    Ok(())
}

fn describe(op: &Op) -> String {
    match op {
        Op::WriteFile { path, .. } => format!("write {}", path.display()),
        Op::EnsureLine { path, .. } => format!("ensure line in {}", path.display()),
        Op::RemoveLine { path, .. } => format!("remove line from {}", path.display()),
        Op::RemoveFile { path } => format!("remove file {}", path.display()),
        Op::RemoveDir { path } => format!("remove directory {}", path.display()),
        Op::Run { argv, .. } => format!("run {}", argv.join(" ")),
        Op::AssertFileContains { path, .. } => format!("assert {} contains", path.display()),
    }
}

fn execute_op(op: &Op) -> Result<(), String> {
    match op {
        Op::WriteFile {
            path,
            content,
            mode,
        } => write_file(path, content, *mode),
        Op::EnsureLine { path, line } => ensure_line(path, line),
        Op::RemoveLine { path, line } => remove_line(path, line),
        Op::RemoveFile { path } => remove_file(path),
        Op::RemoveDir { path } => remove_dir(path),
        Op::Run { argv, allow_codes } => run_command(argv, allow_codes.as_deref()),
        Op::AssertFileContains { path, needle } => assert_file_contains(path, needle),
    }
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

fn run_command(argv: &[String], allow_codes: Option<&[i32]>) -> Result<(), String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "empty command".to_string())?;
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run {}: {}", argv.join(" "), error))?;
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = crate::test_support::temp_dir().join(format!(
                "svcd-install-{}-{}-{}",
                std::process::id(),
                nanos,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
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
}
