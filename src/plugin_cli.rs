//! The `plugin` entry: plugin-only views over the same daemon channel.
//!
//! `svc` stays the generic service CLI; `plugin` refuses core ids for
//! status/start/stop and adds install/uninstall.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::channel;
use crate::paths;
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
    Install {
        path: PathBuf,
        strategy: String,
        allow_hooks: bool,
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
    match command {
        Command::List => list(),
        Command::Status { id, json } => status_command(&id, json),
        Command::Start { id } => lifecycle("start", &id),
        Command::Stop { id } => lifecycle("stop", &id),
        Command::Install {
            path,
            strategy,
            allow_hooks,
        } => install(&path, &strategy, allow_hooks),
        Command::Uninstall { id, purge } => uninstall(&id, purge),
        Command::EditConfig {
            id,
            print,
            file,
            reset,
        } => edit_config(&id, print, file.as_deref(), reset),
        Command::Export {
            id,
            config_only,
            out,
        } => export(&id, config_only, out.as_deref()),
        Command::Import {
            path,
            config_id,
            no_config,
            force,
        } => import(&path, config_id.as_deref(), no_config, force),
        Command::Bundle(options) => bundle_command(options),
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
        "start" | "stop" => {
            if args.len() != 2 || args[1].starts_with('-') {
                return Err(());
            }
            let id = args[1].clone();
            if command == "start" {
                Ok(Command::Start { id })
            } else {
                Ok(Command::Stop { id })
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
                })
            })
            .ok_or(())
        }
        _ => Err(()),
    }
}

fn list() -> i32 {
    let response = match channel::request(&paths::config_dir(), "svc.list", json!({})) {
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

fn status_command(id: &str, json: bool) -> i32 {
    let status = match fetch_status(id, true) {
        Ok(status) => status,
        Err(code) => return code,
    };
    if status.summary.kind != Kind::Plugin {
        eprintln!("plugin: {} is not a plugin", id);
        return EXIT_SERVICE_ERROR;
    }
    print_status(status, json)
}

fn lifecycle(verb: &str, id: &str) -> i32 {
    let status = match fetch_status(id, false) {
        Ok(status) => status,
        Err(code) => return code,
    };
    if status.summary.kind != Kind::Plugin {
        eprintln!("plugin: {} is not a plugin", id);
        return EXIT_SERVICE_ERROR;
    }
    let method = format!("svc.{}", verb);
    match request(&method, json!({ "id": id })) {
        Ok(status) => print_status(status, false),
        Err(code) => code,
    }
}

fn install(path: &PathBuf, strategy: &str, allow_hooks: bool) -> i32 {
    let path = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("plugin: cannot read {}: {}", path.display(), error);
            return EXIT_SERVICE_ERROR;
        }
    };
    match request(
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

fn uninstall(id: &str, purge: bool) -> i32 {
    match channel::request(
        &paths::config_dir(),
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

fn edit_config(id: &str, print: bool, file: Option<&Path>, reset: bool) -> i32 {
    if print {
        let Some(path) = plugin::active_config(id) else {
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
    let _lock = match plugin::lock_config(id) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("plugin: {}", error);
            return EXIT_SERVICE_ERROR;
        }
    };

    let candidate = if reset {
        match plugin::config_template(id) {
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
        match edit_with_editor(id) {
            Ok(data) => data,
            Err(error) => {
                eprintln!("plugin: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
    };

    match plugin::replace_config(id, &candidate, true) {
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

fn edit_with_editor(id: &str) -> Result<Vec<u8>, String> {
    let initial = plugin::active_config(id)
        .map(|path| fs::read(&path))
        .transpose()
        .map_err(|error| format!("read config: {}", error))?
        .or_else(|| plugin::config_template(id).map(|(data, _)| data))
        .ok_or_else(|| format!("plugin {} has no config", id))?;
    let directory = plugin::plugin_dir(id);
    let temporary = directory.join(format!("config.edit.{}", std::process::id()));
    plugin::write_config(&temporary, &initial)?;
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

fn export(id: &str, config_only: bool, out: Option<&Path>) -> i32 {
    let (version, data, extension) = if config_only {
        match plugin::export_config(id) {
            Ok((version, data)) => (version, data, "config"),
            Err(error) => {
                eprintln!("plugin: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
    } else {
        match plugin::export_spkb(id) {
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

fn import(path: &Path, config_id: Option<&str>, no_config: bool, force: bool) -> i32 {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("plugin: read {}: {}", path.display(), error);
            return EXIT_SERVICE_ERROR;
        }
    };
    match transfer::import(&data, config_id, no_config, force, true) {
        Ok(outcome) => {
            for warning in &outcome.warnings {
                eprintln!("plugin: warning: {}", warning);
            }
            if let Some(status) = outcome.status {
                print_status(status, false)
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

fn fetch_status(id: &str, probe: bool) -> Result<Status, i32> {
    // Match `svc status`: a plugin lifecycle `status` action runs only for the
    // status command, not as a side effect of start/stop.
    let mut params = json!({ "id": id });
    if probe {
        params["probe"] = json!(true);
    }
    request("svc.status", params)
}

fn request(method: &str, params: Value) -> Result<Status, i32> {
    let response = match channel::request(&paths::config_dir(), method, params) {
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
    eprintln!("       plugin start|stop <id>");
    eprintln!("       plugin install <file.spk> [--strategy keep-old|replace] [--allow-hooks] (default: replace)");
    eprintln!("       plugin uninstall <id> [--purge]");
    eprintln!("       plugin editconfig <id> [--print|--file <path>|--reset]");
    eprintln!("       plugin export <id> [--config-only] [--out <path>]");
    eprintln!("       plugin import <file> [--config <id>] [--no-config] [--force]");
    eprintln!(
        "       plugin bundle <dir> --key <pem> [-o <out>] [--id <id>] [--version <semver>] [--exec <path>] [--desc <text>] [--key-id <id>] [--from-core] [--interpreter <name>] [--interpreter-version <constraint>] [--interpreter-arg <arg>]"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
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
                interpreter_args: Vec::new()
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
                interpreter_args: vec!["-e".into()]
            })
        );
        assert!(parse(&args(&["bundle", "a", "b"])).is_err());
    }
}
