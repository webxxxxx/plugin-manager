use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub name: String,
    pub version: String,
    pub desc: String,
    pub platform: String,
    pub protocol: u32,
    pub exec: Exec,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    pub ui: Option<Ui>,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    pub hooks: Option<Hooks>,
}

#[derive(Debug, Deserialize)]
pub struct Exec {
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub ready: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Capability {
    pub name: String,
    pub desc: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct Ui {
    #[serde(rename = "type")]
    pub kind: String,
    pub entry: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Debug, Deserialize)]
pub struct Hooks {
    #[serde(rename = "postInstall")]
    pub post_install: Option<String>,
    #[serde(rename = "preRemove")]
    pub pre_remove: Option<String>,
}

pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    let m: Manifest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if m.schema != 1 {
        return Err(format!("unsupported schema {}", m.schema));
    }
    if m.protocol != 1 {
        return Err(format!("unsupported protocol {}", m.protocol));
    }
    if !valid_name(&m.name) {
        return Err("invalid service id".into());
    }
    if !m.platform.contains('-') {
        return Err("platform must be a target triple".into());
    }
    Ok(m)
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().next().map(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn check_path(p: &str) -> Result<(), String> {
    if p.starts_with('/') || p.contains('\\') {
        return Err(format!("unsafe path: {}", p));
    }
    if p.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return Err(format!("unsafe path: {}", p));
    }
    Ok(())
}

pub fn parse_ustar_header(hdr: &[u8; 512]) -> Result<(String, usize, u32), String> {
    let name = field(hdr, 0, 100);
    let mode = octal(field(hdr, 100, 8)) as u32;
    let size = octal(field(hdr, 124, 12));
    let typ = hdr[156];
    if typ != b'0' {
        return Err("only regular files are allowed".into());
    }
    check_path(&name)?;
    Ok((name, size, mode))
}

fn field(hdr: &[u8; 512], start: usize, len: usize) -> String {
    let slice = &hdr[start..start + len];
    let end = slice.iter().position(|b| *b == 0).unwrap_or(len);
    String::from_utf8_lossy(&slice[..end]).to_string()
}

fn octal(s: String) -> usize {
    usize::from_str_radix(s.trim_matches(char::from(0)).trim(), 8).unwrap_or(0)
}
