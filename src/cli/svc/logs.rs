//! `svc logs`: tail, level filter, follow and the merged view.
//!
//! The read side is a passive projection of the files the daemon wrote: a line
//! without the new timestamp/level shape passes through untouched.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::client::{EXIT_NOT_FOUND, EXIT_OK, EXIT_SERVICE_ERROR};
use crate::platform::config::Config;
use crate::platform::log::{self, Level};

pub(super) fn show_logs(
    config: &Config,
    id: Option<&str>,
    follow: bool,
    lines: usize,
    level: Option<Level>,
    all: bool,
) -> i32 {
    if all {
        return match merged_logs(&config.paths.logs_dir, lines, level) {
            Ok(text) => {
                print!("{}", text);
                EXIT_OK
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("svc: no logs in {}", config.paths.logs_dir.display());
                EXIT_NOT_FOUND
            }
            Err(error) => {
                eprintln!("svc: cannot read logs: {}", error);
                EXIT_SERVICE_ERROR
            }
        };
    }
    let id = id.expect("parse_logs requires an id unless --all is set");
    let path = if id == "svcd" {
        config.paths.logs_dir.join("svcd.log")
    } else {
        config.paths.logs_dir.join(format!("{}.log", id))
    };
    let tail = match log::tail(&path, lines) {
        Ok(tail) => tail,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("svc: no log for {}", id);
            return EXIT_NOT_FOUND;
        }
        Err(error) => {
            eprintln!("svc: cannot read {}: {}", path.display(), error);
            return EXIT_SERVICE_ERROR;
        }
    };
    let tail = match level {
        Some(threshold) => log::filter_text(&tail, threshold),
        None => tail,
    };
    print!("{}", tail);
    if follow {
        let offset = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        follow_log(&path, offset, level);
    }
    EXIT_OK
}

/// Merge every `<logs>/*.log` by the timestamp column. A per-file tail is
/// taken first: a line older than the requested window in its own file cannot
/// be in the global window either. Old-format lines have no time column and
/// sort before timestamped ones, in file order.
fn merged_logs(dir: &Path, lines: usize, level: Option<Level>) -> std::io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
            paths.push(path);
        }
    }
    paths.sort();
    let mut rows: Vec<(String, usize, String)> = Vec::new();
    for (file_index, path) in paths.iter().enumerate() {
        let Ok(text) = log::tail(path, lines) else {
            continue;
        };
        for line in text.lines() {
            if let Some(threshold) = level {
                if !log::keep(line, threshold) {
                    continue;
                }
            }
            rows.push((
                log::timestamp(line).unwrap_or("").to_string(),
                file_index,
                line.to_string(),
            ));
        }
    }
    rows.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let start = rows.len().saturating_sub(lines);
    let mut out = String::new();
    for (_, _, line) in &rows[start..] {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

fn follow_log(path: &Path, mut offset: u64, level: Option<Level>) -> ! {
    use std::io::{Read, Seek, SeekFrom};

    let mut pending: Vec<u8> = Vec::new();
    loop {
        if let Ok(metadata) = std::fs::metadata(path) {
            if metadata.len() < offset {
                // The file rotated or was truncated: start its new generation.
                offset = 0;
                pending.clear();
            }
        }
        if let Ok(mut file) = std::fs::File::open(path) {
            if file.seek(SeekFrom::Start(offset)).is_ok() {
                let mut buffer = Vec::new();
                if file.read_to_end(&mut buffer).is_ok() && !buffer.is_empty() {
                    offset = offset.saturating_add(buffer.len() as u64);
                    pending.extend_from_slice(&buffer);
                    while let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<u8> = pending.drain(..=position).collect();
                        let text = String::from_utf8_lossy(&line[..line.len() - 1]);
                        if let Some(threshold) = level {
                            if !log::keep(&text, threshold) {
                                continue;
                            }
                        }
                        println!("{}", text);
                        let _ = std::io::stdout().flush();
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::super::{parse, Command};
    use crate::platform::log::Level;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_logs_with_flags_and_a_default_tail() {
        assert_eq!(
            parse(&args(&["logs", "demo"])).expect("logs"),
            Command::Logs {
                id: Some("demo".into()),
                follow: false,
                lines: 200,
                level: None,
                all: false
            }
        );
        assert_eq!(
            parse(&args(&["logs", "--follow", "demo", "--lines", "5"])).expect("logs"),
            Command::Logs {
                id: Some("demo".into()),
                follow: true,
                lines: 5,
                level: None,
                all: false
            }
        );
        assert_eq!(
            parse(&args(&["logs", "-f", "demo"])).expect("logs"),
            Command::Logs {
                id: Some("demo".into()),
                follow: true,
                lines: 200,
                level: None,
                all: false
            }
        );
        assert_eq!(
            parse(&args(&["logs", "demo", "--lines=7"])).expect("logs"),
            Command::Logs {
                id: Some("demo".into()),
                follow: false,
                lines: 7,
                level: None,
                all: false
            }
        );
        assert_eq!(
            parse(&args(&["logs", "demo", "--level", "warn"])).expect("logs"),
            Command::Logs {
                id: Some("demo".into()),
                follow: false,
                lines: 200,
                level: Some(Level::Warn),
                all: false
            }
        );
        assert_eq!(
            parse(&args(&["logs", "--all", "--level=ERROR"])).expect("logs"),
            Command::Logs {
                id: None,
                follow: false,
                lines: 200,
                level: Some(Level::Error),
                all: true
            }
        );
        assert!(parse(&args(&["logs"])).is_err());
        assert!(parse(&args(&["logs", "--all", "demo"])).is_err());
        assert!(parse(&args(&["logs", "--all", "--follow"])).is_err());
        assert!(parse(&args(&["logs", "demo", "--level", "loud"])).is_err());
        assert!(parse(&args(&["logs", "demo", "--lines", "nope"])).is_err());
        assert!(parse(&args(&["logs", "demo", "--lines", "2001"])).is_err());
    }
}
