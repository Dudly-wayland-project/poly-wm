//! Configuration errors: collected with field paths instead of failing fast,
//! so one load reports everything that is wrong (`docs/plan.md` §11).

use std::fmt;
use std::path::PathBuf;

/// One problem with the configuration, labelled with the setting it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigMessage {
    /// Path of the setting, like `keybinds["Mod+Q"]` or `input.keyboard.layout`.
    /// Empty for whole-file problems (a Lua error, a missing include file).
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl ConfigMessage {
    /// A problem with one specific setting.
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        return Self {
            path: path.into(),
            message: message.into(),
        };
    }

    /// A problem with the file as a whole.
    pub fn file(message: impl Into<String>) -> Self {
        return Self {
            path: String::new(),
            message: message.into(),
        };
    }
}

impl fmt::Display for ConfigMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            return write!(f, "{}", self.message);
        }
        return write!(f, "{}: {}", self.path, self.message);
    }
}

/// Everything that went wrong while loading the configuration.
#[derive(Debug)]
pub struct ConfigError {
    /// The root configuration file.
    pub source: PathBuf,
    /// Every problem found, in the order it was reported.
    pub messages: Vec<ConfigMessage>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "error loading config {}:", self.source.display())?;
        for message in &self.messages {
            // A message can span lines (a Lua traceback); indent the rest so
            // the list stays readable.
            let text = message.to_string();
            let mut lines = text.lines();
            let first = lines.next().unwrap_or("");
            writeln!(f, "  - {first}")?;
            for line in lines {
                writeln!(f, "    {line}")?;
            }
        }
        return Ok(());
    }
}

impl std::error::Error for ConfigError {}

/// Collects problems while parsing one configuration.
#[derive(Debug, Default)]
pub(crate) struct Errors {
    messages: Vec<ConfigMessage>,
    warnings: Vec<ConfigMessage>,
}

impl Errors {
    /// Record a problem with one setting.
    pub fn error(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.messages.push(ConfigMessage::new(path, message));
    }

    /// Record a non-fatal problem: the setting is ignored, the load continues.
    pub fn warn(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(ConfigMessage::new(path, message));
    }

    /// Record a problem with the file as a whole.
    pub fn file(&mut self, message: impl Into<String>) {
        self.messages.push(ConfigMessage::file(message));
    }

    /// Split into (errors, warnings).
    pub fn into_parts(self) -> (Vec<ConfigMessage>, Vec<ConfigMessage>) {
        return (self.messages, self.warnings);
    }
}

/// Errors raised while locating the configuration file.
#[derive(Debug, thiserror::Error)]
pub enum LocateError {
    /// A path given explicitly (via `--config` or `$POLY_CONFIG`) does not exist.
    #[error("config file {path} (from {origin}) does not exist")]
    Missing {
        /// The path that does not exist.
        path: PathBuf,
        /// Where the path came from, for the message.
        origin: &'static str,
    },
}

/// Append a key to a setting path: `keybinds` + `Mod+Q` -> `keybinds["Mod+Q"]`.
pub(crate) fn path_key(base: &str, key: &str) -> String {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|c| return c.is_ascii_alphanumeric() || c == '_');
    if base.is_empty() {
        return key.to_owned();
    }
    if plain {
        return format!("{base}.{key}");
    }
    return format!("{base}[\"{key}\"]");
}

/// Append an index to a setting path: `spawn_at_startup` + 0 -> `spawn_at_startup[0]`.
pub(crate) fn path_index(base: &str, index: usize) -> String {
    return format!("{base}[{index}]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_look_like_lua() {
        assert_eq!(path_key("input", "keyboard"), "input.keyboard");
        assert_eq!(path_key("keybinds", "Mod+Q"), r#"keybinds["Mod+Q"]"#);
        assert_eq!(path_key("", "general"), "general");
        assert_eq!(path_index("spawn_at_startup", 2), "spawn_at_startup[2]");
    }

    #[test]
    fn error_display_lists_messages() {
        let err = ConfigError {
            source: PathBuf::from("/tmp/config.lua"),
            messages: vec![
                ConfigMessage::new("keybinds[\"Mod+Q\"]", "invalid key: Q"),
                ConfigMessage::file("include chain: a.lua -> b.lua -> a.lua"),
            ],
        };
        let text = err.to_string();
        assert!(
            text.contains("keybinds[\"Mod+Q\"]: invalid key: Q"),
            "{text}"
        );
        assert!(text.contains("- include chain"), "{text}");
    }
}
