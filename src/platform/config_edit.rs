//! Reading and editing `svcd.conf` from the settings screen (#263).
//!
//! The parser in `settings` turns a document into typed values; this module
//! keeps the document *text*: which keys it states, which environment override
//! wins, and how to change a few keys while preserving every other line.

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

/// The document text after `set` and `unset`, with every line the caller did
/// not name preserved byte for byte — including its line ending and a missing
/// final newline. A duplicate assignment of a set key collapses to one line.
pub fn rewrite_text(
    text: &str,
    set: &std::collections::BTreeMap<String, String>,
    unset: &[String],
) -> String {
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    };
    let newline = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let mut output = String::with_capacity(text.len() + 64);
    output.push_str(bom);
    let mut written = std::collections::BTreeSet::new();
    for segment in body.split_inclusive('\n') {
        let (line, terminator) = if let Some(rest) = segment.strip_suffix("\r\n") {
            (rest, "\r\n")
        } else if let Some(rest) = segment.strip_suffix('\n') {
            (rest, "\n")
        } else {
            (segment, "")
        };
        let trimmed = line.trim();
        let key = (!trimmed.starts_with('#') && !trimmed.is_empty())
            .then(|| trimmed.split_once('=').map(|(left, _)| left.trim()))
            .flatten();
        match key {
            Some(key) if unset.iter().any(|candidate| candidate == key) => {}
            Some(key) => match set.get(key) {
                Some(value) if written.insert(key.to_string()) => {
                    output.push_str(&format!("{} = {}", key, value));
                    output.push_str(terminator);
                }
                Some(_) => {}
                None => {
                    output.push_str(line);
                    output.push_str(terminator);
                }
            },
            None => {
                output.push_str(line);
                output.push_str(terminator);
            }
        }
    }
    for (key, value) in set {
        if written.insert(key.clone()) {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push_str(newline);
            }
            output.push_str(&format!("{} = {}", key, value));
            output.push_str(newline);
        }
    }
    output
}

/// `strip a trailing " # comment"` — the same rule the parser applies.
fn strip_comment(value: &str) -> &str {
    match value.find(" #") {
        Some(index) => value[..index].trim_end(),
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn an_edit_preserves_comments_crlf_and_unknown_keys() {
        let text = "# header\r\nbind = 127.0.0.1:8787\r\n\r\nlog_level = info\r\nother = 1\r\n";

        let next = rewrite_text(text, &map(&[("log_level", "debug")]), &[]);

        assert_eq!(
            next,
            "# header\r\nbind = 127.0.0.1:8787\r\n\r\nlog_level = debug\r\nother = 1\r\n"
        );
    }

    #[test]
    fn an_edit_keeps_a_missing_trailing_newline() {
        let text = "log_level = info";

        let next = rewrite_text(text, &map(&[("log_level", "warn")]), &[]);

        assert_eq!(next, "log_level = warn");
    }

    #[test]
    fn a_new_key_starts_on_its_own_line() {
        let text = "log_level = info";
        let next = rewrite_text(text, &map(&[("bind", "127.0.0.1:0")]), &[]);
        assert_eq!(next, "log_level = info\nbind = 127.0.0.1:0\n");

        let next = rewrite_text("", &map(&[("bind", "127.0.0.1:0")]), &[]);
        assert_eq!(next, "bind = 127.0.0.1:0\n");
    }

    #[test]
    fn an_unset_key_disappears_and_duplicates_collapse() {
        let text = "a = 1\nb = 2\na = 3\n";

        let next = rewrite_text(text, &map(&[("a", "9")]), &["b".to_string()]);

        assert_eq!(next, "a = 9\n");
        assert_eq!(file_values(&next).get("a").map(String::as_str), Some("9"));
        assert!(!file_values(&next).contains_key("b"));
    }

    #[test]
    fn comments_and_blank_lines_are_not_values() {
        let values = file_values("# a = 1\n\nb = 2 # trailing\n");
        assert_eq!(values.len(), 1);
        assert_eq!(values.get("b").map(String::as_str), Some("2"));
    }
}
