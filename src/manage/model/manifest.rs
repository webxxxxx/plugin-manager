//! `.spk` 清单的类型与默认值。解析与校验留在 `package.rs`（它们按 `Problem` 分类
//! 判定，属于 check 段；随 content 切片再搬）。

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::platform::compat::CompatSpec;

/// Whether a `.spk` must carry a valid manifest signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureCheck {
    /// Normal installs: `signature.json` must be the second entry and verify
    /// against a key in `trust.d`.
    Required,
    /// Host-level insecure mode: a package without `signature.json` is
    /// accepted, and a signature that is present is not verified.
    AllowUnsigned,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// Plugin identity: the directory name, log file name, `/plugin-ui/` segment
    /// and `<id>.<cap>` tool prefix. Structurally optional only so a package
    /// without it fails with a repack hint instead of a bare serde error;
    /// [`parse_manifest`](crate::manage::parse_manifest) rejects it.
    ///
    /// Absent means "not a schema-1 package" — never fall back to `name` or the
    /// directory name.
    #[serde(default)]
    pub id: Option<String>,
    /// Display name. Human-facing only; may be any UTF-8 including Chinese, and
    /// may repeat across plugins. Defaults to `id` when a package omits it.
    pub name: String,
    pub version: String,
    pub desc: String,
    /// Who wrote the plugin: a declared attribution, not a verified identity.
    /// The store shows it next to the signature-derived publisher so a reader
    /// can tell the two apart. Optional so every existing package stays valid.
    #[serde(default)]
    pub author: Option<Author>,
    /// The project's own page, opened from the store detail view.
    #[serde(default)]
    pub homepage: Option<String>,
    /// Where the store fetches the long description from. The description is
    /// deliberately *not* in the manifest: one URL keeps every package small
    /// and lets the text change without a repack. The store renders Markdown
    /// from this address; raw HTML is never executed.
    #[serde(default, rename = "detailUrl")]
    pub detail_url: Option<String>,
    /// SPDX-ish short name (`MIT`, `Apache-2.0`); display only.
    #[serde(default)]
    pub license: Option<String>,
    pub platform: String,
    pub protocol: u32,
    pub exec: Exec,
    /// Host interpreter for script payloads; absent for native executables.
    #[serde(default)]
    pub interpreter: Option<Interpreter>,
    /// Optional start/started/stop/status actions supervised like a core service.
    #[serde(default)]
    pub lifecycle: Option<Lifecycle>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    pub ui: Option<Ui>,
    /// Package icons for desktop shells, packagers and the web storefront
    /// (`png` / `ico` / `icns` / `svg`).
    #[serde(default)]
    pub icons: Option<Icons>,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    pub hooks: Option<Hooks>,
    pub config: Option<String>,
    #[serde(rename = "configCheck")]
    pub config_check: Option<ConfigCheck>,
    /// Device compatibility list. Absent means universal for this platform;
    /// present means the host id must be listed.
    #[serde(default)]
    pub compat: Option<CompatSpec>,
}

impl Manifest {
    /// The plugin id. A manifest that reached this point came through
    /// `parse_manifest`, which rejects a missing `id`; there is deliberately no
    /// fallback to `name` or to the install directory.
    pub fn id(&self) -> &str {
        self.id
            .as_deref()
            .expect("manifest id: parse_manifest rejects a manifest without id")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Exec {
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub ready: Option<String>,
    /// Only with `ready: "port"`: the port the instance must accept on.
    #[serde(default)]
    pub port: Option<u16>,
    /// Only with `ready: "port"`: the address to probe (default 127.0.0.1).
    #[serde(default)]
    pub probe_addr: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Interpreter {
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Numeric constraint such as `>=3.10,<4`; absent/empty means presence only.
    #[serde(default)]
    pub version: Option<String>,
}

/// Plugin lifecycle actions, mirroring a core definition.
#[derive(Debug, Clone, Deserialize)]
pub struct Lifecycle {
    pub start: Option<Hook>,
    /// The post-ready action: runs once the instance is ready and before it is
    /// reported running; a failure fails this start.
    pub started: Option<Hook>,
    pub stop: Option<Hook>,
    pub status: Option<Hook>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hook {
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Timeout in seconds; omitted uses the action default.
    #[serde(default)]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Capability {
    pub name: String,
    pub desc: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
    /// Declarative output contract. Absent = undeclared: the result is passed
    /// through unchanged and no schema is projected. `{}` = declared-any.
    /// Schema 1 only declares and projects it; nothing validates results yet.
    #[serde(rename = "outputSchema", default)]
    pub output_schema: Option<serde_json::Value>,
    /// The one-shot action that answers a call to this capability (#342).
    ///
    /// Only for a plugin with no protocol channel (`exec.ready` = `exec` or
    /// `port`): the host runs the hook once per call, `arguments` on stdin and
    /// the result JSON on stdout. A channel peer leaves it out.
    #[serde(default)]
    pub hook: Option<Hook>,
}

/// Declared author attribution. `url` is optional; the store links it when it
/// is an http(s) address.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Author {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
}

/// Package icons: one package-relative path per format. Unknown keys are
/// rejected so a typo cannot silently ship an icon nobody reads. `svg` is for
/// the web storefront; desktop packagers ignore it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Icons {
    #[serde(default)]
    pub png: Option<String>,
    #[serde(default)]
    pub ico: Option<String>,
    #[serde(default)]
    pub icns: Option<String>,
    #[serde(default)]
    pub svg: Option<String>,
}

impl Icons {
    /// Every declared `(format, path)` pair, in a stable order.
    pub fn entries(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("png", self.png.as_deref()),
            ("ico", self.ico.as_deref()),
            ("icns", self.icns.as_deref()),
            ("svg", self.svg.as_deref()),
        ]
        .into_iter()
        .filter_map(|(format, path)| path.map(|path| (format, path)))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ui {
    #[serde(rename = "type")]
    pub kind: String,
    pub entry: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hooks {
    #[serde(rename = "postInstall")]
    pub post_install: Option<String>,
    #[serde(rename = "preRemove")]
    pub pre_remove: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigCheck {
    pub path: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SignatureFile {
    pub(crate) alg: String,
    #[serde(rename = "keyId")]
    pub(crate) key_id: String,
    pub(crate) sig: String,
}

/// One verified file, in package order.
#[derive(Debug, Clone)]
pub struct FileData {
    pub path: String,
    pub data: Vec<u8>,
    pub mode: u32,
}

#[derive(Debug)]
pub struct Package {
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub signature_bytes: Vec<u8>,
    pub files: Vec<FileData>,
}

/// What `signature.json` says about the package itself. Trust is a separate
/// host fact, so a signature whose key is unknown is reported as `unknown-key`
/// rather than `valid`: claiming validity without having verified anything is
/// exactly the hole a CI gate would fall into. The caller decides what to do.
pub(crate) const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_FILES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInfo {
    /// `valid`, `unknown-key`, `missing`, or `unverified` (malformed signatures
    /// are errors).
    pub state: &'static str,
    pub key_id: Option<String>,
    pub trusted: bool,
}

/// A package that passed its own integrity checks. Host-dependent facts are
/// deliberately absent: the caller compares `platform`/`compat` with its own
/// host state.
#[derive(Debug)]
pub struct Inspection {
    pub package: Package,
    pub signature: SignatureInfo,
}
