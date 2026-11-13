//! Runtime settings loaded from `<config>/svcd.conf`.
//!
//! The file is a small `key = value` document written by
//! `svcd --install --conf <file>` and read at daemon startup. Values pair with
//! the existing environment overrides: `SVCD_HTTP_PORT` still wins for the HTTP
//! port, while explicit paths in `svcd.conf` win over the platform defaults.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub http_port: Option<u16>,
    pub lan_default: bool,
    pub plugins_dir: Option<PathBuf>,
    pub services_dir: Option<PathBuf>,
}

/// The settings for this process. Loaded once from `<config>/svcd.conf`.
pub fn current() -> Result<&'static Settings, String> {
    static SETTINGS: OnceLock<Result<Settings, String>> = OnceLock::new();
    SETTINGS
        .get_or_init(|| load(&crate::paths::config_dir()))
        .as_ref()
        .map_err(Clone::clone)
}

fn load(config_dir: &Path) -> Result<Settings, String> {
    let path = config_dir.join("svcd.conf");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default())
        }
        Err(error) => return Err(format!("read {}: {}", path.display(), error)),
    };
    parse(&text, config_dir).map_err(|error| format!("{}: {}", path.display(), error))
}

pub fn parse(text: &str, base: &Path) -> Result<Settings, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut settings = Settings::default();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected key = value", index + 1))?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            return Err(format!("line {}: empty key", index + 1));
        }
        if value.is_empty() {
            return Err(format!("line {}: empty value for {}", index + 1, key));
        }
        match key {
            "http_port" => {
                settings.http_port =
                    Some(value.parse::<u16>().map_err(|_| {
                        format!("line {}: invalid http_port {:?}", index + 1, value)
                    })?);
            }
            "lan_default" => {
                settings.lan_default = match value.to_ascii_lowercase().as_str() {
                    "true" | "1" | "on" | "yes" => true,
                    "false" | "0" | "off" | "no" => false,
                    _ => {
                        return Err(format!(
                            "line {}: invalid lan_default {:?}",
                            index + 1,
                            value
                        ))
                    }
                };
            }
            "plugins_dir" => settings.plugins_dir = Some(resolve(base, value)),
            "services_dir" => settings.services_dir = Some(resolve(base, value)),
            other => return Err(format!("line {}: unknown key {:?}", index + 1, other)),
        }
    }
    Ok(settings)
}

fn resolve(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_keys_and_resolves_relative_paths() {
        let settings = parse(
            "\u{feff}# svcd runtime\nhttp_port = 8787\nlan_default = yes\nplugins_dir = plugins\nservices_dir = /var/lib/svcd/services.d\n",
            Path::new("/etc/svcd"),
        )
        .expect("parse");
        assert_eq!(settings.http_port, Some(8787));
        assert!(settings.lan_default);
        assert_eq!(
            settings.plugins_dir,
            Some(PathBuf::from("/etc/svcd/plugins"))
        );
        assert_eq!(
            settings.services_dir,
            Some(PathBuf::from("/var/lib/svcd/services.d"))
        );
    }

    #[test]
    fn false_spellings_are_false() {
        for value in ["false", "0", "off", "no", "FALSE"] {
            let settings =
                parse(&format!("lan_default = {value}\n"), Path::new("/etc/svcd")).expect("parse");
            assert!(!settings.lan_default, "{value}");
        }
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let settings =
            parse("\n# comment\n   \nhttp_port=0\n", Path::new("/etc/svcd")).expect("parse");
        assert_eq!(settings.http_port, Some(0));
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        for text in [
            "unknown = 1\n",
            "http_port = 70000\n",
            "http_port = nope\n",
            "lan_default = maybe\n",
            "http_port 8787\n",
            "= 1\n",
        ] {
            assert!(parse(text, Path::new("/etc/svcd")).is_err(), "{text:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_absolute_paths_are_not_joined_again() {
        let settings =
            parse("plugins_dir = C:\\svcd\\plugins\n", Path::new("C:\\config")).expect("parse");
        assert_eq!(
            settings.plugins_dir,
            Some(PathBuf::from(r"C:\svcd\plugins"))
        );
    }
}
