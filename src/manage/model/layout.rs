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
