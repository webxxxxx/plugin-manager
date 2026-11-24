use super::parse::{parse, Command};
use super::render::{file_name, print_status, usage, verify_failure, verify_host, verify_json};
use super::*;

pub fn run(args: Vec<String>) -> i32 {
    let command = match parse(&args) {
        Ok(command) => command,
        Err(()) => {
            usage();
            return EXIT_USAGE;
        }
    };
    // `plugin bundle` works on a directory and a key: it needs no runtime
    // directories, so a broken `svcd.conf` must not block it.
    let command = match command {
        Command::Bundle(options) => return bundle_command(options),
        other => other,
    };
    let Some(config) = Config::bootstrap_or_report("plugin") else {
        return 1;
    };
    match command {
        Command::List => list(&config),
        Command::Status { id, json } => status_command(&config, &id, json),
        Command::Start { id } => lifecycle(&config, "start", &id),
        Command::Stop { id } => lifecycle(&config, "stop", &id),
        Command::Restart { id } => lifecycle(&config, "restart", &id),
        Command::Enable { id } => lifecycle(&config, "enable", &id),
        Command::Disable { id } => lifecycle(&config, "disable", &id),
        Command::Install {
            path,
            strategy,
            allow_hooks,
            root,
        } => install(&config, &path, &strategy, allow_hooks, root.as_deref()),
        Command::Verify { path, json, pubkey } => verify(&config, &path, json, pubkey.as_deref()),
        Command::Uninstall { id, purge, force } => uninstall(&config, &id, purge, force),
        Command::Where { id, json } => where_is(&config, &id, json),
        Command::EditConfig {
            id,
            print,
            file,
            reset,
        } => edit_config(&config, &id, print, file.as_deref(), reset),
        Command::Export {
            id,
            config_only,
            out,
        } => export(&config, &id, config_only, out.as_deref()),
        Command::Import {
            path,
            config_id,
            no_config,
            force,
        } => import(&config, &path, config_id.as_deref(), no_config, force),
        Command::Bundle(_) => unreachable!("bundle is handled before bootstrap"),
    }
}

fn list(config: &Config) -> i32 {
    let response = match channel::request(config.paths.control_dir(), "svc.list", json!({})) {
        Ok(response) => response,
        Err(error) => return client::unreachable(&error, "plugin"),
    };
    if let Some(error) = response.error {
        eprintln!("plugin: {}", error.message);
        return EXIT_SERVICE_ERROR;
    }
    let result = match response.result {
        Some(result) => result,
        None => {
            eprintln!("plugin: empty list response");
            return EXIT_SERVICE_ERROR;
        }
    };
    let services = result.get("services").cloned().unwrap_or(Value::Null);
    let services: Vec<Status> = match serde_json::from_value(services) {
        Ok(services) => services,
        Err(error) => {
            eprintln!("plugin: invalid list response: {}", error);
            return EXIT_SERVICE_ERROR;
        }
    };
    let plugins: Vec<Status> = services
        .into_iter()
        .filter(|status| status.summary.kind == Kind::Plugin)
        .collect();
    println!("{}", status::table(&plugins));
    EXIT_OK
}

fn status_command(config: &Config, id: &str, json: bool) -> i32 {
    let status = match fetch_status(config, id, true) {
        Ok(status) => status,
        Err(code) => return code,
    };
    if status.summary.kind != Kind::Plugin {
        eprintln!("plugin: {} is not a plugin", id);
        return EXIT_SERVICE_ERROR;
    }
    print_status(status, json)
}

fn lifecycle(config: &Config, verb: &str, id: &str) -> i32 {
    let status = match fetch_status(config, id, false) {
        Ok(status) => status,
        Err(code) => return code,
    };
    if status.summary.kind != Kind::Plugin {
        eprintln!("plugin: {} is not a plugin", id);
        return EXIT_SERVICE_ERROR;
    }
    let method = format!("svc.{}", verb);
    match request(config, &method, json!({ "id": id })) {
        Ok(status) => print_status(status, false),
        Err(code) => code,
    }
}

fn install(
    config: &Config,
    path: &PathBuf,
    strategy: &str,
    allow_hooks: bool,
    root: Option<&Path>,
) -> i32 {
    // The daemon runs `plugin::verify` before it touches the plugin tree; the
    // same function backs `plugin verify`. Checking here as well would read
    // trust.d and svcd.conf in the caller's environment instead of the
    // daemon's, so the client stays a thin channel.
    let path = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("plugin: cannot read {}: {}", path.display(), error);
            return EXIT_SERVICE_ERROR;
        }
    };
    match request(
        config,
        "plugin.install",
        json!({
            "path": path.to_string_lossy(),
            "strategy": strategy,
            "allowHooks": allow_hooks,
            "root": root.map(|root| root.to_string_lossy().into_owned()),
        }),
    ) {
        Ok(status) => print_status(status, false),
        Err(code) => code,
    }
}

fn verify(config: &Config, path: &Path, json: bool, pubkey: Option<&Path>) -> i32 {
    let file = file_name(path);
    if let Ok(metadata) = fs::metadata(path) {
        if metadata.len() > package::MAX_PACKAGE_BYTES {
            return verify_failure(
                &file,
                &format!(
                    "package exceeds {} MiB",
                    package::MAX_PACKAGE_BYTES / (1024 * 1024)
                ),
                json,
            );
        }
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return verify_failure(
                &file,
                &format!("cannot read {}: {}", path.display(), error),
                json,
            )
        }
    };
    let explicit_key = match pubkey {
        Some(key_path) => match fs::read(key_path) {
            Ok(pem) => Some(pem),
            Err(error) => {
                return verify_failure(
                    &file,
                    &format!("cannot read {}: {}", key_path.display(), error),
                    json,
                )
            }
        },
        None => None,
    };
    let inspection =
        match package::inspect_bytes_with(&bytes, explicit_key.as_deref(), &config.paths.trust_dir)
        {
            Ok(inspection) => inspection,
            Err(error) => return verify_failure(&file, error.detail(), json),
        };
    let host = verify_host(config, &inspection);
    if json {
        return match serde_json::to_string_pretty(&verify_json(&file, &bytes, &inspection, &host)) {
            Ok(text) => {
                println!("{}", text);
                EXIT_OK
            }
            Err(error) => {
                eprintln!("plugin: cannot encode verify result: {}", error);
                EXIT_SERVICE_ERROR
            }
        };
    }
    let manifest = &inspection.package.manifest;
    // Human output leads with the display name and keeps the id visible: the
    // id is what every later command takes (#260).
    println!("{} {}", manifest.name, manifest.version);
    println!("id: {}", manifest.id());
    println!(
        "platform: {} (host match: {})",
        manifest.platform,
        if host.matches_host { "yes" } else { "no" }
    );
    println!("files: {}", manifest.files.len());
    let key_id = inspection.signature.key_id.as_deref().unwrap_or("-");
    println!(
        "signature: {} (keyId: {}, trusted: {})",
        inspection.signature.state, key_id, inspection.signature.trusted
    );
    if let Some(device) = &host.device {
        println!("device: {}", device);
    }
    if let Some(interpreter) = &host.interpreter {
        println!("interpreter: {}", interpreter);
    }
    EXIT_OK
}

fn uninstall(config: &Config, id: &str, purge: bool, force: bool) -> i32 {
    match channel::request(
        config.paths.control_dir(),
        "plugin.uninstall",
        json!({ "id": id, "purge": purge, "force": force }),
    ) {
        Ok(response) => match response.error {
            Some(error) => {
                eprintln!("plugin: {}", error.message);
                EXIT_SERVICE_ERROR
            }
            None => {
                println!("plugin {} uninstalled", id);
                EXIT_OK
            }
        },
        Err(error) => client::unreachable(&error, "plugin"),
    }
}

/// `plugin where <id>`: the root the index recorded and whether that volume is
/// there right now. Read from this process's own paths, not the control
/// channel: the usual reason to ask is that the volume (or the daemon) is down.
fn where_is(config: &Config, id: &str, json: bool) -> i32 {
    if !crate::kernel::registry::valid_id(id) {
        eprintln!("plugin: invalid plugin id: {}", id);
        return EXIT_USAGE;
    }
    let Some(root) = crate::platform::locations::root_of(&config.paths, id) else {
        if json {
            println!("{}", json!({ "id": id, "installed": false }));
        } else {
            eprintln!("plugin: unknown plugin: {}", id);
        }
        return EXIT_SERVICE_ERROR;
    };
    let available = crate::platform::locations::available(&root);
    let version = plugin::current_version(&config.paths, id);
    if json {
        println!(
            "{}",
            json!({
                "id": id,
                "installed": true,
                "root": root.to_string_lossy(),
                "available": available,
                "version": version,
            })
        );
        return EXIT_OK;
    }
    println!("{}", id);
    println!("  root:      {}", root.display());
    println!("  available: {}", if available { "yes" } else { "no" });
    match version {
        Some(version) => println!("  version:   {}", version),
        None => println!("  version:   (unknown)"),
    }
    if !available {
        println!(
            "  the volume holding this root is not there; plug it back in, or `plugin uninstall --force {}` to forget the record",
            id
        );
        return EXIT_SERVICE_ERROR;
    }
    EXIT_OK
}

fn edit_config(config: &Config, id: &str, print: bool, file: Option<&Path>, reset: bool) -> i32 {
    if print {
        let Some(path) = plugin::active_config(&config.paths, id) else {
            eprintln!("plugin: {} has no config", id);
            return EXIT_SERVICE_ERROR;
        };
        return match fs::read(&path) {
            Ok(data) => {
                let _ = std::io::stdout().write_all(&data);
                EXIT_OK
            }
            Err(error) => {
                eprintln!("plugin: read {}: {}", path.display(), error);
                EXIT_SERVICE_ERROR
            }
        };
    }
    if let Err(error) = plugin::root_required() {
        eprintln!("plugin: {}", error);
        return EXIT_SERVICE_ERROR;
    }
    let _lock = match plugin::lock_config(&config.paths, id) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("plugin: {}", error);
            return EXIT_SERVICE_ERROR;
        }
    };

    let candidate = if reset {
        match plugin::config_template(&config.paths, id) {
            Some((data, _)) => data,
            None => {
                eprintln!("plugin: {} has no config template", id);
                return EXIT_SERVICE_ERROR;
            }
        }
    } else if let Some(file) = file {
        match read_config(file) {
            Ok(data) => data,
            Err(error) => {
                eprintln!("plugin: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
    } else {
        match edit_with_editor(config, id) {
            Ok(data) => data,
            Err(error) => {
                eprintln!("plugin: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
    };

    match plugin::replace_config(&config.paths, id, &candidate, true) {
        Ok(()) => {
            println!("config updated; restart the plugin to apply");
            EXIT_OK
        }
        Err(error) => {
            eprintln!("plugin: {}", error);
            EXIT_SERVICE_ERROR
        }
    }
}

fn read_config(path: &Path) -> Result<Vec<u8>, String> {
    let data = fs::read(path).map_err(|error| format!("read {}: {}", path.display(), error))?;
    if data.len() > 1024 * 1024 {
        return Err("config exceeds 1 MiB".into());
    }
    Ok(data)
}

fn edit_with_editor(config: &Config, id: &str) -> Result<Vec<u8>, String> {
    let initial = plugin::active_config(&config.paths, id)
        .map(|path| fs::read(&path))
        .transpose()
        .map_err(|error| format!("read config: {}", error))?
        .or_else(|| plugin::config_template(&config.paths, id).map(|(data, _)| data))
        .ok_or_else(|| format!("plugin {} has no config", id))?;
    let directory = crate::kernel::layout::plugin_dir(&config.paths, id);
    let temporary = directory.join(format!("config.edit.{}", std::process::id()));
    plugin::write_config(&temporary, &initial).map_err(|error| error.detail().to_string())?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let program = parts.next().ok_or_else(|| "EDITOR is empty".to_string())?;
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(&temporary)
        .status()
        .map_err(|error| format!("run editor {}: {}", program, error))?;
    if !status.success() {
        let _ = fs::remove_file(&temporary);
        return Err(format!("editor exited with {}", status));
    }
    let data = fs::read(&temporary).map_err(|error| format!("read editor result: {}", error))?;
    let _ = fs::remove_file(&temporary);
    if data.len() > 1024 * 1024 {
        return Err("config exceeds 1 MiB".into());
    }
    Ok(data)
}

fn export(config: &Config, id: &str, config_only: bool, out: Option<&Path>) -> i32 {
    let (version, data, extension) = if config_only {
        match plugin::export_config(&config.paths, id) {
            Ok((version, data)) => (version, data, "config"),
            Err(error) => {
                eprintln!("plugin: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
    } else {
        match plugin::export_spkb(&config.paths, id) {
            Ok((version, data)) => (version, data, "spkb"),
            Err(error) => {
                eprintln!("plugin: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
    };
    let path = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{}-{}.{}", id, version, extension)));
    if let Err(error) = write_private(&path, &data) {
        eprintln!("plugin: {}", error);
        return EXIT_SERVICE_ERROR;
    }
    println!("{}", path.display());
    EXIT_OK
}

fn import(
    config: &Config,
    path: &Path,
    config_id: Option<&str>,
    no_config: bool,
    force: bool,
) -> i32 {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("plugin: read {}: {}", path.display(), error);
            return EXIT_SERVICE_ERROR;
        }
    };
    match transfer::import(
        &config.paths,
        config.settings.allow_insecure_install,
        &data,
        config_id,
        no_config,
        force,
        true,
    ) {
        Ok(outcome) => {
            for warning in &outcome.warnings {
                eprintln!("plugin: warning: {}", warning);
            }
            if let Some(status) = outcome.status {
                match serde_json::from_value::<Status>(status) {
                    Ok(status) => print_status(status, false),
                    Err(error) => {
                        eprintln!("plugin: invalid status response: {}", error);
                        EXIT_SERVICE_ERROR
                    }
                }
            } else if let Some(id) = outcome.config {
                println!("config imported for {}; restart to apply", id);
                EXIT_OK
            } else {
                EXIT_OK
            }
        }
        Err(transfer::Error::Usage(message)) => {
            eprintln!("plugin: {}", message);
            EXIT_USAGE
        }
        Err(transfer::Error::Failed(message)) => {
            eprintln!("plugin: {}", message);
            EXIT_SERVICE_ERROR
        }
    }
}

fn bundle_command(options: crate::manage::ops::bundle::Options) -> i32 {
    match crate::manage::ops::bundle::bundle(&options) {
        Ok(path) => {
            println!("{}", path.display());
            EXIT_OK
        }
        Err(error) => {
            eprintln!("plugin: {}", error);
            EXIT_SERVICE_ERROR
        }
    }
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    fs::write(path, data).map_err(|error| format!("write {}: {}", path.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("chmod {}: {}", path.display(), error))?;
    }
    Ok(())
}

fn fetch_status(config: &Config, id: &str, probe: bool) -> Result<Status, i32> {
    // Match `svc status`: a plugin lifecycle `status` action runs only for the
    // status command, not as a side effect of start/stop.
    let mut params = json!({ "id": id });
    if probe {
        params["probe"] = json!(true);
    }
    request(config, "svc.status", params)
}

fn request(config: &Config, method: &str, params: Value) -> Result<Status, i32> {
    client::request(config, method, params, "plugin")
}
