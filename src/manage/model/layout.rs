//! 插件在磁盘上的布局：版本目录、`current` 指针与 Web 资产入口。

use std::fs;
use std::path::{Path, PathBuf};

use crate::kernel::layout::plugin_dir;
#[cfg(feature = "http")]
use crate::manage::ops::scan::read_manifest;
use crate::platform::paths::Paths;

/// Where an installed plugin's declared web UI lives on disk.
#[derive(Clone, Debug)]
#[cfg(feature = "http")]
pub struct WebAssets {
    /// `payload/web/` inside the current version directory.
    pub root: PathBuf,
    /// Entry file relative to `root`; defaults to `index.html`.
    pub entry: String,
}

pub fn current_version(paths: &Paths, id: &str) -> Option<String> {
    current_version_at(&plugin_dir(paths, id))
}

/// The `current` pointer of one plugin directory.
///
/// Scanning already holds the directory it walked (`<root>/<id>` for any
/// root), so it must not re-derive the path from the default root.
pub(crate) fn current_version_at(directory: &Path) -> Option<String> {
    let text = fs::read_to_string(directory.join("current")).ok()?;
    let version = text.trim();
    if version.is_empty()
        || version.split('.').count() != 3
        || version
            .split('.')
            .any(|part| part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()))
    {
        None
    } else {
        Some(version.to_string())
    }
}

pub fn active_config(paths: &Paths, id: &str) -> Option<PathBuf> {
    let path = plugin_dir(paths, id).join("config");
    path.is_file().then_some(path)
}

/// Resolve the web UI declared by `manifest.ui`.
///
/// Only `web` and `gui` UIs are browser-loadable (the desktop WebView loads the
/// same files); `headerless` and unknown types have no UI to serve. The entry is
/// relative to `payload/web/` and defaults to `index.html`.
#[cfg(feature = "http")]
pub fn web_assets(paths: &Paths, id: &str) -> Option<WebAssets> {
    let version = current_version(paths, id)?;
    let version_dir = plugin_dir(paths, id).join(version);
    let manifest = read_manifest(&version_dir).ok()?;
    let ui = manifest.ui.as_ref()?;
    if ui.kind != "web" && ui.kind != "gui" {
        return None;
    }
    Some(WebAssets {
        root: version_dir.join("payload/web"),
        entry: ui.entry.clone().unwrap_or_else(|| "index.html".to_string()),
    })
}

/// Where an installed plugin's browser-readable icon lives on disk.
#[derive(Clone, Debug)]
#[cfg(feature = "http")]
pub struct IconAsset {
    pub path: PathBuf,
    pub mime: &'static str,
}

/// Resolve the icon declared by `manifest.icons` for the console list.
///
/// Preference is svg, then png, then ico: the three formats a browser renders.
/// `icns` is a desktop packager format and is skipped. The declared path is
/// checked against the package rules before it is joined, so a tampered
/// installed manifest cannot point this route at an arbitrary file.
#[cfg(feature = "http")]
pub fn icon_asset(paths: &Paths, id: &str) -> Option<IconAsset> {
    let version = current_version(paths, id)?;
    let version_dir = plugin_dir(paths, id).join(version);
    let manifest = read_manifest(&version_dir).ok()?;
    let icons = manifest.icons.as_ref()?;
    let (relative, mime) = [
        (icons.svg.as_deref(), "image/svg+xml"),
        (icons.png.as_deref(), "image/png"),
        (icons.ico.as_deref(), "image/x-icon"),
    ]
    .into_iter()
    .find_map(|(path, mime)| path.map(|path| (path, mime)))?;
    if crate::kernel::layout::check_path(relative).is_err() {
        return None;
    }
    let path = version_dir.join(relative);
    path.is_file().then_some(IconAsset { path, mime })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_support::TempDir;

    /// Install a minimal `demo` version directory under a fresh root.
    fn installed(temp: &TempDir, icons: Option<serde_json::Value>) -> Paths {
        let paths = Paths::under(temp.path());
        let directory = paths.default_plugin_root().join("demo");
        let version_dir = directory.join("1.0.0");
        std::fs::create_dir_all(version_dir.join("payload/icons")).expect("icon dir");
        for (name, body) in [
            ("icon.svg", b"<svg></svg>".as_slice()),
            ("icon.png", b"\x89PNG\r\n\x1a\n".as_slice()),
            ("icon.icns", b"icns".as_slice()),
        ] {
            std::fs::write(version_dir.join("payload/icons").join(name), body).expect("icon");
        }
        let mut manifest = serde_json::json!({
            "schema": 1,
            "id": "demo",
            "name": "demo",
            "version": "1.0.0",
            "desc": "icon test",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/demo", "ready": "exec"}
        });
        if let Some(icons) = icons {
            manifest["icons"] = icons;
        }
        std::fs::write(version_dir.join("manifest.json"), manifest.to_string()).expect("manifest");
        std::fs::write(directory.join("current"), b"1.0.0\n").expect("current");
        paths
    }

    #[test]
    fn the_icon_prefers_svg_then_png_and_skips_icns() {
        let temp = TempDir::with_prefix("svcd-icon");
        let paths = installed(
            &temp,
            Some(serde_json::json!({
                "icns": "payload/icons/icon.icns",
                "png": "payload/icons/icon.png",
                "svg": "payload/icons/icon.svg"
            })),
        );
        let icon = icon_asset(&paths, "demo").expect("icon");
        assert_eq!(icon.mime, "image/svg+xml");
        assert!(icon.path.ends_with("payload/icons/icon.svg"));
    }

    #[test]
    fn png_is_the_fallback_when_no_svg_is_declared() {
        let temp = TempDir::with_prefix("svcd-icon");
        let paths = installed(
            &temp,
            Some(serde_json::json!({"png": "payload/icons/icon.png"})),
        );
        let icon = icon_asset(&paths, "demo").expect("icon");
        assert_eq!(icon.mime, "image/png");
    }

    #[test]
    fn a_desktop_only_icon_is_not_served_to_the_browser() {
        let temp = TempDir::with_prefix("svcd-icon");
        let paths = installed(
            &temp,
            Some(serde_json::json!({"icns": "payload/icons/icon.icns"})),
        );
        assert!(icon_asset(&paths, "demo").is_none());
    }

    #[test]
    fn an_icon_path_must_stay_inside_the_package() {
        let temp = TempDir::with_prefix("svcd-icon");
        let paths = installed(&temp, Some(serde_json::json!({"svg": "../outside.svg"})));
        assert!(icon_asset(&paths, "demo").is_none());
    }

    #[test]
    fn a_missing_icon_file_is_not_advertised() {
        let temp = TempDir::with_prefix("svcd-icon");
        let paths = installed(
            &temp,
            Some(serde_json::json!({"svg": "payload/icons/missing.svg"})),
        );
        assert!(icon_asset(&paths, "demo").is_none());
    }

    #[test]
    fn a_plugin_without_icons_has_none() {
        let temp = TempDir::with_prefix("svcd-icon");
        let paths = installed(&temp, None);
        assert!(icon_asset(&paths, "demo").is_none());
    }
}
