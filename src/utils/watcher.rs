//! Configuration file watching.
//!
//! Polling the modification time of a handful of files every 500 ms is enough
//! for a configuration that is edited by hand: it needs no `inotify` handle,
//! works on filesystems where watching a file that does not exist yet is
//! awkward, and adds one `stat` per file per half second.
//!
//! What is watched is the *include closure*: the root file plus every file
//! `include()` pulled in, because editing a theme file has to reload just
//! like editing the root file does. The closure is rebuilt after every load, so
//! an include that is added or removed follows along.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The files one load read, with the timestamps they had at that moment.
#[derive(Debug, Default)]
pub struct ConfigWatcher {
    /// Watched file and its modification time when the configuration was read.
    ///
    /// `None` means "did not exist"; a file that appears later counts as a
    /// change, which is how a configuration created while poly is running gets
    /// picked up.
    files: Vec<(PathBuf, Option<SystemTime>)>,
}

impl ConfigWatcher {
    /// Start watching `files`, taking the current timestamps as the baseline.
    pub fn watch(&mut self, files: Vec<PathBuf>) {
        self.files = files
            .into_iter()
            .map(|path| {
                let stamp = mtime(&path);
                (path, stamp)
            })
            .collect();
    }

    /// The first watched file that changed since the last call, if any.
    ///
    /// The timestamp is refreshed before returning, so one edit causes exactly
    /// one reload even if the caller needs a few polls to finish it.
    pub fn changed(&mut self) -> Option<PathBuf> {
        for (path, stamp) in &mut self.files {
            let now = mtime(path);
            if now != *stamp {
                *stamp = now;
                return Some(path.clone());
            }
        }
        return None;
    }

    /// The watched paths, for logging and tests.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        return self.files.iter().map(|(path, _)| return path.as_path());
    }
}

/// The modification time of `path`, or `None` if it cannot be read.
fn mtime(path: &Path) -> Option<SystemTime> {
    return fs::metadata(path)
        .ok()
        .and_then(|meta| return meta.modified().ok());
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::time::Duration;

    use super::*;

    /// A file in a private temporary directory.
    struct TempFile {
        dir: PathBuf,
    }

    impl TempFile {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("poly-watcher-{}-{name}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            return Self { dir };
        }

        fn path(&self, name: &str) -> PathBuf {
            return self.dir.join(name);
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.path(name);
            let mut file = fs::File::create(&path).unwrap();
            file.write_all(contents.as_bytes()).unwrap();
            return path;
        }

        /// Push the modification time forward, because `stat` has a coarse
        /// resolution on some filesystems and a test must not depend on it.
        fn touch_later(&self, path: &Path, millis: u64) {
            let later = SystemTime::now() + Duration::from_millis(millis);
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(later)
                .unwrap();
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn unchanged_files_report_nothing() {
        let dir = TempFile::new("unchanged");
        let root = dir.write("config.js", "config({})");
        let mut watcher = ConfigWatcher::default();
        watcher.watch(vec![root.clone()]);
        assert_eq!(watcher.changed(), None);
        assert_eq!(watcher.changed(), None);
    }

    #[test]
    fn an_edited_file_is_reported_once() {
        let dir = TempFile::new("edited");
        let root = dir.write("config.js", "config({})");
        let mut watcher = ConfigWatcher::default();
        watcher.watch(vec![root.clone()]);

        dir.touch_later(&root, 1_000);
        assert_eq!(watcher.changed(), Some(root));
        // The stamp was refreshed, so the same edit is not reported twice.
        assert_eq!(watcher.changed(), None);
    }

    #[test]
    fn an_include_is_watched_too() {
        let dir = TempFile::new("include");
        let root = dir.write("config.js", "config({})");
        let theme = dir.write("theme.js", "config({})");
        let mut watcher = ConfigWatcher::default();
        watcher.watch(vec![root.clone(), theme.clone()]);
        assert_eq!(watcher.paths().count(), 2);

        dir.touch_later(&theme, 1_000);
        assert_eq!(watcher.changed(), Some(theme));
    }

    #[test]
    fn a_file_that_appears_is_a_change() {
        let dir = TempFile::new("created");
        let missing = dir.path("config.js");
        let mut watcher = ConfigWatcher::default();
        watcher.watch(vec![missing.clone()]);
        assert_eq!(watcher.changed(), None);

        // Creating the file counts as a change, which is how a configuration
        // written while poly runs gets picked up.
        dir.write("config.js", "config({})");
        assert_eq!(watcher.changed(), Some(missing));
    }
}
