//! The manifest: author fields, exec/lifecycle/interpreter and `files[]`.
//!
//! The directory is the source of truth for `files[]`; `manifest.json` only
//! supplies author fields. The result is serialized with sorted keys, then
//! parsed and validated by the package model before it is signed.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;
use crate::manage::Manifest;

use super::scan::{detect_exec, effective_mode, normalize_exec, SourceFile};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub dir: PathBuf,
    pub out: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub key_id: Option<String>,
    pub id: Option<String>,
    /// Display name written into `name`; defaults to the id.
    pub name: Option<String>,
    pub version: Option<String>,
    pub exec: Option<String>,
    pub desc: Option<String>,
    /// Emit schema-2 lifecycle actions from the exec script's declarations.
    pub from_core: bool,
    /// Host interpreter required by a script payload.
    pub interpreter: Option<String>,
    pub interpreter_version: Option<String>,
    pub interpreter_args: Vec<String>,
    /// Device compatibility ids (`--compat sr6110`); empty means universal.
    pub compat: Vec<String>,
    /// Target triple written into the manifest. Defaults to the host triple;
    /// cross-packaging for a device (`aarch64-unknown-linux-musl`) sets it.
    pub platform: Option<String>,
    /// Produce an unsigned package. Local development only: it installs on a
    /// host with `allow_insecure_install`, and `plugin verify` reports the
    /// signature as `missing`.
    pub unsigned: bool,
}

pub(super) fn assemble_manifest(
    dir: &Path,
    sources: &mut [SourceFile],
    mut manifest: Map<String, Value>,
    options: &Options,
) -> Result<(Vec<u8>, Manifest), ProblemError> {
    let directory_name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string);
    let id = match &options.id {
        Some(id) => id.clone(),
        None => manifest
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| directory_name.clone())
            .ok_or_else(|| {
                Problem::Invalid
                    .error("cannot derive the plugin id from the directory name; pass --id")
            })?,
    };
    if !package::valid_id(&id) {
        return Err(Problem::Invalid.error(format!(
            "invalid plugin id: {} (expected [a-z0-9][a-z0-9_-]{{0,63}}; pass --id)",
            id
        )));
    }
    // `name` is the display name from here on: any UTF-8, defaults to the id.
    let name = match &options.name {
        Some(name) => name.clone(),
        None => manifest
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| id.clone()),
    };
    if !package::valid_display_name(&name) {
        return Err(Problem::Invalid.error(
            "invalid display name: it must be non-empty, at most 128 characters and free of \
             control characters (pass --name)",
        ));
    }
    let version = match &options.version {
        Some(version) => version.clone(),
        None => manifest
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "0.1.0".to_string()),
    };
    if !package::valid_version(&version) {
        return Err(Problem::Invalid.error(format!(
            "version is not semver: {} (pass --version)",
            version
        )));
    }
    let desc = match &options.desc {
        Some(desc) => desc.clone(),
        None => manifest
            .get("desc")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| directory_name.clone())
            .unwrap_or_else(|| id.clone()),
    };
    if desc.is_empty() {
        return Err(Problem::Invalid.error("desc must not be empty (pass --desc)"));
    }

    let declared_exec = manifest
        .get("exec")
        .and_then(Value::as_object)
        .and_then(|exec| exec.get("path"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let exec_path = match &options.exec {
        Some(value) => normalize_exec(value)?,
        None => match declared_exec {
            Some(path) => path,
            None => detect_exec(sources, options.from_core || options.interpreter.is_some())?,
        },
    };
    if !sources.iter().any(|source| source.path == exec_path) {
        return Err(Problem::Invalid.error(format!("exec.path is not in files[]: {}", exec_path)));
    }
    package::check_path(&exec_path)?;

    if options.from_core {
        let source = sources
            .iter()
            .find(|source| source.path == exec_path)
            .expect("exec membership checked above");
        if package::is_native_binary(&source.data) {
            return Err(Problem::Invalid
                .error("--from-core needs a script payload, but exec is a native binary"));
        }
        if manifest.contains_key("capabilities") {
            return Err(Problem::Invalid.error("--from-core packages cannot declare capabilities"));
        }
        let extension = std::path::Path::new(&exec_path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let actions = crate::supervise::ops::script::declared_actions(
            &String::from_utf8_lossy(&source.data),
            &extension,
        );
        if actions.any() {
            let mut lifecycle = Map::new();
            if actions.start {
                lifecycle.insert(
                    "start".into(),
                    serde_json::json!({"path": exec_path, "args": ["start"]}),
                );
            }
            if actions.stop {
                lifecycle.insert(
                    "stop".into(),
                    serde_json::json!({"path": exec_path, "args": ["stop"]}),
                );
            }
            if actions.status {
                lifecycle.insert(
                    "status".into(),
                    serde_json::json!({"path": exec_path, "args": ["status"]}),
                );
            }
            manifest.insert("lifecycle".into(), Value::Object(lifecycle));
        } else {
            manifest.remove("lifecycle");
        }
        let mut exec = manifest
            .get("exec")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        exec.insert("ready".into(), Value::String("exec".into()));
        manifest.insert("exec".into(), Value::Object(exec));
    }

    if options.interpreter_version.is_some() && options.interpreter.is_none() {
        return Err(Problem::Invalid.error("--interpreter-version requires --interpreter"));
    }
    if !options.interpreter_args.is_empty() && options.interpreter.is_none() {
        return Err(Problem::Invalid.error("--interpreter-arg requires --interpreter"));
    }
    if let Some(name) = &options.interpreter {
        let mut interpreter = manifest
            .get("interpreter")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        interpreter.insert("name".into(), Value::String(name.clone()));
        if let Some(version) = &options.interpreter_version {
            interpreter.insert("version".into(), Value::String(version.clone()));
        }
        if !options.interpreter_args.is_empty() {
            interpreter.insert(
                "args".into(),
                Value::Array(
                    options
                        .interpreter_args
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        manifest.insert("interpreter".into(), Value::Object(interpreter));
    }
    if options.from_core && !manifest.contains_key("interpreter") {
        return Err(Problem::Invalid
            .error("--from-core with a script payload requires --interpreter <name>"));
    }

    if !manifest.contains_key("hooks") {
        let mut hooks = Map::new();
        if sources
            .iter()
            .any(|source| source.path == "hooks/post-install")
        {
            hooks.insert(
                "postInstall".into(),
                Value::String("hooks/post-install".into()),
            );
        }
        if sources
            .iter()
            .any(|source| source.path == "hooks/pre-remove")
        {
            hooks.insert("preRemove".into(), Value::String("hooks/pre-remove".into()));
        }
        if !hooks.is_empty() {
            manifest.insert("hooks".into(), Value::Object(hooks));
        }
    }
    let hook_paths: BTreeSet<String> = manifest
        .get("hooks")
        .and_then(Value::as_object)
        .map(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    #[cfg(unix)]
    for path in &hook_paths {
        if let Some(source) = sources.iter().find(|source| &source.path == path) {
            if !source.data.starts_with(b"#!") {
                return Err(
                    Problem::Invalid.error(format!("hook must start with a shebang: {}", path))
                );
            }
            if source.data.contains(&b'\r') {
                return Err(
                    Problem::Invalid.error(format!("hook must use LF line endings: {}", path))
                );
            }
        }
    }
    let interpreter_runs_exec = manifest.contains_key("interpreter");
    for source in sources.iter_mut() {
        source.mode = effective_mode(
            &source.path,
            source.mode,
            &exec_path,
            &hook_paths,
            interpreter_runs_exec,
        )?;
    }

    let mut files = Vec::with_capacity(sources.len());
    for source in sources.iter() {
        files.push(serde_json::json!({
            "path": source.path,
            "size": source.data.len(),
            "sha256": package::hex_sha256(&source.data),
            "mode": source.mode,
        }));
    }

    // Device compatibility: CLI wins; an authored manifest may still provide it.
    if !options.compat.is_empty() {
        let mut seen = BTreeSet::new();
        let mut devices = Vec::new();
        for device in &options.compat {
            crate::platform::compat::validate_device_id(device).map_err(|error| {
                Problem::Invalid.error(format!("--compat {}: {}", device, error))
            })?;
            if seen.insert(device.clone()) {
                devices.push(device.clone());
            }
        }
        manifest.insert("compat".into(), serde_json::json!({ "devices": devices }));
    }
    let has_kernel_module = sources
        .iter()
        .any(|source| source.path.to_ascii_lowercase().ends_with(".ko"));
    let declared_compat = manifest
        .get("compat")
        .and_then(|value| value.get("devices"))
        .and_then(Value::as_array)
        .map(|items| !items.is_empty())
        .unwrap_or(false);
    if has_kernel_module && !declared_compat {
        return Err(Problem::Invalid.error(
            "the payload contains a kernel module (.ko); pass --compat <device> so it is only \
             installed on the matching firmware generation",
        ));
    }

    // One manifest format: the bundler always writes schema 1, whatever the
    // authored skeleton says. Anything else is not a package this daemon reads.
    manifest.insert(
        "schema".into(),
        Value::from(crate::kernel::schema::MAX_SCHEMA),
    );
    manifest.entry("protocol").or_insert_with(|| Value::from(1));
    manifest.insert("id".into(), Value::String(id));
    manifest.insert("name".into(), Value::String(name));
    manifest.insert("version".into(), Value::String(version));
    manifest.insert("desc".into(), Value::String(desc));
    manifest.insert(
        "platform".into(),
        Value::String(
            options
                .platform
                .clone()
                .unwrap_or_else(|| crate::platform::target().to_string()),
        ),
    );
    let mut exec = manifest
        .get("exec")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    exec.insert("path".into(), Value::String(exec_path));
    manifest.insert("exec".into(), Value::Object(exec));
    manifest.insert("files".into(), Value::Array(files));

    if !manifest.contains_key("config")
        && sources.iter().any(|source| source.path == "config/config")
    {
        manifest.insert("config".into(), Value::String("config/config".into()));
    }
    if !manifest.contains_key("ui")
        && sources
            .iter()
            .any(|source| source.path.starts_with("payload/web/"))
    {
        manifest.insert("ui".into(), serde_json::json!({"type": "web"}));
    }
    let manifest_bytes = serde_json::to_vec(&Value::Object(manifest))
        .map_err(|error| Problem::Invalid.error(format!("serialize manifest: {}", error)))?;
    let parsed = package::parse_manifest(&manifest_bytes)?;
    package::validate_manifest(&parsed)?;
    let borrowed: Vec<(&str, &[u8])> = sources
        .iter()
        .map(|source| (source.path.as_str(), source.data.as_slice()))
        .collect();
    package::validate_script_payload(&parsed, &borrowed)?;
    package::check_icon_magic(&parsed, &borrowed)?;
    Ok((manifest_bytes, parsed))
}

pub(super) fn read_skeleton(dir: &Path) -> Result<Option<Map<String, Value>>, ProblemError> {
    let path = dir.join("manifest.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Problem::Internal.error(format!("stat {}: {}", path.display(), error)))
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(Problem::Invalid.error("symbolic links are not allowed: manifest.json"));
    }
    if !metadata.is_file() {
        return Err(Problem::Invalid.error("manifest.json must be a regular file"));
    }
    let bytes = fs::read(&path)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| Problem::Invalid.error(format!("{}: {}", path.display(), error)))?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => Err(Problem::Invalid.error("manifest.json must contain a JSON object")),
    }
}
