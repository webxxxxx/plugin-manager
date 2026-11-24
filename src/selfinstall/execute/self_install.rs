use super::plan_exec::usage_self;
use super::user_path::{add_user_path, user_path_update_skipped};
use super::*;

/// Refuse an install that would wrap a document this binary cannot load.
///
/// The failure this replaces was silent: the files were upgraded, the unit was
/// enabled, systemd retried five times, and the operator only saw
/// "service registration failed" while the daemon's `invalid config` line sat
/// in the journal.
/// Reuse the installed document unless this binary cannot load it.
fn installed_conf_or_refuse() -> Result<Option<crate::platform::settings::Settings>, i32> {
    match installed_conf() {
        InstalledConf::Loaded(settings) => Ok(Some(*settings)),
        InstalledConf::Absent => Ok(None),
        InstalledConf::Invalid(message) => Err(refuse_invalid_conf(&message)),
    }
}

fn refuse_invalid_conf(message: &str) -> i32 {
    eprintln!("svcd: config {}", message);
    eprintln!(
        "svcd: refusing to install: the daemon exits 1 on this document; fix the key, or replace a pre-ADR-0010 `http_port`/`lan_default` pair with `bind = <ip>:<port>`"
    );
    eprintln!("svcd: rerun with --conf <file> to install a different document instead");
    EXIT_ERROR
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
        match installed_conf_or_refuse() {
            Ok(settings) => settings,
            Err(code) => return code,
        }
    } else {
        None
    };
    let mut settings = prepared
        .as_ref()
        .map(|(_, settings, _)| settings.clone())
        .or(installed)
        .unwrap_or_default();
    let prefix_requested = request.prefix.is_some();
    let install_root = resolve_install_root(
        request.prefix.as_deref(),
        settings.install_dir.as_deref(),
        default_install_prefix(),
    );
    let install_root = if install_root.is_absolute() {
        install_root
    } else {
        std::path::absolute(&install_root).unwrap_or(install_root)
    };
    // A CLI `--prefix` becomes the recorded `install_dir`, so later reinstalls,
    // runtime paths and `--uninstall` all agree with where the files went.
    if prefix_requested {
        settings.install_dir = Some(install_root.clone());
    }
    // The roots are filled before the resolver runs; `install_dir` is folded
    // here and nowhere else. A source document with explicit roots keeps them.
    crate::platform::config_file::fill_missing(&mut settings);
    let runtime = current_runtime_dirs(Some(&settings));
    // The document records the roots the installed daemon will resolve, not a
    // pre-override guess. This also makes `SVCD_CONFIG_DIR` part of install's
    // own answer instead of a value that disappears at the next boot.
    settings.config_dir = Some(runtime.config.clone());
    settings.data_dir = Some(runtime.data.clone());
    settings.logs_dir = Some(runtime.logs.clone());
    let record_root = prefix_requested || (prepared.is_some() && settings.install_dir.is_some());
    // The service and the launcher must use the installed document, not the
    // --conf source it was copied from: the source may be temporary.
    let installed_conf = runtime.conf();
    // One plan decides for both the dry run and the real run (#207).
    let plan = plan_self_install(SelfInstallInputs {
        install_root: &install_root,
        runtime,
        config_path: Some(installed_conf.as_path()),
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
        println!("  trust:   {}", runtime.config.join("trust.d").display());
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
    for dir in [
        &runtime.config,
        &runtime.data,
        &runtime.logs,
        &runtime.data.join("run"),
        &runtime.data.join("temp"),
    ] {
        if let Err(error) = fs::create_dir_all(dir) {
            eprintln!("svcd: cannot create {}: {}", dir.display(), error);
            return EXIT_ERROR;
        }
    }

    let conf_path = runtime.conf();
    let had_conf = conf_path.is_file();
    if let Some((text, _, _)) = &prepared {
        if let Err(error) = write_file(&conf_path, text, 0o644) {
            eprintln!("svcd: cannot write {}: {}", conf_path.display(), error);
            return EXIT_ERROR;
        }
    } else if !had_conf {
        // No source and no installed document: install owns the first one.
        let text = crate::platform::config_file::generated(&settings);
        if let Err(error) = write_conf(&conf_path, &text) {
            eprintln!("svcd: cannot write {}: {}", conf_path.display(), error);
            return EXIT_ERROR;
        }
    }

    if record_root {
        if let Err(error) = record_install_dir(&conf_path, &install_root) {
            eprintln!("svcd: cannot update {}: {}", conf_path.display(), error);
            return EXIT_ERROR;
        }
    }
    if prepared.is_some() || had_conf {
        let missing = crate::platform::config_file::missing_roots(&conf_path);
        for warning in crate::platform::config_file::update_filled(&conf_path, &settings, &missing)
        {
            eprintln!("svcd: warning: {}", warning);
        }
    }

    match seed_store_trust(&runtime.config) {
        Ok(Some(path)) => println!("  store key: {}", path.display()),
        Ok(None) => {}
        Err(error) => {
            eprintln!("svcd: cannot seed store trust root: {}", error);
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

fn self_install_user(request: SelfRequest) -> i32 {
    let prepared = match &request.conf {
        Some(path) => match fs::read_to_string(path) {
            Ok(text) => {
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
                Some((text, settings))
            }
            Err(error) => {
                eprintln!("svcd: cannot read {}: {}", path.display(), error);
                return EXIT_ERROR;
            }
        },
        None => None,
    };
    let installed = if prepared.is_none() {
        match installed_conf_or_refuse() {
            Ok(settings) => settings,
            Err(code) => return code,
        }
    } else {
        None
    };
    let mut settings = prepared
        .as_ref()
        .map(|(_, settings)| settings.clone())
        .or(installed)
        .unwrap_or_default();
    let prefix = request.prefix.as_ref().map(|prefix| {
        if prefix.is_absolute() {
            prefix.clone()
        } else {
            std::path::absolute(prefix).unwrap_or_else(|_| prefix.clone())
        }
    });
    if let Some(prefix) = &prefix {
        settings.install_dir = Some(prefix.clone());
    }
    crate::platform::config_file::fill_missing(&mut settings);
    let paths =
        crate::platform::paths::Paths::resolve_with(&settings, |name| std::env::var_os(name));
    let runtime = RuntimeDirs::from_paths(&paths);
    // The document records the roots this user's daemon will resolve.
    settings.config_dir = Some(runtime.config.clone());
    settings.data_dir = Some(runtime.data.clone());
    settings.logs_dir = Some(runtime.logs.clone());
    let install_root = prefix
        .or_else(|| settings.install_dir.clone())
        .unwrap_or_else(|| runtime.data.join("bin"));
    let target_name = if cfg!(windows) { "svcd.exe" } else { "svcd" };
    let target = install_root.join(target_name);
    let current = std::env::current_exe().unwrap_or_else(|_| target.clone());
    let launcher = request.manual.then(|| install_root.join(launcher_name()));

    if request.dry_run {
        println!("user mode install (no system service)");
        println!("install {} -> {}", current.display(), target.display());
        println!("  aliases: {}", alias_names().join(", "));
        println!("  config:  {}", runtime.config.display());
        println!("  trust:   {}", runtime.config.join("trust.d").display());
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

    for dir in [
        &install_root,
        &runtime.config,
        &runtime.data,
        &runtime.logs,
        &runtime.data.join("run"),
        &runtime.data.join("temp"),
    ] {
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

    let conf_path = runtime.conf();
    let had_conf = conf_path.is_file();
    if let Some((text, _)) = &prepared {
        if let Err(error) = write_file(&conf_path, text, 0o644) {
            eprintln!("svcd: cannot write {}: {}", conf_path.display(), error);
            return EXIT_ERROR;
        }
    } else if !had_conf {
        let text = crate::platform::config_file::generated(&settings);
        if let Err(error) = write_conf(&conf_path, &text) {
            eprintln!("svcd: cannot write {}: {}", conf_path.display(), error);
            return EXIT_ERROR;
        }
    }
    if request.prefix.is_some() {
        if let Err(error) = record_install_dir(&conf_path, &install_root) {
            eprintln!("svcd: cannot update {}: {}", conf_path.display(), error);
            return EXIT_ERROR;
        }
    }
    if prepared.is_some() || had_conf {
        let missing = crate::platform::config_file::missing_roots(&conf_path);
        for warning in crate::platform::config_file::update_filled(&conf_path, &settings, &missing)
        {
            eprintln!("svcd: warning: {}", warning);
        }
    }

    match seed_store_trust(&runtime.config) {
        Ok(Some(path)) => println!("  store key: {}", path.display()),
        Ok(None) => {}
        Err(error) => {
            eprintln!("svcd: cannot seed store trust root: {}", error);
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
