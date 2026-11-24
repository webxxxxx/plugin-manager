use super::*;

pub(super) struct VerifyHost {
    pub(super) matches_host: bool,
    pub(super) device: Option<String>,
    pub(super) interpreter: Option<String>,
}

pub(super) fn verify_host(config: &Config, inspection: &package::Inspection) -> VerifyHost {
    let host = compat::detect(&config.paths, MAX_SCHEMA, MAX_PROTOCOL);
    VerifyHost {
        matches_host: inspection.package.manifest.platform == host.platform,
        device: host.device,
        interpreter: crate::manage::prepare(&inspection.package)
            .ok()
            .flatten()
            .map(|interpreter| interpreter.path),
    }
}

pub(super) fn verify_failure(file: &str, error: &str, json: bool) -> i32 {
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

pub(super) fn verify_json(
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
        "id": manifest.id(),
        "name": manifest.name,
        "displayName": manifest.name,
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

pub(super) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub(super) fn print_status(status: Status, json: bool) -> i32 {
    client::print_status(status, json, "plugin")
}

pub(super) fn usage() {
    eprintln!("usage: plugin list");
    eprintln!("       plugin status [--json] <id>");
    eprintln!("       plugin start|stop|restart <id>");
    eprintln!("       plugin enable|disable <id>");
    eprintln!("       plugin install <file.spk> [--strategy keep-old|replace] [--allow-hooks] [--root <dir>] (default: replace, root: smallest that fits)");
    eprintln!("       plugin verify <file.spk> [--json] [--pubkey <pem>]");
    eprintln!("       plugin uninstall <id> [--purge]");
    eprintln!("       plugin editconfig <id> [--print|--file <path>|--reset]");
    eprintln!("       plugin export <id> [--config-only] [--out <path>]");
    eprintln!("       plugin import <file> [--config <id>] [--no-config] [--force]");
    eprintln!(
        "       plugin bundle <dir> --key <pem> [-o <file|dir>] [--id <id>] [--name <display name>] [--version <semver>] [--exec <path>] [--desc <text>] [--key-id <id>] [--from-core] [--interpreter <name>] [--interpreter-version <constraint>] [--interpreter-arg <arg>] [--compat <device[,device...]>] [--platform <triple>] [--unsigned]"
    );
}
