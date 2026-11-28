//! Manifest parsing and field validation: schema, protocol, compat, lifecycle,
//! hooks and the paths they point at.

use super::*;

use crate::kernel::registry::valid_id;

/// Icon ceilings: one file per format, and the whole icon set stays small
/// enough that it never competes with the payload budget.
pub(crate) const MAX_ICON_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_ICONS_TOTAL_BYTES: u64 = 2 * 1024 * 1024;

pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, ProblemError> {
    let m: Manifest =
        serde_json::from_slice(bytes).map_err(|error| Problem::Invalid.error(error.to_string()))?;
    check_schema(&m)?;
    check_identity(&m)?;
    if m.protocol == 0 || m.protocol > MAX_PROTOCOL {
        return Err(Problem::Invalid.error(format!("unsupported protocol {}", m.protocol)));
    }
    if !m.platform.contains('-') {
        return Err(Problem::UnsupportedPlatform.error("platform must be a target triple"));
    }
    check_version(&m.version)?;
    check_attribution(&m)?;
    Ok(m)
}

/// Attribution fields the store displays. They are optional, but a present
/// value has to be safe to render and small enough to stay a manifest detail:
/// a URL that is not http(s) becomes a `javascript:` link in the store, and an
/// unbounded string turns a 5 KiB manifest into a payload.
///
/// `detailUrl` is https-only on purpose: it is fetched by the store, so an
/// `http://` address would either be a plaintext dependency or a feature the
/// store silently refuses to use. `homepage` and `author.url` are only opened
/// in a browser tab and may use http.
const MAX_URL_CHARS: usize = 2048;
const MAX_AUTHOR_CHARS: usize = 128;
const MAX_LICENSE_CHARS: usize = 64;

fn check_attribution(manifest: &Manifest) -> Result<(), ProblemError> {
    if let Some(author) = &manifest.author {
        let name = author.name.trim();
        if name.is_empty() || name.chars().count() > MAX_AUTHOR_CHARS {
            return Err(Problem::Invalid.error(format!(
                "author.name must be 1..={MAX_AUTHOR_CHARS} characters"
            )));
        }
        if name.chars().any(char::is_control) {
            return Err(Problem::Invalid.error("author.name must not contain control characters"));
        }
        if let Some(url) = &author.url {
            check_url("author.url", url, false)?;
        }
    }
    if let Some(homepage) = &manifest.homepage {
        check_url("homepage", homepage, false)?;
    }
    if let Some(detail) = &manifest.detail_url {
        check_url("detailUrl", detail, true)?;
    }
    if let Some(license) = &manifest.license {
        let license = license.trim();
        if license.is_empty() || license.chars().count() > MAX_LICENSE_CHARS {
            return Err(Problem::Invalid.error(format!(
                "license must be 1..={MAX_LICENSE_CHARS} characters"
            )));
        }
        if license.chars().any(char::is_control) {
            return Err(Problem::Invalid.error("license must not contain control characters"));
        }
    }
    Ok(())
}

/// An absolute http(s) URL with a host. Not a full RFC parser: the store does
/// the strict parsing and private-address checks before it fetches anything;
/// this only keeps a manifest from carrying a scheme that must never become a
/// link.
fn check_url(field: &str, value: &str, https_only: bool) -> Result<(), ProblemError> {
    let value = value.trim();
    if value.chars().count() > MAX_URL_CHARS {
        return Err(Problem::Invalid.error(format!("{field} exceeds {MAX_URL_CHARS} characters")));
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(
            Problem::Invalid.error(format!("{field} contains whitespace or control characters"))
        );
    }
    let https = value.strip_prefix("https://");
    let http = value.strip_prefix("http://");
    let rest = match (https, http) {
        (Some(rest), _) => rest,
        (None, Some(rest)) if !https_only => rest,
        (None, Some(_)) => {
            return Err(Problem::Invalid.error(format!(
                "{field} must use https (the store only fetches https)"
            )))
        }
        (None, None) => {
            return Err(Problem::Invalid.error(format!("{field} must be an absolute http(s) URL")))
        }
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.contains('@') {
        return Err(Problem::Invalid.error(format!("{field} has no host or carries credentials")));
    }
    Ok(())
}

/// `id` is the plugin identity and `name` is only a display name. A package
/// without `id` predates schema 1 or was hand-assembled; either way the fix is
/// the bundler, so the error says so instead of guessing an id.
fn check_identity(manifest: &Manifest) -> Result<(), ProblemError> {
    let Some(id) = manifest.id.as_deref() else {
        return Err(Problem::Invalid
            .error("schema 1 requires an id; repack with `plugin bundle --id <id>`"));
    };
    if !valid_id(id) {
        return Err(Problem::Invalid.error(format!(
            "invalid plugin id: {} (expected [a-z0-9][a-z0-9_-]{{0,63}})",
            id
        )));
    }
    if id == crate::kernel::registry::RESERVED_ID {
        return Err(Problem::Invalid
            .error("the built-in svc id is reserved and cannot be used as a plugin id"));
    }
    if !valid_display_name(&manifest.name) {
        return Err(Problem::Invalid.error(
            "name is the display name: it must be non-empty, at most 128 characters and free of \
             control characters",
        ));
    }
    Ok(())
}

/// The display name may be any UTF-8 (Chinese included); only emptiness, the
/// length ceiling and control characters are rejected.
pub fn valid_display_name(s: &str) -> bool {
    !s.is_empty() && s.chars().count() <= 128 && !s.chars().any(char::is_control)
}

pub fn valid_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn check_version(value: &str) -> Result<(), ProblemError> {
    if !valid_version(value) {
        return Err(Problem::Invalid.error(format!("version is not semver: {}", value)));
    }
    Ok(())
}

/// Validate every cross-reference a manifest makes, including the hook and
/// capability rules the install path enforces separately. The bundler calls
/// this before writing so authoring mistakes fail early.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), ProblemError> {
    check_schema(manifest)?;
    check_attribution(manifest)?;
    check_ready(manifest)?;
    check_capabilities_with_exec(manifest)?;
    check_compat(manifest)?;
    if manifest.files.is_empty() {
        return Err(Problem::Invalid.error("package has no payload files"));
    }
    check_manifest_paths(manifest)?;
    check_icons(manifest)?;
    check_hooks(manifest)?;
    check_lifecycle(manifest)?;
    check_interpreter(manifest)?;
    if let Some(ui) = &manifest.ui {
        if ui.kind == "web" {
            let entry = ui.entry.as_deref().unwrap_or("index.html");
            let path = format!("payload/web/{}", entry);
            check_path(&path)?;
            if !manifest.files.iter().any(|file| file.path == path) {
                return Err(Problem::Invalid.error(format!("ui entry is not in files[]: {}", path)));
            }
        }
    }
    let mut names = BTreeSet::new();
    for capability in &manifest.capabilities {
        if capability.name.is_empty() {
            return Err(Problem::Invalid.error("capability name must not be empty"));
        }
        if capability.name.starts_with("svc.") {
            return Err(Problem::Invalid.error(format!(
                "capability may not use the svc. prefix: {}",
                capability.name
            )));
        }
        if capability.name == "start" {
            return Err(Problem::Invalid
                .error("capability name \"start\" is reserved for the lifecycle tool"));
        }
        if !names.insert(&capability.name) {
            return Err(
                Problem::Invalid.error(format!("duplicate capability: {}", capability.name))
            );
        }
    }
    Ok(())
}

fn check_schema(manifest: &Manifest) -> Result<(), ProblemError> {
    if manifest.schema != MAX_SCHEMA {
        return Err(Problem::Invalid.error(format!(
            "unsupported manifest schema {}: this daemon reads schema {} only (packages written \
             for the old 1/2/3 line also lack the required `id`); repack with `plugin bundle`",
            manifest.schema, MAX_SCHEMA
        )));
    }
    Ok(())
}

/// Package icons: `icons` names one payload file per format. The size ceilings
/// keep a package from smuggling a 100 MiB bitmap through the icon field; the
/// bytes themselves are magic-checked where the payload is available.
fn check_icons(manifest: &Manifest) -> Result<(), ProblemError> {
    let Some(icons) = &manifest.icons else {
        return Ok(());
    };
    if icons.entries().next().is_none() {
        return Err(
            Problem::Invalid.error("icons must declare at least one of png, ico, icns or svg")
        );
    }
    let mut total = 0u64;
    for (format, path) in icons.entries() {
        check_path(path)?;
        let entry = manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| {
                Problem::Invalid.error(format!("icons.{} is not in files[]: {}", format, path))
            })?;
        if entry.size > MAX_ICON_BYTES {
            return Err(Problem::TooLarge.error(format!(
                "icons.{} is {} bytes; the per-icon ceiling is {}",
                format, entry.size, MAX_ICON_BYTES
            )));
        }
        total += entry.size;
    }
    if total > MAX_ICONS_TOTAL_BYTES {
        return Err(Problem::TooLarge.error(format!(
            "icons total {} bytes; the ceiling is {}",
            total, MAX_ICONS_TOTAL_BYTES
        )));
    }
    Ok(())
}

fn check_compat(manifest: &Manifest) -> Result<(), ProblemError> {
    let kernel_module = manifest
        .files
        .iter()
        .any(|file| file.path.to_ascii_lowercase().ends_with(".ko"));
    match &manifest.compat {
        Some(spec) => {
            if spec.devices.is_empty() {
                return Err(Problem::Invalid.error(
                    "compat.devices must not be empty; omit compat for a universal package",
                ));
            }
            if spec.devices.len() > 64 {
                return Err(Problem::Invalid.error("compat.devices lists more than 64 ids"));
            }
            let mut seen = BTreeSet::new();
            for device in &spec.devices {
                crate::platform::compat::validate_device_id(device).map_err(|error| {
                    Problem::Invalid.error(format!("compat.devices: {}", error))
                })?;
                if !seen.insert(device) {
                    return Err(
                        Problem::Invalid.error(format!("duplicate compat device id: {}", device))
                    );
                }
            }
        }
        None => {
            if kernel_module {
                return Err(Problem::Invalid.error(
                    "the package contains a kernel module (.ko) but declares no compat.devices; \
                     kernel modules only run on the device generation they were built for",
                ));
            }
        }
    }
    Ok(())
}

fn check_ready(manifest: &Manifest) -> Result<(), ProblemError> {
    crate::kernel::registry::ready_from(
        manifest.exec.ready.as_deref(),
        manifest.exec.port,
        manifest.exec.probe_addr.as_deref(),
    )
    .map_err(|error| Problem::Invalid.error(error.to_string()))?;
    Ok(())
}

/// One capability has exactly one delivery path (#342): the protocol channel for
/// a `notify`-ready plugin, a one-shot hook for a plugin with no channel.
///
/// The installer's verify step and the boot-time scan both read a manifest, so
/// this decision lives here once: a second copy drifts, and the two paths start
/// judging the same package differently.
pub(crate) fn check_capability_delivery(manifest: &Manifest) -> Result<(), ProblemError> {
    if manifest.capabilities.is_empty() {
        return Ok(());
    }
    if !no_channel(manifest) {
        if manifest
            .capabilities
            .iter()
            .any(|capability| capability.hook.is_some())
        {
            return Err(Problem::Invalid.error(
                "a capability hook is only for a plugin with no protocol channel; \
                 exec.ready=notify already delivers the call",
            ));
        }
        return Ok(());
    }
    // No channel: every capability has to bring its own action, or it would be
    // listed as a tool that can never answer.
    if let Some(missing) = manifest
        .capabilities
        .iter()
        .find(|capability| capability.hook.is_none())
    {
        return Err(Problem::Invalid.error(format!(
            "exec.ready=exec/port has no protocol channel, so capability {} needs a hook",
            missing.name
        )));
    }
    Ok(())
}

/// The delivery rule plus the parts only a package can be held to: the action
/// exists in `files[]`, its path is well formed, and its timeout is within the
/// host's bounds. The scan cannot check these — it reads an installed tree, not
/// a package — which is why they live beside the rule instead of inside it.
fn check_capabilities_with_exec(manifest: &Manifest) -> Result<(), ProblemError> {
    check_capability_delivery(manifest)?;
    if !no_channel(manifest) {
        return Ok(());
    }
    for capability in &manifest.capabilities {
        let Some(hook) = capability.hook.as_ref() else {
            continue;
        };
        check_path(&hook.path)?;
        if !manifest.files.iter().any(|file| file.path == hook.path) {
            return Err(Problem::Invalid.error(format!(
                "capabilities[{}].hook is not in files[]: {}",
                capability.name, hook.path
            )));
        }
        if let Some(timeout) = hook.timeout {
            if timeout == 0 || timeout > 300 {
                return Err(Problem::Invalid.error(format!(
                    "capabilities[{}].hook.timeout must be between 1 and 300 seconds",
                    capability.name
                )));
            }
        }
    }
    Ok(())
}

fn no_channel(manifest: &Manifest) -> bool {
    matches!(manifest.exec.ready.as_deref(), Some("exec") | Some("port"))
}

fn check_lifecycle(manifest: &Manifest) -> Result<(), ProblemError> {
    let Some(lifecycle) = &manifest.lifecycle else {
        return Ok(());
    };
    let mut declared = false;
    for (name, hook, max_seconds) in [
        ("start", lifecycle.start.as_ref(), 300u64),
        ("started", lifecycle.started.as_ref(), 300u64),
        ("stop", lifecycle.stop.as_ref(), 300u64),
        ("status", lifecycle.status.as_ref(), 30u64),
    ] {
        let Some(hook) = hook else {
            continue;
        };
        declared = true;
        check_path(&hook.path)?;
        if !manifest.files.iter().any(|file| file.path == hook.path) {
            return Err(Problem::Invalid.error(format!(
                "lifecycle.{} is not in files[]: {}",
                name, hook.path
            )));
        }
        if let Some(timeout) = hook.timeout {
            if timeout == 0 || timeout > max_seconds {
                return Err(Problem::Invalid.error(format!(
                    "lifecycle.{}.timeout must be between 1 and {} seconds",
                    name, max_seconds
                )));
            }
        }
        for argument in &hook.args {
            check_argument("lifecycle", name, argument)?;
        }
        for (key, value) in &hook.env {
            check_argument("lifecycle", name, key)?;
            check_argument("lifecycle", name, value)?;
        }
    }
    if !declared {
        return Err(
            Problem::Invalid.error("lifecycle declares no start, started, stop or status action")
        );
    }
    Ok(())
}

fn check_interpreter(manifest: &Manifest) -> Result<(), ProblemError> {
    let Some(interpreter) = &manifest.interpreter else {
        return Ok(());
    };
    if !crate::manage::check::interpreter::valid_name(&interpreter.name) {
        return Err(Problem::Invalid.error(format!(
            "interpreter.name {:?} is not a bare name (no paths or spaces)",
            interpreter.name
        )));
    }
    if let Some(version) = &interpreter.version {
        crate::manage::check::interpreter::validate_constraint(version)
            .map_err(|error| Problem::Invalid.error(format!("interpreter.version: {}", error)))?;
    }
    for argument in &interpreter.args {
        check_argument("interpreter", &interpreter.name, argument)?;
    }
    Ok(())
}

fn check_argument(section: &str, name: &str, value: &str) -> Result<(), ProblemError> {
    if value.contains('\0') {
        return Err(Problem::Invalid.error(format!("{} {} contains a NUL byte", section, name)));
    }
    Ok(())
}

/// `true` when the manifest actually names at least one hook; an empty
/// `"hooks": {}` object is not an install-time hook package.
pub fn declares_hooks(manifest: &Manifest) -> bool {
    manifest
        .hooks
        .as_ref()
        .map(|hooks| hooks.post_install.is_some() || hooks.pre_remove.is_some())
        .unwrap_or(false)
}

/// Hooks are package-signed executables under `hooks/`; the manifest path is
/// resolved inside the installed version directory at run time.
pub(crate) fn check_hooks(manifest: &Manifest) -> Result<(), ProblemError> {
    for (name, path) in [
        (
            "postInstall",
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.post_install.as_deref()),
        ),
        (
            "preRemove",
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.pre_remove.as_deref()),
        ),
    ] {
        let Some(path) = path else { continue };
        check_path(path)?;
        if !path.starts_with("hooks/") {
            return Err(Problem::Invalid.error(format!("{} must be under hooks/: {}", name, path)));
        }
        let file = manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| {
                Problem::Invalid.error(format!("{} is not in files[]: {}", name, path))
            })?;
        check_hook_executable(name, path, file.mode)?;
    }
    Ok(())
}

fn check_hook_executable(name: &str, path: &str, mode: u32) -> Result<(), ProblemError> {
    #[cfg(unix)]
    {
        if mode & 0o111 == 0 {
            return Err(Problem::Invalid.error(format!("{} must be executable: {}", name, path)));
        }
    }
    #[cfg(not(unix))]
    {
        // Windows has no exec bit; the hook runner rejects a file that is not
        // .exe/.cmd/.bat when it actually runs it.
        let _ = (name, path, mode);
    }
    Ok(())
}

pub(crate) fn check_manifest_paths(manifest: &Manifest) -> Result<(), ProblemError> {
    check_path(&manifest.exec.path)?;
    if !manifest
        .files
        .iter()
        .any(|file| file.path == manifest.exec.path)
    {
        return Err(Problem::Invalid.error(format!(
            "exec.path is not in files[]: {}",
            manifest.exec.path
        )));
    }
    if let Some(config) = &manifest.config {
        check_path(config)?;
        if !manifest.files.iter().any(|file| &file.path == config) {
            return Err(Problem::Invalid.error(format!("config is not in files[]: {}", config)));
        }
    }
    if let Some(check) = &manifest.config_check {
        check_path(&check.path)?;
        if !manifest.files.iter().any(|file| file.path == check.path) {
            return Err(Problem::Invalid.error(format!(
                "configCheck.path is not in files[]: {}",
                check.path
            )));
        }
        let config = manifest.config.as_deref().unwrap_or("config/config");
        if !manifest.files.iter().any(|file| file.path == config) {
            return Err(Problem::Invalid.error("configCheck is declared but config is missing"));
        }
    }
    if let Some(ui) = &manifest.ui {
        if let Some(entry) = &ui.entry {
            check_path(entry)?;
        }
    }
    Ok(())
}

/// The host-platform gate. `Inspect` deliberately skips it: a package built
/// for another target is still well-formed, and the caller gets
/// `host.matchesHost` as a fact.
pub(crate) fn check_platform(manifest: &Manifest) -> Result<(), ProblemError> {
    if manifest.platform != crate::platform::target() {
        return Err(Problem::UnsupportedPlatform.error(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            crate::platform::target()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn manifest_with(extra: Value) -> Result<Manifest, ProblemError> {
        let mut manifest = json!({
            "schema": 1,
            "id": "demo",
            "name": "Demo",
            "version": "1.0.0",
            "desc": "a demo",
            "platform": "x86_64-unknown-linux-musl",
            "protocol": 2,
            "exec": {"path": "payload/bin/demo"},
            "files": [{"path": "payload/bin/demo", "size": 3, "sha256": "00", "mode": 493}]
        });
        manifest
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        parse_manifest(&serde_json::to_vec(&manifest).expect("json"))
    }

    #[test]
    fn attribution_fields_are_optional_and_accepted() {
        let parsed = manifest_with(json!({
            "author": {"name": "张三", "url": "https://example.com/zhang"},
            "homepage": "https://example.com/meow",
            "detailUrl": "https://example.com/meow/docs",
            "license": "MIT"
        }))
        .expect("attribution fields parse");
        assert_eq!(parsed.author.expect("author").name, "张三");
        assert_eq!(
            parsed.detail_url.as_deref(),
            Some("https://example.com/meow/docs")
        );
        assert_eq!(parsed.license.as_deref(), Some("MIT"));

        // No attribution at all is still a valid package.
        manifest_with(json!({})).expect("absent attribution parses");
    }

    #[test]
    fn detail_url_must_be_https() {
        let error = manifest_with(json!({"detailUrl": "http://example.com/meow.md"}))
            .expect_err("plain http detail is rejected");
        assert!(
            error.to_string().contains("detailUrl must use https"),
            "{error}"
        );

        let error = manifest_with(json!({"detailUrl": "example.com/meow.md"}))
            .expect_err("relative detail is rejected");
        assert!(
            error.to_string().contains("absolute http(s) URL"),
            "{error}"
        );
    }

    #[test]
    fn author_name_is_bounded_and_control_free() {
        let error = manifest_with(json!({"author": {"name": "   "}}))
            .expect_err("blank author is rejected");
        assert!(error.to_string().contains("author.name"), "{error}");

        let long = "x".repeat(MAX_AUTHOR_CHARS + 1);
        let error =
            manifest_with(json!({"author": {"name": long}})).expect_err("long author is rejected");
        assert!(error.to_string().contains("author.name"), "{error}");

        let error = manifest_with(json!({"author": {"name": "a\nb"}}))
            .expect_err("control characters are rejected");
        assert!(error.to_string().contains("control"), "{error}");
    }

    #[test]
    fn homepage_and_author_urls_must_be_absolute_http_s() {
        for field in ["homepage", "author"] {
            let extra = if field == "homepage" {
                json!({"homepage": "javascript:alert(1)"})
            } else {
                json!({"author": {"name": "a", "url": "javascript:alert(1)"}})
            };
            let error = manifest_with(extra).expect_err("javascript URL is rejected");
            assert!(
                error.to_string().contains("absolute http(s) URL"),
                "{error}"
            );
        }

        // http is allowed for links a browser only opens.
        manifest_with(json!({"homepage": "http://example.com/"}))
            .expect("http homepage is accepted");
    }

    #[test]
    fn license_is_bounded() {
        let error = manifest_with(json!({"license": "x".repeat(MAX_LICENSE_CHARS + 1)}))
            .expect_err("long license is rejected");
        assert!(error.to_string().contains("license"), "{error}");
        manifest_with(json!({"license": "Apache-2.0"})).expect("license parses");
    }

    #[test]
    fn an_svg_icon_is_a_valid_package_icon() {
        let parsed = manifest_with(json!({
            "icons": {"svg": "payload/web/icon.svg"},
            "files": [
                {"path": "payload/bin/demo", "size": 3, "sha256": "00", "mode": 493},
                {"path": "payload/web/icon.svg", "size": 3, "sha256": "00", "mode": 420}
            ]
        }))
        .expect("parse");
        validate_manifest(&parsed).expect("svg icon validates");
    }
}
