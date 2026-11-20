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
    pub name: String,
    pub version: String,
    pub desc: String,
    pub platform: String,
    pub protocol: u32,
    pub exec: Exec,
    /// Host interpreter for script payloads; absent for native executables.
    #[serde(default)]
    pub interpreter: Option<Interpreter>,
    /// Optional start/stop/status actions supervised like a core service.
    #[serde(default)]
    pub lifecycle: Option<Lifecycle>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    pub ui: Option<Ui>,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    pub hooks: Option<Hooks>,
    pub config: Option<String>,
    #[serde(rename = "configCheck")]
    pub config_check: Option<ConfigCheck>,
    /// Device compatibility list (schema 3). Absent means universal for this
    /// platform; present means the host id must be listed.
    #[serde(default)]
    pub compat: Option<CompatSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Exec {
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub ready: Option<String>,
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
