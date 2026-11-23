//! Build-time walk of `src/assets/static/`.
//!
//! `build.rs` includes this file, and the console's tests include it too, so
//! the recursion and the dot-file rule are exercised by `cargo test` instead of
//! only by whichever build happens to run.

use std::fs;
use std::path::{Path, PathBuf};

/// Collect regular files below `directory` with `/`-separated relative paths.
/// Dot files and dot directories stay out of the console.
pub(crate) fn collect(directory: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
    for entry in entries {
        let entry = entry.expect("console asset directory entry");
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str().map(str::to_string) else {
            panic!(
                "console asset name is not UTF-8: {}",
                entry.path().display()
            );
        };
        if name.starts_with('.') {
            continue;
        }
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        let file_type = entry.file_type().expect("console asset file type");
        if file_type.is_dir() {
            collect(&path, &relative, out);
        } else if file_type.is_file() {
            out.push((relative, path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-console-tree-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create scratch tree");
        root
    }

    #[test]
    fn walks_nested_directories_in_slash_separator_form() {
        let root = scratch("nested");
        fs::create_dir_all(root.join("icons/sub")).expect("create subdirectory");
        fs::write(root.join("index.html"), b"page").expect("write page");
        fs::write(root.join("icons/logo.svg"), b"logo").expect("write icon");
        fs::write(root.join("icons/sub/tiny.txt"), b"tiny").expect("write nested file");

        let mut collected = Vec::new();
        collect(&root, "", &mut collected);
        let mut paths: Vec<String> = collected.into_iter().map(|(path, _)| path).collect();
        paths.sort();
        assert_eq!(
            paths,
            ["icons/logo.svg", "icons/sub/tiny.txt", "index.html"]
        );

        fs::remove_dir_all(&root).expect("clean scratch tree");
    }

    #[test]
    fn skips_dot_files_and_dot_directories() {
        let root = scratch("dots");
        fs::create_dir_all(root.join(".git")).expect("create dot directory");
        fs::write(root.join("index.html"), b"page").expect("write page");
        fs::write(root.join(".hidden"), b"hidden").expect("write dot file");
        fs::write(root.join(".git/config"), b"config").expect("write dot directory file");

        let mut collected = Vec::new();
        collect(&root, "", &mut collected);
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].0, "index.html");

        fs::remove_dir_all(&root).expect("clean scratch tree");
    }
}
