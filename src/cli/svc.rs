use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::cli::client::{
    self, EXIT_DAEMON_UNREACHABLE, EXIT_NOT_FOUND, EXIT_OK, EXIT_SERVICE_ERROR, EXIT_USAGE,
};
use crate::cli::status;
use crate::cli::tune;
use crate::kernel::projection::Status;
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::platform::config::Config;
use crate::platform::log::Level;
use crate::platform::settings;
use crate::wire::control::channel;

mod logs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonVerb {
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthAction {
    Status,
    Passwd(String),
}

#[derive(Debug, PartialEq, Eq)]
enum TrustAction {
    List,
    Add(PathBuf),
    Remove(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    List,
    Status {
        id: String,
        json: bool,
    },
    Daemon {
        verb: DaemonVerb,
        id: String,
    },
    Reload,
    Compat {
        json: bool,
    },
    CompatInit {
        device: String,
        dry_run: bool,
    },
    Logs {
        id: Option<String>,
        follow: bool,
        lines: usize,
        level: Option<Level>,
        all: bool,
    },
    Auth {
        action: AuthAction,
    },
    Trust {
        action: TrustAction,
    },
    /// `svc status --resources`: what the last start derived, from the
    /// snapshot the daemon wrote.
    Resources {
        json: bool,
    },
    /// `svc config tune`: the configuration this machine suggests.
    ConfigTune {
        write: bool,
    },
}

pub fn run(args: Vec<String>) -> i32 {
    let command = match parse(&args) {
        Ok(command) => command,
        Err(()) => {
            usage();
            return EXIT_USAGE;
        }
    };

    // Inspection, not bootstrap: a document the daemon refuses is exactly when
    // `svc status --resources` and `svc config tune` have to be readable.
    let Some(config) = Config::inspect_or_report("svc") else {
        return EXIT_SERVICE_ERROR;
    };
    match command {
        Command::List => list(&config),
        Command::Status { id, json } => show_status(&config, &id, json),
        Command::Daemon { verb, id } => daemon_command(&config, verb, &id),
        Command::Reload => reload(&config),
        Command::Compat { json } => compat_show(&config, json),
        Command::CompatInit { device, dry_run } => compat_init(&config, &device, dry_run),
        Command::Logs {
            id,
            follow,
            lines,
            level,
            all,
        } => logs::show_logs(&config, id.as_deref(), follow, lines, level, all),
        Command::Auth { action } => auth_command(&config, action),
        Command::Trust { action } => trust_command(&config, action),
        Command::Resources { json } => resources_command(&config, json),
        Command::ConfigTune { write } => config_tune(&config, write),
    }
}

fn parse(args: &[String]) -> Result<Command, ()> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(());
    };

    match command {
        "list" => {
            if args.len() == 1 {
                Ok(Command::List)
            } else {
                Err(())
            }
        }
        "status" => {
            let mut id = None;
            let mut json = false;
            let mut resources = false;
            for arg in &args[1..] {
                if arg == "--json" {
                    json = true;
                } else if arg == "--resources" {
                    resources = true;
                } else if arg.starts_with('-') || id.is_some() {
                    return Err(());
                } else {
                    id = Some(arg.clone());
                }
            }
            if resources {
                return match id {
                    None => Ok(Command::Resources { json }),
                    Some(_) => Err(()),
                };
            }
            id.map(|id| Command::Status { id, json }).ok_or(())
        }
        "start" | "stop" | "restart" | "enable" | "disable" => {
            if args.len() != 2 || args[1].starts_with('-') {
                return Err(());
            }
            let verb = match command {
                "start" => DaemonVerb::Start,
                "stop" => DaemonVerb::Stop,
                "restart" => DaemonVerb::Restart,
                "enable" => DaemonVerb::Enable,
                "disable" => DaemonVerb::Disable,
                _ => unreachable!("matched daemon command"),
            };
            Ok(Command::Daemon {
                verb,
                id: args[1].clone(),
            })
        }
        "logs" => parse_logs(&args[1..]),
        "compat" => parse_compat(&args[1..]),
        "reload" if args.len() == 1 => Ok(Command::Reload),
        "auth" | "lan" => parse_auth(&args[1..]),
        "trust" => parse_trust(&args[1..]),
        "config" => parse_config(&args[1..]),
        _ => Err(()),
    }
}

fn parse_config(args: &[String]) -> Result<Command, ()> {
    match args {
        [action] if action == "tune" => Ok(Command::ConfigTune { write: false }),
        [action, flag] if action == "tune" && flag == "--write" => {
            Ok(Command::ConfigTune { write: true })
        }
        _ => Err(()),
    }
}

fn parse_trust(args: &[String]) -> Result<Command, ()> {
    let action = match args {
        [action] if action == "list" => TrustAction::List,
        [action, path] if action == "add" && !path.starts_with('-') => {
            TrustAction::Add(PathBuf::from(path))
        }
        [action, id] if action == "remove" && !id.starts_with('-') => {
            TrustAction::Remove(id.clone())
        }
        _ => return Err(()),
    };
    Ok(Command::Trust { action })
}

fn parse_auth(args: &[String]) -> Result<Command, ()> {
    match args {
        [action] if action == "status" => Ok(Command::Auth {
            action: AuthAction::Status,
        }),
        [action, password]
            if action == "passwd" && !password.is_empty() && !password.starts_with('-') =>
        {
            Ok(Command::Auth {
                action: AuthAction::Passwd(password.clone()),
            })
        }
        _ => Err(()),
    }
}

fn parse_compat(args: &[String]) -> Result<Command, ()> {
    let Some(first) = args.first().map(String::as_str) else {
        return Ok(Command::Compat { json: false });
    };
    if first == "--json" {
        return if args.len() == 1 {
            Ok(Command::Compat { json: true })
        } else {
            Err(())
        };
    }
    if first != "init" {
        return Err(());
    }
    let mut device = None;
    let mut dry_run = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--device" => {
                index += 1;
                let value = args.get(index).ok_or(())?;
                device = Some(value.clone());
            }
            "--dry-run" => dry_run = true,
            other => {
                if let Some(value) = other.strip_prefix("--device=") {
                    if value.is_empty() {
                        return Err(());
                    }
                    device = Some(value.to_string());
                } else {
                    return Err(());
                }
            }
        }
        index += 1;
    }
    device
        .map(|device| Command::CompatInit { device, dry_run })
        .ok_or(())
}

fn parse_logs(args: &[String]) -> Result<Command, ()> {
    let mut id = None;
    let mut follow = false;
    let mut lines = 200usize;
    let mut level = None;
    let mut all = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--follow" || arg == "-f" {
            follow = true;
        } else if arg == "--all" {
            all = true;
        } else if arg == "--lines" {
            index += 1;
            let value = args.get(index).ok_or(())?;
            lines = parse_lines(value)?;
        } else if let Some(value) = arg.strip_prefix("--lines=") {
            lines = parse_lines(value)?;
        } else if arg == "--level" {
            index += 1;
            level = Some(parse_level(args.get(index).ok_or(())?)?);
        } else if let Some(value) = arg.strip_prefix("--level=") {
            level = Some(parse_level(value)?);
        } else if arg.starts_with('-') || id.is_some() {
            return Err(());
        } else {
            id = Some(arg.clone());
        }
        index += 1;
    }
    if all && (id.is_some() || follow) {
        return Err(());
    }
    if !all && id.is_none() {
        return Err(());
    }
    Ok(Command::Logs {
        id,
        follow,
        lines,
        level,
        all,
    })
}

fn parse_level(value: &str) -> Result<Level, ()> {
    Level::parse(value).ok_or(())
}

fn parse_lines(value: &str) -> Result<usize, ()> {
    let lines = value.parse::<usize>().map_err(|_| ())?;
    if lines > 2000 {
        return Err(());
    }
    Ok(lines)
}

/// `svc list` reads live state only. It used to fall back to reading
/// `services.d` when the daemon could not be reached, which made "daemon is
/// down" and "there are no services" look identical; the fallback is gone and an
/// unreachable daemon is now reported and exits non-zero.
fn list(config: &Config) -> i32 {
    match live_list(config) {
        Ok(statuses) => {
            println!("{}", status::table(&statuses));
            EXIT_OK
        }
        Err(ListError::Unreachable(error)) => {
            eprintln!("svc: svcd unreachable: {}", error);
            explain_unreachable(config);
            EXIT_DAEMON_UNREACHABLE
        }
        Err(ListError::Daemon(message)) => {
            eprintln!("svc: {}", message);
            EXIT_SERVICE_ERROR
        }
    }
}

/// A user-mode CLI cannot open the machine-wide control pipe: the pipe name
/// hashes the config directory, and the SCM service uses the machine-wide one.
/// The pipe ACL is the real gate (Administrators only become effective after
/// elevation; the `svcd` group is the read-only tier), so point at that instead
/// of leaving "pipe is not available" unexplained.
fn explain_unreachable(config: &Config) {
    if std::env::var_os("SVCD_CONFIG_DIR").is_some() {
        return;
    }
    let machine = crate::platform::paths::platform_config_dir();
    if machine == config.paths.config_dir || !machine_wide_install_present(&machine) {
        return;
    }
    if cfg!(windows) {
        eprintln!(
            "svc: hint: a machine-wide svcd is installed at {}; run this command from an elevated terminal, or add your account to the \"svcd\" group for read-only commands",
            machine.display()
        );
    } else {
        eprintln!(
            "svc: hint: a machine-wide svcd is installed at {}; run this command as root",
            machine.display()
        );
    }
}

/// A non-elevated administrator cannot read the protected machine-wide config
/// directory after hardening, so also accept its visible parent (`%ProgramData%\svcd`),
/// which the installer and service both create.
fn machine_wide_install_present(machine: &Path) -> bool {
    if machine.join("svcd.conf").is_file() {
        return true;
    }
    cfg!(windows) && machine.parent().is_some_and(|parent| parent.is_dir())
}

/// A live listing failed. An unreachable daemon is an environment problem (exit
/// 2); a daemon that answered with an error is a service problem (exit 1).
enum ListError {
    Unreachable(String),
    Daemon(String),
}

fn live_list(config: &Config) -> Result<Vec<Status>, ListError> {
    let response = channel::request(config.paths.control_dir(), "svc.list", json!({}))
        .map_err(|error| ListError::Unreachable(error.to_string()))?;
    if let Some(error) = response.error {
        return Err(ListError::Daemon(error.message));
    }
    let services = response
        .result
        .and_then(|result| result.get("services").cloned())
        .ok_or_else(|| ListError::Daemon("empty list response".into()))?;
    serde_json::from_value::<Vec<Status>>(services)
        .map_err(|error| ListError::Daemon(format!("invalid list response: {}", error)))
}

/// `svc status` reads live state only, for the same reason as `svc list`: the
/// offline fallback made an unreachable daemon look like a stopped service.
fn show_status(config: &Config, id: &str, json: bool) -> i32 {
    match client::status(config, id, true, "svc") {
        Ok(status) => client::print_status(status, json, "svc"),
        Err(code) => {
            if code == EXIT_DAEMON_UNREACHABLE {
                explain_unreachable(config);
            }
            code
        }
    }
}

fn daemon_command(config: &Config, verb: DaemonVerb, id: &str) -> i32 {
    let method = match verb {
        DaemonVerb::Start => "svc.start",
        DaemonVerb::Stop => "svc.stop",
        DaemonVerb::Restart => "svc.restart",
        DaemonVerb::Enable => "svc.enable",
        DaemonVerb::Disable => "svc.disable",
    };
    match client::request(config, method, json!({ "id": id }), "svc") {
        Ok(status) => client::print_status(status, false, "svc"),
        Err(code) => {
            if code == EXIT_DAEMON_UNREACHABLE {
                explain_unreachable(config);
            }
            code
        }
    }
}

fn reload(config: &Config) -> i32 {
    let response = match channel::request(config.paths.control_dir(), "svc.reload", json!({})) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("svc: svcd unreachable: {}", error);
            return EXIT_DAEMON_UNREACHABLE;
        }
    };
    if let Some(error) = response.error {
        client::report_error("svc", &error);
        return EXIT_SERVICE_ERROR;
    }
    let result = response.result.unwrap_or(Value::Null);
    let ids = |key: &str| {
        result
            .get(key)
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    for (key, label) in [
        ("added", "added"),
        ("removed", "removed"),
        ("updated", "updated"),
    ] {
        let list = ids(key);
        if !list.is_empty() {
            println!("{}: {}", label, list);
        }
    }
    if ids("added").is_empty() && ids("removed").is_empty() && ids("updated").is_empty() {
        println!("reloaded: no changes");
    }
    EXIT_OK
}

/// `svc status --resources` reads the snapshot the daemon wrote at startup, so
/// the numbers are the ones in force rather than a second guess at this
/// machine. Without a snapshot it falls back to a local derivation and says so.
fn resources_command(config: &Config, json: bool) -> i32 {
    let (snapshot, origin) = match crate::platform::limits::read_snapshot(&config.paths) {
        Ok(snapshot) => (snapshot, "last start"),
        Err(_) => (
            config.resources.to_json(),
            "this process (the daemon left no snapshot; numbers are a local derivation)",
        ),
    };
    if json {
        match serde_json::to_string_pretty(&snapshot) {
            Ok(text) => println!("{}", text),
            Err(error) => {
                eprintln!("svc: cannot encode the resource report: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
        return EXIT_OK;
    }
    print!(
        "{}",
        tune::resources_text(&snapshot, origin, &config.settings.probe_overrides())
    );
    EXIT_OK
}

/// `svc config tune` prints (or writes) the configuration this machine
/// suggests: one key per line with its derivation in the trailing comment.
fn config_tune(config: &Config, write: bool) -> i32 {
    let mut body = tune::config_text(&config.limits, &config.resources);
    if let Some(reason) = &config.resources.refusal {
        // Writing this block does not by itself make the document runnable: the
        // terms in the message are what must change.
        body.push_str(&format!(
            "#\n# WARNING: the daemon refuses to start with the current document:\n\
             #   {}\n\
             # The values below are what this machine can use. Reduce the largest term\n\
             # named above (usually request_stack_bytes) before restarting.\n",
            reason
        ));
    }
    if !write {
        print!("{}", body);
        return EXIT_OK;
    }
    let Some(path) = config.settings_path.clone() else {
        eprintln!("svc: no svcd.conf was located; pass --conf or start the daemon once");
        return EXIT_SERVICE_ERROR;
    };
    match settings::upsert_block(&path, tune::BLOCK_BEGIN, tune::BLOCK_END, &body) {
        Ok(()) => {
            println!(
                "wrote {} (block {} .. {})",
                path.display(),
                tune::BLOCK_BEGIN,
                tune::BLOCK_END
            );
            println!("restart svcd to apply");
            EXIT_OK
        }
        Err(error) => {
            eprintln!("svc: {}", error);
            EXIT_SERVICE_ERROR
        }
    }
}

fn compat_show(config: &Config, json: bool) -> i32 {
    let facts = crate::platform::compat::detect(&config.paths, MAX_SCHEMA, MAX_PROTOCOL);
    if json {
        match serde_json::to_string_pretty(&facts) {
            Ok(text) => println!("{}", text),
            Err(error) => {
                eprintln!("svc: cannot encode compat facts: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
        return EXIT_OK;
    }
    println!("platform:      {}", facts.platform);
    match (&facts.device, &facts.device_source) {
        (Some(device), Some(source)) => println!("device:        {} ({})", device, source),
        _ => println!("device:        - (not provisioned)"),
    }
    println!(
        "kernel:        {}",
        facts.kernel_release.as_deref().unwrap_or("-")
    );
    println!("svcd:          {}", facts.svcd_version);
    println!("schema_max:    {}", facts.schema_max);
    println!("protocol_max:  {}", facts.protocol_max);
    println!("marker:        {}", facts.marker_path);
    for warning in &facts.warnings {
        eprintln!("svc: warning: {}", warning);
    }
    EXIT_OK
}

fn compat_init(config: &Config, device: &str, dry_run: bool) -> i32 {
    if let Err(error) = crate::manage::root_required() {
        eprintln!("svc: {}", error);
        return EXIT_SERVICE_ERROR;
    }
    if let Err(error) = crate::platform::compat::validate_device_id(device) {
        eprintln!("svc: {}", error);
        return EXIT_SERVICE_ERROR;
    }
    let path = crate::platform::compat::marker_path(&config.paths);
    if dry_run {
        println!("would write {}:", path.display());
        println!("device = {}", device);
        return EXIT_OK;
    }
    if let Err(error) = crate::platform::compat::write_device(&path, device) {
        eprintln!("svc: {}", error);
        return EXIT_SERVICE_ERROR;
    }
    println!("wrote {} (device = {})", path.display(), device);
    compat_show(config, false)
}

fn auth_command(config: &Config, action: AuthAction) -> i32 {
    if matches!(&action, AuthAction::Passwd(_)) {
        if let Err(error) = crate::server::auth::lan::require_root() {
            eprintln!("svc: {}", error);
            return EXIT_SERVICE_ERROR;
        }
    }
    let (method, params) = match &action {
        AuthAction::Status => ("auth.status", json!({})),
        AuthAction::Passwd(password) => ("auth.passwd", json!({ "password": password })),
    };
    let response = match channel::request(config.paths.control_dir(), method, params) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("svc: svcd unreachable: {}", error);
            return EXIT_DAEMON_UNREACHABLE;
        }
    };
    if let Some(error) = response.error {
        client::report_error("svc", &error);
        return EXIT_SERVICE_ERROR;
    }
    let result = response.result.unwrap_or(Value::Null);
    if matches!(&action, AuthAction::Status) {
        println!(
            "bind: {}",
            result.get("bind").and_then(Value::as_str).unwrap_or("-")
        );
        println!(
            "requested: {}",
            result
                .get("requestedBind")
                .and_then(Value::as_str)
                .unwrap_or("-")
        );
        println!(
            "auth required: {}",
            result
                .get("authRequired")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
        println!(
            "password: {}",
            if result
                .get("passwordSet")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "set"
            } else {
                "unset"
            }
        );
    } else {
        println!("password: set");
        println!(
            "bind: {}",
            result.get("bind").and_then(Value::as_str).unwrap_or("-")
        );
    }
    EXIT_OK
}

fn trust_command(config: &Config, action: TrustAction) -> i32 {
    match action {
        TrustAction::List => match crate::manage::check::trust::keys(&config.paths) {
            Ok(keys) if keys.is_empty() => {
                println!("no trusted keys in {}", config.paths.trust_dir.display());
                EXIT_OK
            }
            Ok(keys) => {
                for key in keys {
                    println!("{}", key);
                }
                EXIT_OK
            }
            Err(error) => {
                eprintln!("svc: {}", error);
                EXIT_SERVICE_ERROR
            }
        },
        TrustAction::Add(path) => {
            if let Err(error) = require_root_for_trust() {
                eprintln!("svc: {}", error);
                return EXIT_SERVICE_ERROR;
            }
            let pem = match std::fs::read(&path) {
                Ok(pem) => pem,
                Err(error) => {
                    eprintln!("svc: cannot read {}: {}", path.display(), error);
                    return EXIT_SERVICE_ERROR;
                }
            };
            let key_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            match crate::manage::check::trust::add(&config.paths, key_id, &pem) {
                Ok(target) => {
                    println!("trusted {} ({})", key_id, target.display());
                    EXIT_OK
                }
                Err(error) => {
                    eprintln!("svc: {}", error);
                    EXIT_SERVICE_ERROR
                }
            }
        }
        TrustAction::Remove(key_id) => {
            if let Err(error) = require_root_for_trust() {
                eprintln!("svc: {}", error);
                return EXIT_SERVICE_ERROR;
            }
            match crate::manage::check::trust::remove(&config.paths, &key_id) {
                Ok(true) => {
                    println!("removed {}", key_id);
                    EXIT_OK
                }
                Ok(false) => {
                    eprintln!("svc: unknown trusted key: {}", key_id);
                    EXIT_NOT_FOUND
                }
                Err(error) => {
                    eprintln!("svc: {}", error);
                    EXIT_SERVICE_ERROR
                }
            }
        }
    }
}

fn require_root_for_trust() -> Result<(), String> {
    if crate::platform::is_privileged() || crate::platform::paths::user_mode() {
        Ok(())
    } else {
        Err("root / Administrator is required to change trust roots".into())
    }
}

// Only the verbs not implemented by #29 reach this helper.
fn usage() {
    eprintln!("usage: svc list");
    eprintln!("       svc status [--json] <id>");
    eprintln!("       svc <start|stop|restart|enable|disable> <id>");
    eprintln!("       svc reload");
    eprintln!("       svc compat [--json]");
    eprintln!("       svc compat init --device <id> [--dry-run]");
    eprintln!("       svc logs <id> [--follow|-f] [--lines N] [--level L]");
    eprintln!("       svc logs --all [--lines N] [--level L]");
    eprintln!("       svc lan status|passwd <password>");
    eprintln!("       svc auth ...  (deprecated alias of svc lan)");
    eprintln!("       svc trust list|add <file.pub>|remove <keyId>");
    eprintln!("       svc status --resources [--json]");
    eprintln!("       svc config tune [--write]");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_list() {
        assert_eq!(parse(&args(&["list"])).expect("list"), Command::List);
        assert!(parse(&args(&["list", "--json"])).is_err());
        assert!(parse(&args(&[])).is_err());
    }

    #[test]
    fn parses_status_with_json_in_either_position() {
        assert_eq!(
            parse(&args(&["status", "demo"])).expect("status"),
            Command::Status {
                id: "demo".into(),
                json: false
            }
        );
        assert_eq!(
            parse(&args(&["status", "--json", "demo"])).expect("status json"),
            Command::Status {
                id: "demo".into(),
                json: true
            }
        );
        assert_eq!(
            parse(&args(&["status", "demo", "--json"])).expect("status json"),
            Command::Status {
                id: "demo".into(),
                json: true
            }
        );
    }

    #[test]
    fn parses_the_resource_and_tuning_views() {
        assert_eq!(
            parse(&args(&["status", "--resources"])).expect("resources"),
            Command::Resources { json: false }
        );
        assert_eq!(
            parse(&args(&["status", "--resources", "--json"])).expect("resources json"),
            Command::Resources { json: true }
        );
        assert_eq!(
            parse(&args(&["config", "tune"])).expect("tune"),
            Command::ConfigTune { write: false }
        );
        assert_eq!(
            parse(&args(&["config", "tune", "--write"])).expect("tune write"),
            Command::ConfigTune { write: true }
        );
        // A resource view is not a service status: an id has no meaning there,
        // and an unknown config action is a usage error.
        assert!(parse(&args(&["status", "--resources", "demo"])).is_err());
        assert!(parse(&args(&["config"])).is_err());
        assert!(parse(&args(&["config", "apply"])).is_err());
        assert!(parse(&args(&["config", "tune", "--force"])).is_err());
    }

    #[test]
    fn rejects_status_without_an_id_or_with_two_ids() {
        assert!(parse(&args(&["status"])).is_err());
        assert!(parse(&args(&["status", "--json"])).is_err());
        assert!(parse(&args(&["status", "one", "two"])).is_err());
        assert!(parse(&args(&["status", "--nope", "one"])).is_err());
    }

    #[test]
    fn parses_daemon_commands_but_rejects_missing_ids() {
        assert_eq!(
            parse(&args(&["start", "demo"])).expect("start"),
            Command::Daemon {
                verb: DaemonVerb::Start,
                id: "demo".into()
            }
        );
        assert_eq!(
            parse(&args(&["restart", "demo"])).expect("restart"),
            Command::Daemon {
                verb: DaemonVerb::Restart,
                id: "demo".into()
            }
        );
        assert!(parse(&args(&["start"])).is_err());
        assert!(parse(&args(&["restart"])).is_err());
        assert!(parse(&args(&["stop", "demo", "extra"])).is_err());
        assert!(parse(&args(&["logs", "--follow"])).is_err());
    }

    #[test]
    fn parses_reload_and_rejects_extra_arguments() {
        assert_eq!(parse(&args(&["reload"])).expect("reload"), Command::Reload);
        assert!(parse(&args(&["reload", "--force"])).is_err());
    }

    #[test]
    fn parses_compat_and_rejects_unknown_forms() {
        assert_eq!(
            parse(&args(&["compat"])).expect("compat"),
            Command::Compat { json: false }
        );
        assert_eq!(
            parse(&args(&["compat", "--json"])).expect("compat json"),
            Command::Compat { json: true }
        );
        assert_eq!(
            parse(&args(&["compat", "init", "--device", "sr6110"])).expect("init"),
            Command::CompatInit {
                device: "sr6110".into(),
                dry_run: false
            }
        );
        assert_eq!(
            parse(&args(&["compat", "init", "--device=sr6110", "--dry-run"])).expect("init"),
            Command::CompatInit {
                device: "sr6110".into(),
                dry_run: true
            }
        );
        assert!(parse(&args(&["compat", "init"])).is_err());
        assert!(parse(&args(&["compat", "--json", "init"])).is_err());
        assert!(parse(&args(&["compat", "init", "--device"])).is_err());
        assert!(parse(&args(&["compat", "bogus"])).is_err());
    }

    #[test]
    fn parses_lan_commands_and_the_deprecated_auth_alias() {
        assert_eq!(
            parse(&args(&["lan", "status"])).expect("lan status"),
            Command::Auth {
                action: AuthAction::Status
            }
        );
        assert_eq!(
            parse(&args(&["lan", "passwd", "secret"])).expect("lan passwd"),
            Command::Auth {
                action: AuthAction::Passwd("secret".into())
            }
        );
        // Existing scripts keep working while the docs move to `svc lan`.
        assert_eq!(
            parse(&args(&["auth", "status"])).expect("auth alias"),
            Command::Auth {
                action: AuthAction::Status
            }
        );
        assert!(parse(&args(&["lan", "enable", "--bind"])).is_err());
        assert!(parse(&args(&["lan", "bogus"])).is_err());
        assert!(parse(&args(&["auth", "passwd"])).is_err());
        assert!(parse(&args(&["lan", "passwd"])).is_err());
    }

    #[test]
    fn parses_trust_commands() {
        assert_eq!(
            parse(&args(&["trust", "list"])).expect("trust list"),
            Command::Trust {
                action: TrustAction::List
            }
        );
        assert_eq!(
            parse(&args(&["trust", "add", "sample-2026.pub"])).expect("trust add"),
            Command::Trust {
                action: TrustAction::Add(PathBuf::from("sample-2026.pub"))
            }
        );
        assert_eq!(
            parse(&args(&["trust", "remove", "sample-2026"])).expect("trust remove"),
            Command::Trust {
                action: TrustAction::Remove("sample-2026".into())
            }
        );
        assert!(parse(&args(&["trust"])).is_err());
        assert!(parse(&args(&["trust", "add"])).is_err());
        assert!(parse(&args(&["trust", "add", "--key-id"])).is_err());
        assert!(parse(&args(&["trust", "bogus"])).is_err());
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(parse(&args(&["describe", "demo"])).is_err());
        assert!(parse(&args(&["--json", "status", "demo"])).is_err());
    }
}
