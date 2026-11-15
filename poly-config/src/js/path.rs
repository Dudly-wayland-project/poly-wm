//! Path resolution shared by `include()`, `read_file()` and the settings that
//! name files.
//!
//! A relative path always means "relative to the file that wrote it", so a
//! configuration split over several files keeps working when it is moved
//! around as a directory.

use std::path::{Path, PathBuf};

/// Resolve a path the way the whole configuration does:
/// `~` expands to the home directory, relative paths are relative to the file
/// that wrote them.
pub(crate) fn resolve_path(from_file: Option<&Path>, requested: &str) -> PathBuf {
    let requested = expand_tilde(requested);
    if requested.is_absolute() {
        return requested;
    }
    let base = from_file
        .and_then(|path| return path.parent())
        .unwrap_or(Path::new("."));
    return base.join(requested);
}

/// Expand a leading `~` or `~/`.
fn expand_tilde(requested: &str) -> PathBuf {
    if requested == "~" {
        return home_dir().unwrap_or_else(|| return PathBuf::from(requested));
    }
    if let Some(rest) = requested.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    return PathBuf::from(requested);
}

/// `$HOME`, when it is set and non-empty.
fn home_dir() -> Option<PathBuf> {
    return std::env::var_os("HOME")
        .filter(|home| return !home.is_empty())
        .map(PathBuf::from);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_follow_the_writing_file() {
        let resolved = resolve_path(Some(Path::new("/cfg/poly/config.js")), "parts/binds.js");
        assert_eq!(resolved, PathBuf::from("/cfg/poly/parts/binds.js"));
    }

    #[test]
    fn absolute_paths_are_left_alone() {
        let resolved = resolve_path(Some(Path::new("/cfg/config.js")), "/etc/poly/x.js");
        assert_eq!(resolved, PathBuf::from("/etc/poly/x.js"));
    }

    #[test]
    fn without_a_file_paths_are_relative_to_the_cwd() {
        let resolved = resolve_path(None, "config.js");
        assert_eq!(resolved, PathBuf::from("./config.js"));
    }
}
