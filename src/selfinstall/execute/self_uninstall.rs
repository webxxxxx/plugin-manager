use super::plan_exec::usage_self;
use super::user_path::{remove_user_path, user_path_update_skipped};
use super::*;

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
    Err(last.unwrap_or_else(|| std::io::Error::other("cannot remove file")))
}

#[cfg(windows)]
pub(crate) fn wait_for_process(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const INFINITE: u32 = 0xFFFF_FFFF;
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if !handle.is_null() {
        // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
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
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
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

/// Take the install prefix back out of the machine PATH (#311).
///
/// A system install is `--manual`-less by construction (`--manual` is rejected
/// on uninstall), so there is no flag to consult: a prefix that was never added
/// makes this a no-op.
fn remove_install_path(root: &Path) {
    if super::system_path::skipped() {
        println!("path: skipped (test hook)");
        return;
    }
    #[cfg(windows)]
    match super::system_path::machine_key() {
        Some((hive, subkey)) => match super::system_path::remove_system_path(hive, &subkey, root) {
            Ok(true) => println!("path: removed {} from the system PATH", root.display()),
            Ok(false) => {}
            Err(error) => eprintln!("svcd: warning: could not update the system PATH: {}", error),
        },
        None => eprintln!(
            "svcd: warning: could not update the system PATH: SVCD_TEST_SYSTEM_PATH_KEY is malformed"
        ),
    }
    #[cfg(unix)]
    match super::system_path::remove_system_path(&super::system_path::base(), root) {
        Ok(files) if !files.is_empty() => {
            let names: Vec<String> = files
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            println!("path: removed from {}", names.join(", "));
        }
        Ok(_) => {}
        Err(error) => eprintln!("svcd: warning: could not update the system PATH: {}", error),
    }
}

fn self_uninstall_user(
    request: SelfRequest,
    installed: Option<&crate::platform::settings::Settings>,
    stop_daemon: &dyn Fn(),
) -> i32 {
    let runtime = installed
        .map(|settings| {
            crate::platform::paths::Paths::resolve_with(settings, |name| std::env::var_os(name))
        })
        .unwrap_or_else(|| ambient_paths(None));
    let install_root = request
        .prefix
        .clone()
        .unwrap_or_else(|| runtime.data_dir.join("bin"));
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
        for dir in [&runtime.config_dir, &runtime.data_dir, &runtime.logs_dir] {
            if dir.exists() {
                let _ = fs::remove_dir_all(dir);
                println!("removed {}", dir.display());
            }
        }
    }
    #[cfg(windows)]
    cleanup_current_uninstaller();
    EXIT_OK
}

pub fn self_uninstall(args: &[String], stop_daemon: &dyn Fn()) -> i32 {
    let mut request = match parse_self(args, true) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("svcd: {}", message);
            usage_self(true);
            return EXIT_USAGE;
        }
    };
    let installed = installed_conf_settings_for(request.prefix.as_deref());
    if request.prefix.is_none() {
        request.prefix = installed
            .as_ref()
            .and_then(|settings| settings.install_dir.clone());
    }
    // A Windows helper may run from the temp directory, so carry the prefix
    // explicitly instead of relying on program-directory discovery.
    #[cfg(windows)]
    let child_args = helper_args(args, request.prefix.as_deref());
    #[cfg(windows)]
    if crate::platform::paths::user_mode()
        && !request.dry_run
        && self_uninstall_current(&request)
        && !current_is_uninstaller(&request)
    {
        if let Some(code) = spawn_dedicated_uninstaller(&request, &child_args) {
            return code;
        }
        return spawn_uninstall_helper(&child_args);
    }
    if crate::platform::paths::user_mode() {
        return self_uninstall_user(request, installed.as_ref(), stop_daemon);
    }
    let install_root = request
        .prefix
        .clone()
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
        println!(
            "  path: remove {} from the system PATH",
            install_root.display()
        );
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

    remove_install_path(&install_root);
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
