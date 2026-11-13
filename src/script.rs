//! Action detection shared by core drop-in scripts and
//! `plugin bundle --from-core`.
//!
//! The heuristics are deliberately textual and conservative: only a declared
//! shell function, PowerShell function, cmd label or shell `case` branch counts
//! as an action. Comments are ignored.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Actions {
    pub start: bool,
    pub stop: bool,
    pub status: bool,
}

impl Actions {
    fn declare(&mut self, action: &str) {
        match action {
            "start" => self.start = true,
            "stop" => self.stop = true,
            "status" => self.status = true,
            _ => {}
        }
    }

    pub fn any(self) -> bool {
        self.start || self.stop || self.status
    }
}

const ACTIONS: [&str; 3] = ["start", "stop", "status"];

/// Any language the bundler may treat as a script when an interpreter is
/// declared. Unlike [`extension_supported`] this is not limited to drop-in
/// services: `.py` / `.js` / ... are valid plugin payloads.
pub fn looks_like_script(path: &str, data: &[u8]) -> bool {
    if data.starts_with(b"#!") {
        return true;
    }
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        extension.as_str(),
        "sh" | "bash"
            | "py"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "pl"
            | "rb"
            | "php"
            | "lua"
            | "ps1"
            | "cmd"
            | "bat"
    )
}

/// The script extensions a drop-in service may use on this platform.
pub fn extension_supported(extension: &str) -> bool {
    #[cfg(windows)]
    {
        matches!(extension, "bat" | "cmd" | "ps1")
    }
    #[cfg(not(windows))]
    {
        extension == "sh"
    }
}

/// Detect declared actions. Unsupported script languages return the empty set:
/// they run from the top with no action argument.
pub fn declared_actions(text: &str, extension: &str) -> Actions {
    let mut actions = Actions::default();
    if !extension_supported(extension) {
        return actions;
    }
    for raw in text.lines() {
        let line = raw.trim_start();
        if line.is_empty() || is_comment(line) {
            continue;
        }
        for action in ACTIONS {
            if marks_action(line, action, extension) {
                actions.declare(action);
            }
        }
    }
    actions
}

fn is_comment(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    line.starts_with('#') || lower.starts_with("rem ") || lower.starts_with("rem\t")
}

fn marks_action(line: &str, action: &str, extension: &str) -> bool {
    match extension {
        "bat" | "cmd" => {
            let lower = line.to_ascii_lowercase();
            let Some(rest) = lower.strip_prefix(&format!(":{}", action)) else {
                return false;
            };
            rest.is_empty() || rest.chars().next().is_some_and(char::is_whitespace)
        }
        "ps1" => {
            let lower = line.to_ascii_lowercase();
            let Some(rest) = lower.strip_prefix("function ") else {
                return false;
            };
            let Some(after) = rest.trim_start().strip_prefix(action) else {
                return false;
            };
            let after = after.trim_start();
            after.starts_with('{') || after.starts_with('(') || after.is_empty()
        }
        _ => {
            let (candidates, keyword) = match line.strip_prefix("function ") {
                Some(rest) => (rest.trim_start(), true),
                None => (line, false),
            };
            if let Some(after) = candidates.strip_prefix(action) {
                let after = after.trim_start();
                if after.starts_with('(')
                    || (keyword && (after.starts_with('{') || after.is_empty()))
                {
                    return true;
                }
            }
            // `case "$1" in start) ...` style dispatchers.
            line.strip_prefix(action)
                .map(|after| after.trim_start().starts_with(')'))
                .unwrap_or(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn shell_functions_and_case_branches_declare_actions() {
        let actions = declared_actions(
            "#!/bin/sh\nstart() { :; }\ncase \"$1\" in\n  stop)\n    :;;\n  status) :;;\nesac\n",
            "sh",
        );
        assert!(actions.start);
        assert!(actions.stop);
        assert!(actions.status);
    }

    #[test]
    fn comments_do_not_declare_actions() {
        #[cfg(windows)]
        let text = "@echo off\r\nrem :start\r\n:: :stop\r\n";
        #[cfg(not(windows))]
        let text = "#!/bin/sh\n# start() { :; }\n# stop() { :; }\n";
        let extension = if cfg!(windows) { "cmd" } else { "sh" };
        let actions = declared_actions(text, extension);
        assert!(!actions.any(), "{:?}", actions);
    }

    #[cfg(windows)]
    #[test]
    fn cmd_labels_and_powershell_functions_declare_actions() {
        let cmd = declared_actions("@echo off\r\n:start\r\n:status\r\n", "cmd");
        assert!(cmd.start);
        assert!(cmd.status);
        assert!(!cmd.stop);

        let ps1 = declared_actions("function Start { }\r\nfunction Stop { }\r\n", "ps1");
        assert!(ps1.start);
        assert!(ps1.stop);
        assert!(!ps1.status);
    }

    #[test]
    fn unsupported_languages_declare_nothing() {
        let actions = declared_actions("def start():\n    pass\n", "py");
        assert!(!actions.any(), "{:?}", actions);
    }
}
