//! Host interpreter dependencies declared by `.spk` manifests.
//!
//! A script payload is not self-contained: the manifest names the interpreter
//! (`python3`, `bash`, `pwsh`, ...) and optionally a version constraint. The
//! daemon resolves the name on `PATH` and, when a constraint is present, runs
//! the interpreter's version probe. The resolved absolute path is what the
//! supervisor later executes, so the validated binary is the one the service
//! runs.

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Interpreter reference from the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    /// Flags placed before the script path, e.g. `py -3` or `cmd /C`.
    pub args: Vec<String>,
    /// Comma-separated numeric constraints; `None` or empty means presence only.
    pub version: Option<String>,
}

/// Absolute interpreter plus the flags placed before the script path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub args: Vec<String>,
}

/// A dotted numeric interpreter version. Interpreters do not all follow semver,
/// so this is only an ordered tuple; suffixes such as `-release` or `rc1` are
/// ignored by the parser.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(Vec<u32>);

/// Resolve and validate one manifest interpreter against the current host.
pub fn check(spec: &Spec) -> Result<Resolved, String> {
    let (path_var, pathext) = host_search();
    check_with(spec, &path_var, pathext.as_deref(), 5_000)
}

fn host_search() -> (String, Option<String>) {
    #[cfg(windows)]
    let pathext = std::env::var("PATHEXT").ok();
    #[cfg(not(windows))]
    let pathext: Option<String> = None;
    (std::env::var("PATH").unwrap_or_default(), pathext)
}

/// Testable form of [`check`]: the search path and probe timeout are injected so
/// tests do not mutate process-wide environment variables.
pub fn check_with(
    spec: &Spec,
    path_var: &str,
    pathext: Option<&str>,
    timeout_ms: u64,
) -> Result<Resolved, String> {
    let constraint = spec
        .version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(constraint) = constraint {
        validate_constraint(constraint)
            .map_err(|error| format!("interpreter {} version: {}", spec.name, error))?;
    }
    let resolved = lookup_with(spec, path_var, pathext)?;
    if let Some(constraint) = constraint {
        let actual = probe(&resolved.path, timeout_ms)?;
        if !satisfies(&actual, constraint)? {
            return Err(format!(
                "interpreter {} is {} at {}, but the manifest requires {}",
                spec.name,
                actual,
                resolved.path.display(),
                constraint
            ));
        }
    }
    Ok(resolved)
}

/// Presence-only lookup for plugin scans: a daemon restart must not run every
/// interpreter's version probe, but it still needs the absolute path.
pub fn lookup(spec: &Spec) -> Result<Resolved, String> {
    let (path_var, pathext) = host_search();
    lookup_with(spec, &path_var, pathext.as_deref())
}

fn lookup_with(spec: &Spec, path_var: &str, pathext: Option<&str>) -> Result<Resolved, String> {
    let path = resolve(&spec.name, path_var, pathext)?;
    Ok(Resolved {
        path,
        args: spec.args.clone(),
    })
}

/// An interpreter is a bare executable name; paths and whitespace are rejected
/// so a manifest cannot point at an arbitrary binary.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Validate a version constraint without resolving anything. Empty is valid and
/// means "presence only".
pub fn validate_constraint(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Ok(());
    }
    parse_clauses(value).map(|_| ())
}

/// Find a bare executable name on the injected search path.
pub fn resolve(name: &str, path_var: &str, pathext: Option<&str>) -> Result<PathBuf, String> {
    if !valid_name(name) {
        return Err(format!(
            "interpreter {:?} is not a bare name (no paths or spaces)",
            name
        ));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        let extensions: Vec<String> = pathext
            .unwrap_or(".COM;.EXE;.BAT;.CMD")
            .split(';')
            .map(str::trim)
            .filter(|extension| !extension.is_empty())
            .map(|extension| extension.to_ascii_lowercase())
            .collect();
        let lower = name.to_ascii_lowercase();
        let has_known_extension = extensions
            .iter()
            .any(|extension| lower.ends_with(extension.as_str()));
        for directory in path_var.split(';').filter(|entry| !entry.is_empty()) {
            let directory = Path::new(directory);
            candidates.push(directory.join(name));
            if !has_known_extension {
                for extension in &extensions {
                    candidates.push(directory.join(format!("{}{}", name, extension)));
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pathext;
        for directory in path_var.split(':').filter(|entry| !entry.is_empty()) {
            candidates.push(Path::new(directory).join(name));
        }
    }
    for candidate in candidates {
        if is_executable(&candidate) {
            return std::path::absolute(&candidate)
                .map_err(|error| format!("interpreter {}: {}", candidate.display(), error));
        }
    }
    Err(format!("interpreter {} not found on PATH", name))
}

/// Extract the first `x.y[.z...]` version from probe output.
pub fn parse_version(text: &str) -> Option<Version> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let mut parts = Vec::new();
        let mut cursor = index;
        loop {
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            let part = std::str::from_utf8(&bytes[start..cursor])
                .ok()?
                .parse::<u32>()
                .ok()?;
            parts.push(part);
            if cursor + 1 < bytes.len()
                && bytes[cursor] == b'.'
                && bytes[cursor + 1].is_ascii_digit()
            {
                cursor += 1;
            } else {
                break;
            }
        }
        if parts.len() >= 2 {
            return Some(Version(parts));
        }
        index = cursor.max(index + 1);
    }
    None
}

/// Check a probed version against a comma-separated constraint list.
pub fn satisfies(version: &Version, constraint: &str) -> Result<bool, String> {
    let clauses = parse_clauses(constraint)?;
    Ok(clauses
        .iter()
        .all(|(op, expected)| matches_operator(compare(version, expected), *op)))
}

fn matches_operator(ordering: Ordering, op: Op) -> bool {
    match op {
        Op::Ge => ordering != Ordering::Less,
        Op::Gt => ordering == Ordering::Greater,
        Op::Le => ordering != Ordering::Greater,
        Op::Lt => ordering == Ordering::Less,
        Op::Eq => ordering == Ordering::Equal,
    }
}

fn compare(left: &Version, right: &Version) -> Ordering {
    let length = left.0.len().max(right.0.len());
    for index in 0..length {
        let left = left.0.get(index).copied().unwrap_or(0);
        let right = right.0.get(index).copied().unwrap_or(0);
        match left.cmp(&right) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
}

fn parse_clauses(value: &str) -> Result<Vec<(Op, Version)>, String> {
    let mut clauses = Vec::new();
    for clause in value.split(',') {
        let clause = clause.trim();
        if clause.is_empty() {
            return Err(format!("empty clause in version constraint {:?}", value));
        }
        let (op, rest) = if let Some(rest) = clause.strip_prefix(">=") {
            (Op::Ge, rest)
        } else if let Some(rest) = clause.strip_prefix("<=") {
            (Op::Le, rest)
        } else if let Some(rest) = clause.strip_prefix('>') {
            (Op::Gt, rest)
        } else if let Some(rest) = clause.strip_prefix('<') {
            (Op::Lt, rest)
        } else if let Some(rest) = clause.strip_prefix('=') {
            (Op::Eq, rest)
        } else {
            return Err(format!(
                "version clause {:?} must start with >=, <=, >, < or =",
                clause
            ));
        };
        let version = parse_dotted(rest.trim())
            .ok_or_else(|| format!("invalid version in clause {:?}", clause))?;
        clauses.push((op, version));
    }
    if clauses.is_empty() {
        return Err("empty version constraint".into());
    }
    Ok(clauses)
}

fn parse_dotted(value: &str) -> Option<Version> {
    if value.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for part in value.split('.') {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        parts.push(part.parse::<u32>().ok()?);
    }
    Some(Version(parts))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        // Interpreter probes and service spawns must not depend on a shell.
        matches!(
            path.extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| extension.to_ascii_lowercase())
                .as_deref(),
            Some("exe" | "com")
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self
            .0
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".");
        formatter.write_str(&text)
    }
}

/// Run the interpreter's version probe with a hard timeout.
fn probe(path: &Path, timeout_ms: u64) -> Result<Version, String> {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let args: Vec<&str> = match name.as_str() {
        "powershell" => vec![
            "-NoProfile",
            "-Command",
            "$PSVersionTable.PSVersion.ToString()",
        ],
        "cmd" => vec!["/C", "ver"],
        _ => vec!["--version"],
    };

    let mut command = Command::new(path);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    for key in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run {} --version: {}", path.display(), error))?;

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} --version: {}", path.display(), error));
            }
        }
        if started.elapsed() >= Duration::from_millis(timeout_ms) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "interpreter {} version probe timed out after {}ms",
                path.display(),
                timeout_ms
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    use std::io::Read;
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let output = format!("{}\n{}", stdout, stderr);
    let version = parse_version(&output).ok_or_else(|| {
        format!(
            "cannot determine the version of {} ({}): {}",
            path.display(),
            status,
            output.trim()
        )
    })?;
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_first_dotted_version() {
        assert_eq!(
            parse_version("Python 3.11.4"),
            Some(Version(vec![3, 11, 4]))
        );
        assert_eq!(parse_version("v20.11.0"), Some(Version(vec![20, 11, 0])));
        assert_eq!(
            parse_version("GNU bash, version 5.2.15(1)-release"),
            Some(Version(vec![5, 2, 15]))
        );
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("2026"), None);
    }

    #[test]
    fn compares_numeric_versions_with_padding() {
        let version = Version(vec![3, 10]);
        assert!(satisfies(&version, ">=3.10").expect("valid"));
        assert!(satisfies(&version, ">=3.10.0").expect("valid"));
        assert!(satisfies(&version, "<=3.10.0").expect("valid"));
        assert!(satisfies(&version, "=3.10").expect("valid"));
        assert!(!satisfies(&version, ">3.10").expect("valid"));
        assert!(!satisfies(&version, "<3.10").expect("valid"));
        assert!(!satisfies(&version, ">=3.11").expect("valid"));
    }

    #[test]
    fn parses_comma_separated_constraints() {
        let version = Version(vec![3, 11, 4]);
        assert!(satisfies(&version, ">=3.10,<4").expect("valid"));
        assert!(!satisfies(&version, ">=3.10,<3.11").expect("valid"));
        assert!(validate_constraint(">=3.10, <4").is_ok());
        assert!(validate_constraint("").is_ok());
    }

    #[test]
    fn rejects_bad_constraints() {
        for bad in [
            "3.10",
            ">=",
            ">=x",
            ">=3.10,",
            ">=3.10,,<4",
            "~3.10",
            "^3.10",
        ] {
            assert!(
                validate_constraint(bad).is_err(),
                "{} should be rejected",
                bad
            );
        }
        assert!(satisfies(&Version(vec![3, 10]), "").is_err());
    }

    #[test]
    fn validates_interpreter_names() {
        for good in ["python3", "python3.11", "pwsh", "cmd", "node"] {
            assert!(valid_name(good), "{} should be valid", good);
        }
        for bad in ["", "py -3", "a/b", "a\\b", "../python", "python 3"] {
            assert!(!valid_name(bad), "{} should be rejected", bad);
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static NEXT: AtomicU64 = AtomicU64::new(0);

        struct TempDir(PathBuf);

        impl TempDir {
            fn new() -> Self {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos();
                let path = std::env::temp_dir().join(format!(
                    "svcd-interpreter-{}-{}-{}",
                    std::process::id(),
                    nanos,
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir_all(&path).expect("temp dir");
                TempDir(path)
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        fn executable(dir: &Path, name: &str, body: &str) -> PathBuf {
            let path = dir.join(name);
            fs::write(&path, body).expect("script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        }

        fn search_path(dir: &Path) -> String {
            dir.to_string_lossy().to_string()
        }

        #[test]
        fn resolves_a_bare_name_on_the_search_path() {
            let dir = TempDir::new();
            let expected = executable(dir.0.as_path(), "demo", "#!/bin/sh\n");

            let found = resolve("demo", &search_path(&dir.0), None).expect("resolve");

            assert_eq!(found, expected);
        }

        #[test]
        fn skips_a_non_executable_file() {
            let dir = TempDir::new();
            let path = dir.0.join("demo");
            fs::write(&path, "#!/bin/sh\n").expect("file");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

            let error = resolve("demo", &search_path(&dir.0), None).expect_err("missing");

            assert!(error.contains("not found"), "{error}");
        }

        #[test]
        fn reports_a_missing_interpreter() {
            let dir = TempDir::new();
            let spec = Spec {
                name: "definitely-not-installed".into(),
                args: Vec::new(),
                version: None,
            };
            let error = check_with(&spec, &search_path(&dir.0), None, 100).expect_err("missing");
            assert!(error.contains("not found"), "{error}");
        }

        #[test]
        fn rejects_a_name_with_a_separator() {
            let dir = TempDir::new();
            let spec = Spec {
                name: "../python".into(),
                args: Vec::new(),
                version: None,
            };
            let error = check_with(&spec, &search_path(&dir.0), None, 100).expect_err("bad name");
            assert!(
                error.contains("not a bare name") || error.contains("invalid"),
                "{error}"
            );
        }

        #[test]
        fn skips_the_version_probe_when_the_constraint_is_empty() {
            let dir = TempDir::new();
            let marker = dir.0.join("ran");
            let body = format!(
                "#!/bin/sh\ntouch '{}'\necho 'Fake 1.2.3'\n",
                marker.display()
            );
            executable(dir.0.as_path(), "demo", &body);
            let spec = Spec {
                name: "demo".into(),
                args: vec!["--flag".into()],
                version: Some(String::new()),
            };

            let resolved =
                check_with(&spec, &search_path(&dir.0), None, 1_000).expect("presence only");

            assert_eq!(resolved.args, vec!["--flag".to_string()]);
            assert!(!marker.exists(), "an empty version must not run a probe");
        }

        #[test]
        fn probes_and_checks_the_version_when_one_is_declared() {
            let dir = TempDir::new();
            executable(
                dir.0.as_path(),
                "demo",
                "#!/bin/sh\necho 'Fake interpreter 1.2.3'\n",
            );

            let ok = Spec {
                name: "demo".into(),
                args: Vec::new(),
                version: Some(">=1.2,<2".into()),
            };
            assert!(check_with(&ok, &search_path(&dir.0), None, 1_000).is_ok());

            let bad = Spec {
                name: "demo".into(),
                args: Vec::new(),
                version: Some(">=2.0".into()),
            };
            let error = check_with(&bad, &search_path(&dir.0), None, 1_000).expect_err("too old");
            assert!(error.contains("2.0"), "{error}");
            assert!(error.contains("1.2.3"), "{error}");
        }

        #[test]
        fn a_hanging_probe_times_out() {
            let dir = TempDir::new();
            let path = executable(dir.0.as_path(), "demo", "#!/bin/sh\nsleep 5\n");
            let error = probe(&path, 100).expect_err("timeout");
            assert!(error.contains("timed out"), "{error}");
        }
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use std::fs;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static NEXT: AtomicU64 = AtomicU64::new(0);

        struct TempDir(PathBuf);

        impl TempDir {
            fn new() -> Self {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos();
                let path = std::env::temp_dir().join(format!(
                    "svcd-interpreter-win-{}-{}-{}",
                    std::process::id(),
                    nanos,
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir_all(&path).expect("temp dir");
                TempDir(path)
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn resolves_through_pathext() {
            let dir = TempDir::new();
            let expected = dir.0.join("demo.EXE");
            fs::write(&expected, b"stub").expect("stub");

            let found = resolve(
                "demo",
                &dir.0.to_string_lossy(),
                Some(".COM;.EXE;.BAT;.CMD"),
            )
            .expect("resolve");

            assert_eq!(
                found.to_string_lossy().to_ascii_lowercase(),
                expected.to_string_lossy().to_ascii_lowercase()
            );
        }
    }
}
