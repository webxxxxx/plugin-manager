//! Value parsers shared by the `svcd.conf` reader and the console's editors.
//!
//! The document is line-oriented `key = value`; these helpers know the value
//! shapes (bools, byte sizes, numbers) and the trailing-comment rule, so the
//! parser and the console's "edit one key" path agree on what a value is.

/// A trailing `# ...` comment is allowed after a value when the hash is
/// preceded by whitespace. `svc config tune` writes the derivation on the same
/// line, and a hand-edited file may do the same.
pub(super) fn strip_comment(value: &str) -> &str {
    match value.find(" #") {
        Some(index) => value[..index].trim_end(),
        None => value,
    }
}

pub(super) fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Some(true),
        "false" | "0" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Parse a byte size the way an operator writes it: digits, or a count with a
/// binary suffix (`K`/`M`/`G`, optionally `iB`), case-insensitive and with or
/// without a space. The document itself only ever holds digits; this is the
/// console's convenient form for byte fields (#290).
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub fn parse_byte_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits_end = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());
    if digits_end == 0 {
        return None;
    }
    let count: u64 = text[..digits_end].parse().ok()?;
    let multiplier = match text[digits_end..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => return None,
    };
    count.checked_mul(multiplier)
}

pub(super) fn parse_u64(value: &str, key: &str, index: usize) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("line {}: invalid {} {:?}", index + 1, key, value))
}

pub(super) fn parse_usize(value: &str, key: &str, index: usize) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("line {}: invalid {} {:?}", index + 1, key, value))
}
