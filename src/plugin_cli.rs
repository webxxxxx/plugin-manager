//! The `plugin` entry: plugin-only views over the same daemon channel.
//!
//! `svc` stays the generic service CLI; `plugin` refuses core ids for
//! status/start/stop and adds install/uninstall.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::channel;
use crate::compat;
use crate::config::Config;
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::package;
use crate::plugin;
use crate::registry::Kind;
use crate::status::{self, Status};
use crate::transfer;

const EXIT_OK: i32 = 0;
const EXIT_SERVICE_ERROR: i32 = 1;
const EXIT_DAEMON_UNREACHABLE: i32 = 2;
const EXIT_USAGE: i32 = 64;

#[derive(Debug, PartialEq, Eq)]
enum Command {
    List,
    Status {
        id: String,
        json: bool,
    },
    Start {
        id: String,
    },
    Stop {
        id: String,
    },
    Restart {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Install {
        path: PathBuf,
        strategy: String,
        allow_hooks: bool,
    },
    Verify {
        path: PathBuf,
        json: bool,
        /// Check the signature against this PEM even when the key is not in
        /// `trust.d`. CI has no trust root; this is how it checks the release
        /// signature without pretending the key is trusted.
        pubkey: Option<PathBuf>,
    },
    Uninstall {
        id: String,
        purge: bool,
    },
    EditConfig {
        id: String,
        print: bool,
        file: Option<PathBuf>,
        reset: bool,
    },
    Export {
        id: String,
        config_only: bool,
        out: Option<PathBuf>,
    },
    Import {
        path: PathBuf,
        config_id: Option<String>,
        no_config: bool,
        force: bool,
    },
    Bundle(crate::bundle::Options),
}

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
        } => install(&config, &path, &strategy, allow_hooks),
        Command::Verify { path, json, pubkey } => verify(&config, &path, json, pubkey.as_deref()),
        Command::Uninstall { id, purge } => uninstall(&config, &id, purge),
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

fn parse(args: &[String]) -> Result<Command, ()> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(());
    };
    match command {
        "list" if args.len() == 1 => Ok(Command::List),
        "status" => {
            let mut id = None;
            let mut json = false;
            for arg in &args[1..] {
                if arg == "--json" {
                    json = true;
                } else if arg.starts_with('-') || id.is_some() {
                    return Err(());
                } else {
                    id = Some(arg.clone());
                }
            }
            id.map(|id| Command::Status { id, json }).ok_or(())
        }
        "start" | "stop" | "restart" | "enable" | "disable" => {
            if args.len() != 2 || args[1].starts_with('-') {
                return Err(());
            }
            let id = args[1].clone();
            match command {
                "start" => Ok(Command::Start { id }),
                "stop" => Ok(Command::Stop { id }),
                "restart" => Ok(Command::Restart { id }),
                "enable" => Ok(Command::Enable { id }),
                _ => Ok(Command::Disable { id }),
            }
        }
        "install" => {
            let mut path = None;
            let mut strategy = "replace".to_string();
            let mut allow_hooks = false;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--strategy" {
                    index += 1;
                    strategy = args.get(index).ok_or(())?.clone();
                } else if let Some(value) = arg.strip_prefix("--strategy=") {
                    strategy = value.to_string();
                } else if arg == "--allow-hooks" {
                    allow_hooks = true;
                } else if arg.starts_with('-') || path.is_some() {
                    return Err(());
                } else {
                    path = Some(PathBuf::from(arg));
                }
                index += 1;
            }
            if strategy != "keep-old" && strategy != "replace" {
                return Err(());
            }
            path.map(|path| Command::Install {
                path,
                strategy,
                allow_hooks,
            })
            .ok_or(())
        }
        "verify" => {
            let mut path = None;
            let mut json = false;
            let mut pubkey = None;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--json" {
                    json = true;
                } else if arg == "--pubkey" {
                    index += 1;
                    pubkey = Some(PathBuf::from(args.get(index).ok_or(())?));
                } else if let Some(value) = arg.strip_prefix("--pubkey=") {
                    pubkey = Some(PathBuf::from(value));
                } else if arg.starts_with('-') || path.is_some() {
                    return Err(());
                } else {
                    path = Some(PathBuf::from(arg));
                }
                index += 1;
            }
            path.map(|path| Command::Verify { path, json, pubkey })
                .ok_or(())
        }
        "uninstall" => {
            let mut id = None;
            let mut purge = false;
            for arg in &args[1..] {
                if arg == "--purge" {
                    purge = true;
                } else if arg.starts_with('-') || id.is_some() {
                    return Err(());
                } else {
                    id = Some(arg.clone());
                }
            }
            id.map(|id| Command::Uninstall { id, purge }).ok_or(())
        }
        "editconfig" => {
            let mut id = None;
            let mut print = false;
            let mut file = None;
            let mut reset = false;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--print" {
                    print = true;
                } else if arg == "--reset" {
                    reset = true;
                } else if arg == "--file" {
                    index += 1;
                    file = Some(PathBuf::from(args.get(index).ok_or(())?));
                } else if arg.starts_with('-') || id.is_some() {
                    return Err(());
                } else {
                    id = Some(arg.clone());
                }
                index += 1;
            }
            if print && (file.is_some() || reset) || file.is_some() && reset {
                return Err(());
            }
            id.map(|id| Command::EditConfig {
                id,
                print,
                file,
                reset,
            })
            .ok_or(())
        }
        "export" => {
            let mut id = None;
            let mut config_only = false;
            let mut out = None;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--config-only" {
                    config_only = true;
                } else if arg == "--out" {
                    index += 1;
                    out = Some(PathBuf::from(args.get(index).ok_or(())?));
                } else if let Some(value) = arg.strip_prefix("--out=") {
                    out = Some(PathBuf::from(value));
                } else if arg.starts_with('-') || id.is_some() {
                    return Err(());
                } else {
                    id = Some(arg.clone());
                }
                index += 1;
            }
            id.map(|id| Command::Export {
                id,
                config_only,
                out,
            })
            .ok_or(())
        }
        "import" => {
            let mut path = None;
            let mut config_id = None;
            let mut no_config = false;
            let mut force = false;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--no-config" {
                    no_config = true;
                } else if arg == "--force" {
                    force = true;
                } else if arg == "--config" {
                    index += 1;
                    config_id = Some(args.get(index).ok_or(())?.clone());
                } else if let Some(value) = arg.strip_prefix("--config=") {
                    config_id = Some(value.to_string());
                } else if arg.starts_with('-') || path.is_some() {
                    return Err(());
                } else {
                    path = Some(PathBuf::from(arg));
                }
                index += 1;
            }
            path.map(|path| Command::Import {
                path,
                config_id,
                no_config,
                force,
            })
            .ok_or(())
        }
        "bundle" => {
            let mut dir = None;
            let mut out = None;
            let mut key = None;
            let mut key_id = None;
            let mut id = None;
            let mut version = None;
            let mut exec = None;
            let mut desc = None;
            let mut from_core = false;
            let mut interpreter = None;
            let mut interpreter_version = None;
            let mut interpreter_args = Vec::new();
            let mut compat = Vec::new();
            let mut platform = None;
            let mut unsigned = false;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                match arg.as_str() {
                    "-o" | "--out" => {
                        index += 1;
                        out = Some(PathBuf::from(args.get(index).ok_or(())?));
                    }
                    "--key" => {
                        index += 1;
                        key = Some(PathBuf::from(args.get(index).ok_or(())?));
                    }
                    "--unsigned" => unsigned = true,
                    "--key-id" => {
                        index += 1;
                        key_id = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--id" => {
                        index += 1;
                        id = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--version" => {
                        index += 1;
                        version = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--exec" => {
                        index += 1;
                        exec = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--desc" => {
                        index += 1;
                        desc = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--from-core" => from_core = true,
                    "--interpreter" => {
                        index += 1;
                        interpreter = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--interpreter-version" => {
                        index += 1;
                        interpreter_version = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--interpreter-arg" => {
                        index += 1;
                        interpreter_args.push(args.get(index).ok_or(())?.clone());
                    }
                    "--compat" => {
                        index += 1;
                        compat.extend(
                            args.get(index)
                                .ok_or(())?
                                .split(',')
                                .filter(|value| !value.is_empty())
                                .map(str::to_string),
                        );
                    }
                    "--platform" => {
                        index += 1;
                        platform = Some(args.get(index).ok_or(())?.clone());
                    }
                    _ if arg.starts_with("--out=") => {
                        out = Some(PathBuf::from(
                            arg.strip_prefix("--out=").expect("matched prefix"),
                        ));
                    }
                    _ if arg.starts_with("--key-id=") => {
                        key_id = Some(
                            arg.strip_prefix("--key-id=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--key=") => {
                        key = Some(PathBuf::from(
                            arg.strip_prefix("--key=").expect("matched prefix"),
                        ));
                    }
                    _ if arg.starts_with("--id=") => {
                        id = Some(
                            arg.strip_prefix("--id=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--version=") => {
                        version = Some(
                            arg.strip_prefix("--version=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--exec=") => {
                        exec = Some(
                            arg.strip_prefix("--exec=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--desc=") => {
                        desc = Some(
                            arg.strip_prefix("--desc=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--interpreter=") => {
                        interpreter = Some(
                            arg.strip_prefix("--interpreter=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--interpreter-version=") => {
                        interpreter_version = Some(
                            arg.strip_prefix("--interpreter-version=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--interpreter-arg=") => {
                        interpreter_args.push(
                            arg.strip_prefix("--interpreter-arg=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--compat=") => {
                        compat.extend(
                            arg.strip_prefix("--compat=")
                                .expect("matched prefix")
                                .split(',')
                                .filter(|value| !value.is_empty())
                                .map(str::to_string),
                        );
                    }
                    _ if arg.starts_with("--platform=") => {
                        platform = Some(
                            arg.strip_prefix("--platform=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with('-') => return Err(()),
                    _ if dir.is_none() => dir = Some(PathBuf::from(arg)),
                    _ => return Err(()),
                }
                index += 1;
            }
            dir.map(|dir| {
                Command::Bundle(crate::bundle::Options {
                    dir,
                    out,
                    key,
                    key_id,
                    id,
                    version,
                    exec,
                    desc,
                    from_core,
                    interpreter,
                    interpreter_version,
                    interpreter_args,
                    compat,
                    platform,
                    unsigned,
                })
            })
            .ok_or(())
        }
        _ => Err(()),
    }
}

fn list(config: &Config) -> i32 {
    let response = match channel::request(&config.paths.config_dir, "svc.list", json!({})) {
        Ok(response) => response,
        Err(error) => return unreachable(&error),
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

fn install(config: &Config, path: &PathBuf, strategy: &str, allow_hooks: bool) -> i32 {
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
    println!("{} {}", manifest.name, manifest.version);
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

struct VerifyHost {
    matches_host: bool,
    device: Option<String>,
    interpreter: Option<String>,
}

fn verify_host(config: &Config, inspection: &package::Inspection) -> VerifyHost {
    let host = compat::detect(&config.paths, MAX_SCHEMA, MAX_PROTOCOL);
    VerifyHost {
        matches_host: inspection.package.manifest.platform == host.platform,
        device: host.device,
        interpreter: crate::plugin::prepare(&inspection.package)
            .ok()
            .flatten()
            .map(|interpreter| interpreter.path),
    }
}

fn verify_failure(file: &str, error: &str, json: bool) -> i32 {
    if json {
        let value = json!({ "ok": false, "file": file, "error": error });
        match serde_json::to_string_pretty(&value) {
            Ok(text) => println!("{}", text),
            Err(_) => eprintln!("plugin: {}", error),
        }
    } else {
        eprintln!("plugin: {}", error);
    }
    EXIT_SERVICE_ERROR
}

fn verify_json(
    file: &str,
    bytes: &[u8],
    inspection: &package::Inspection,
    host: &VerifyHost,
) -> Value {
    let manifest = &inspection.package.manifest;
    let mut signature = json!({
        "state": inspection.signature.state,
        "trusted": inspection.signature.trusted,
    });
    if let Some(key_id) = &inspection.signature.key_id {
        signature["keyId"] = Value::String(key_id.clone());
    }
    json!({
        "ok": true,
        "file": file,
        "name": manifest.name,
        "version": manifest.version,
        "platform": manifest.platform,
        "schema": manifest.schema,
        "size": bytes.len(),
        "sha256": package::hex_sha256(bytes),
        "capabilities": manifest
            .capabilities
            .iter()
            .map(|capability| capability.name.clone())
            .collect::<Vec<_>>(),
        "compat": match &manifest.compat {
            Some(spec) => json!(spec.devices),
            None => Value::Null,
        },
        "signature": signature,
        "host": {
            "matchesHost": host.matches_host,
            "device": host.device,
            "interpreter": host.interpreter,
        },
        "files": manifest.files.len(),
    })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn uninstall(config: &Config, id: &str, purge: bool) -> i32 {
    match channel::request(
        &config.paths.config_dir,
        "plugin.uninstall",
        json!({ "id": id, "purge": purge }),
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
        Err(error) => unreachable(&error),
    }
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
    let directory = plugin::plugin_dir(&config.paths, id);
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

fn bundle_command(options: crate::bundle::Options) -> i32 {
    match crate::bundle::bundle(&options) {
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
    let response = match channel::request(&config.paths.config_dir, method, params) {
        Ok(response) => response,
        Err(error) => return Err(unreachable(&error)),
    };
    if let Some(error) = response.error {
        eprintln!("plugin: {}", error.message);
        return Err(EXIT_SERVICE_ERROR);
    }
    let result = match response.result {
        Some(result) => result,
        None => {
            eprintln!("plugin: empty response");
            return Err(EXIT_SERVICE_ERROR);
        }
    };
    serde_json::from_value(result).map_err(|error| {
        eprintln!("plugin: invalid status response: {}", error);
        EXIT_SERVICE_ERROR
    })
}

fn print_status(status: Status, json: bool) -> i32 {
    if json {
        match serde_json::to_string_pretty(&status) {
            Ok(text) => println!("{}", text),
            Err(error) => {
                eprintln!("plugin: cannot encode status: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
    } else {
        println!("{}", status::text(&status));
    }
    EXIT_OK
}

fn unreachable(error: &std::io::Error) -> i32 {
    eprintln!("plugin: svcd unreachable: {}", error);
    EXIT_DAEMON_UNREACHABLE
}

fn usage() {
    eprintln!("usage: plugin list");
    eprintln!("       plugin status [--json] <id>");
    eprintln!("       plugin start|stop|restart <id>");
    eprintln!("       plugin enable|disable <id>");
    eprintln!("       plugin install <file.spk> [--strategy keep-old|replace] [--allow-hooks] (default: replace)");
    eprintln!("       plugin verify <file.spk> [--json] [--pubkey <pem>]");
    eprintln!("       plugin uninstall <id> [--purge]");
    eprintln!("       plugin editconfig <id> [--print|--file <path>|--reset]");
    eprintln!("       plugin export <id> [--config-only] [--out <path>]");
    eprintln!("       plugin import <file> [--config <id>] [--no-config] [--force]");
    eprintln!(
        "       plugin bundle <dir> --key <pem> [-o <out>] [--id <id>] [--version <semver>] [--exec <path>] [--desc <text>] [--key-id <id>] [--from-core] [--interpreter <name>] [--interpreter-version <constraint>] [--interpreter-arg <arg>] [--compat <device[,device...]>] [--platform <triple>] [--unsigned]"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_bundle_compat_and_platform() {
        assert_eq!(
            parse(&args(&[
                "bundle",
                "src",
                "--key",
                "key.pem",
                "--compat",
                "sr6110,sr1010",
                "--platform=aarch64-unknown-linux-musl"
            ]))
            .expect("bundle"),
            Command::Bundle(crate::bundle::Options {
                dir: PathBuf::from("src"),
                key: Some(PathBuf::from("key.pem")),
                compat: vec!["sr6110".into(), "sr1010".into()],
                platform: Some("aarch64-unknown-linux-musl".into()),
                ..Default::default()
            })
        );
        assert!(parse(&args(&["bundle", "src", "--compat"])).is_err());
        assert!(parse(&args(&["bundle", "src", "--platform"])).is_err());
        assert_eq!(
            parse(&args(&["bundle", "src", "--unsigned"])).expect("unsigned bundle"),
            Command::Bundle(crate::bundle::Options {
                dir: PathBuf::from("src"),
                unsigned: true,
                ..Default::default()
            })
        );
    }

    #[test]
    fn parses_plugin_commands() {
        assert_eq!(parse(&args(&["list"])).expect("list"), Command::List);
        assert_eq!(
            parse(&args(&["status", "demo", "--json"])).expect("status"),
            Command::Status {
                id: "demo".into(),
                json: true
            }
        );
        assert_eq!(
            parse(&args(&["install", "demo.spk", "--strategy", "replace"])).expect("install"),
            Command::Install {
                path: PathBuf::from("demo.spk"),
                strategy: "replace".into(),
                allow_hooks: false
            }
        );
        assert_eq!(
            parse(&args(&["enable", "demo"])).expect("enable"),
            Command::Enable { id: "demo".into() }
        );
        assert_eq!(
            parse(&args(&["disable", "demo"])).expect("disable"),
            Command::Disable { id: "demo".into() }
        );
        assert_eq!(
            parse(&args(&["restart", "demo"])).expect("restart"),
            Command::Restart { id: "demo".into() }
        );
        assert!(parse(&args(&["enable"])).is_err());
        assert!(parse(&args(&["restart"])).is_err());
        assert!(parse(&args(&["disable", "one", "two"])).is_err());
        assert_eq!(
            parse(&args(&["install", "demo.spk"])).expect("default install"),
            Command::Install {
                path: PathBuf::from("demo.spk"),
                strategy: "replace".into(),
                allow_hooks: false
            }
        );
        assert_eq!(
            parse(&args(&["install", "demo.spk", "--allow-hooks"])).expect("hooks install"),
            Command::Install {
                path: PathBuf::from("demo.spk"),
                strategy: "replace".into(),
                allow_hooks: true
            }
        );
        assert_eq!(
            parse(&args(&["verify", "demo.spk"])).expect("verify defaults"),
            Command::Verify {
                path: PathBuf::from("demo.spk"),
                json: false,
                pubkey: None
            }
        );
        assert_eq!(
            parse(&args(&["verify", "demo.spk", "--json"])).expect("verify json"),
            Command::Verify {
                path: PathBuf::from("demo.spk"),
                json: true,
                pubkey: None
            }
        );
        assert_eq!(
            parse(&args(&["verify", "demo.spk", "--pubkey", "release.pub"]))
                .expect("verify explicit key"),
            Command::Verify {
                path: PathBuf::from("demo.spk"),
                json: false,
                pubkey: Some(PathBuf::from("release.pub"))
            }
        );
        assert!(parse(&args(&["verify", "demo.spk", "--allow-hooks"])).is_err());
        assert!(parse(&args(&["verify"])).is_err());
        assert!(parse(&args(&["verify", "one.spk", "two.spk"])).is_err());
        assert_eq!(
            parse(&args(&["uninstall", "demo", "--purge"])).expect("uninstall"),
            Command::Uninstall {
                id: "demo".into(),
                purge: true
            }
        );
        assert!(parse(&args(&["install", "demo.spk", "--strategy", "nope"])).is_err());
        assert!(parse(&args(&["status"])).is_err());
        assert!(parse(&args(&["list", "extra"])).is_err());
        assert_eq!(
            parse(&args(&["editconfig", "demo", "--print"])).expect("editconfig"),
            Command::EditConfig {
                id: "demo".into(),
                print: true,
                file: None,
                reset: false
            }
        );
        assert_eq!(
            parse(&args(&["export", "demo", "--config-only", "--out", "x"])).expect("export"),
            Command::Export {
                id: "demo".into(),
                config_only: true,
                out: Some(PathBuf::from("x"))
            }
        );
        assert_eq!(
            parse(&args(&[
                "import",
                "x.spkb",
                "--config",
                "demo",
                "--no-config",
                "--force"
            ]))
            .expect("import"),
            Command::Import {
                path: PathBuf::from("x.spkb"),
                config_id: Some("demo".into()),
                no_config: true,
                force: true
            }
        );
        assert!(parse(&args(&["editconfig", "demo", "--print", "--reset"])).is_err());
        assert_eq!(
            parse(&args(&[
                "bundle",
                "samples/demo",
                "--key",
                "key.pem",
                "-o",
                "out.spk",
                "--id",
                "demo",
                "--version",
                "1.2.3",
                "--exec",
                "payload/bin/demo",
                "--desc",
                "demo plugin",
                "--key-id",
                "test"
            ]))
            .expect("bundle"),
            Command::Bundle(crate::bundle::Options {
                dir: PathBuf::from("samples/demo"),
                out: Some(PathBuf::from("out.spk")),
                key: Some(PathBuf::from("key.pem")),
                key_id: Some("test".into()),
                id: Some("demo".into()),
                version: Some("1.2.3".into()),
                exec: Some("payload/bin/demo".into()),
                desc: Some("demo plugin".into()),
                from_core: false,
                interpreter: None,
                interpreter_version: None,
                interpreter_args: Vec::new(),
                compat: Vec::new(),
                platform: None,
                unsigned: false
            })
        );
        assert_eq!(
            parse(&args(&[
                "bundle",
                "samples/demo",
                "--key",
                "key.pem",
                "--from-core",
                "--interpreter",
                "sh",
                "--interpreter-version",
                ">=5",
                "--interpreter-arg=-e"
            ]))
            .expect("script bundle"),
            Command::Bundle(crate::bundle::Options {
                dir: PathBuf::from("samples/demo"),
                out: None,
                key: Some(PathBuf::from("key.pem")),
                key_id: None,
                id: None,
                version: None,
                exec: None,
                desc: None,
                from_core: true,
                interpreter: Some("sh".into()),
                interpreter_version: Some(">=5".into()),
                interpreter_args: vec!["-e".into()],
                compat: Vec::new(),
                platform: None,
                unsigned: false
            })
        );
        assert!(parse(&args(&["bundle", "a", "b"])).is_err());
    }
}
