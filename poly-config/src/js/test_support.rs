//! Shared plumbing for the loader tests.
//!
//! The tests in [`super`] and [`super::layout`] both need a temporary
//! configuration directory, the "load this text" shortcut and a small
//! [`LayoutContext`]. Keeping them here avoids two copies that would drift.

use std::path::PathBuf;

use crate::Config;
use crate::error::ConfigError;
use crate::js::layout::{LayoutContext, LayoutOutput, LayoutRect, LayoutWindow, LayoutWorkspace};
use crate::js::{ConfigLoadResult, load_file};

/// A temporary directory that removes itself.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// Create an empty directory named after `name`.
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("poly-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        return Self(dir);
    }

    /// Write `contents` to `name` inside the directory.
    pub(crate) fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).unwrap();
        return path;
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Load `contents` as `config.js` in a fresh directory.
pub(crate) fn load_text(name: &str, contents: &str) -> ConfigLoadResult {
    let dir = TempDir::new(name);
    let path = dir.write("config.js", contents);
    return load_file(&path);
}

/// Load `contents` and panic with the collected errors when it does not parse.
pub(crate) fn expect_config(name: &str, contents: &str) -> (Config, ConfigLoadResult) {
    let result = load_text(name, contents);
    let config = match &result.config {
        Ok(config) => config.clone(),
        Err(err) => panic!("config failed to load: {err}"),
    };
    return (config, result);
}

/// Load `contents` and return the errors it produced.
pub(crate) fn expect_errors(name: &str, contents: &str) -> ConfigError {
    let result = load_text(name, contents);
    return match result.config {
        Ok(_) => panic!("config unexpectedly loaded"),
        Err(err) => err,
    };
}

/// All error messages as `path: message` strings, for assertions.
pub(crate) fn error_text(err: &ConfigError) -> String {
    return err
        .messages
        .iter()
        .map(|message| return format!("{}: {}", message.path, message.message))
        .collect::<Vec<_>>()
        .join("\n");
}

/// All warnings as `path: message` strings, for assertions.
pub(crate) fn warning_text(result: &ConfigLoadResult) -> Vec<String> {
    return result
        .warnings
        .iter()
        .map(|message| return format!("{}: {}", message.path, message.message))
        .collect();
}

/// A minimal context for calling layout actions directly in tests.
pub(crate) fn layout_context() -> LayoutContext {
    return LayoutContext {
        output: LayoutOutput {
            name: "WL-1".into(),
            width: 1280,
            height: 800,
            scale: 1.0,
            work_area: LayoutRect {
                x: 0,
                y: 0,
                w: 1280,
                h: 800,
            },
        },
        workspace: LayoutWorkspace { id: 7, index: 1 },
        windows: Vec::new(),
        floating: Vec::new(),
        fullscreen: Vec::new(),
        focused: None,
    };
}

/// A tiled window entry for calling the `insert` hook in tests.
pub(crate) fn layout_entry() -> LayoutWindow {
    return LayoutWindow {
        id: 1,
        app_id: None,
        title: None,
        focused: false,
        rect: LayoutRect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        },
        min_size: (0, 0),
        max_size: (0, 0),
    };
}
