//! Reading and editing `svcd.conf` from the settings screen (#263).
//!
//! The parser in `settings` turns a document into typed values; this module
//! keeps the document *text*: which keys it states, which environment override
//! wins, and how to change a few keys while preserving every other line.

use std::fs;
use std::path::Path;

use crate::platform::settings;

/// The raw `key = value` pairs a document states, comments stripped. The
/// settings screen needs the text (and where it came from) rather than the
/// parsed [`Settings`], which cannot say whether a value was written or defaulted.
pub fn file_values(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut values = std::collections::BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = strip_comment(value.trim());
        if key.is_empty() || value.is_empty() {
            continue;
        }
        values.insert(key.to_string(), value.to_string());
    }
    values
}

/// The `SVCD_<KEY>` override for a live key, if the environment sets one. Only
/// [`ENV_KEYS`] are consulted, so a stray `SVCD_*` variable stays unknown.
pub fn env_value(key: &str) -> Option<String> {
    if !settings::ENV_KEYS.contains(&key) {
        return None;
    }
    let name = format!("SVCD_{}", key.to_ascii_uppercase());
    let value = std::env::var(&name).ok()?;
    (!value.is_empty()).then_some(value)
}

/// Set and unset several keys in one read-modify-write. Comment lines and keys
/// the caller did not name are preserved byte for byte, which is what makes the
/// settings screen safe to use on a hand-edited document.
pub fn rewrite(
    path: &Path,
    set: &std::collections::BTreeMap<String, String>,
    unset: &[String],
) -> Result<(), String> {
    // A missing document is empty, not an error: the settings screen may be the
    // first thing to create it.
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("read {}: {}", path.display(), error)),
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut output = String::with_capacity(text.len() + 64);
    let mut written = std::collections::BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let key = (!trimmed.starts_with('#') && !trimmed.is_empty())
            .then(|| trimmed.split_once('=').map(|(left, _)| left.trim()))
            .flatten();
        match key {
            Some(key) if unset.iter().any(|candidate| candidate == key) => {}
            Some(key) => match set.get(key) {
                // A duplicate assignment is dropped: one key, one line.
                Some(value) if written.insert(key.to_string()) => {
                    output.push_str(&format!("{} = {}\n", key, value));
                }
                Some(_) => {}
                None => {
                    output.push_str(line);
                    output.push('\n');
                }
            },
            None => {
                output.push_str(line);
                output.push('\n');
            }
        }
    }
    for (key, value) in set {
        if written.insert(key.clone()) {
            output.push_str(&format!("{} = {}\n", key, value));
        }
    }
    fs::write(path, output).map_err(|error| format!("write {}: {}", path.display(), error))?;
    Ok(())
}

/// `strip a trailing " # comment"` — the same rule the parser applies.
fn strip_comment(value: &str) -> &str {
    match value.find(" #") {
        Some(index) => value[..index].trim_end(),
        None => value,
    }
}
