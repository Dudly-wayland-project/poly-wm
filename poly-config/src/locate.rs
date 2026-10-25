//! Config file discovery.
//!
//! Order: `--config` > `$POLY_CONFIG` > `$XDG_CONFIG_HOME/poly/config.lua`
//! (default `~/.config/poly/config.lua`) > `/etc/poly/config.lua`.

use std::path::{Path, PathBuf};

use crate::LocateError;

/// Where the resolved config path came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Explicit `--config` argument.
    Cli(PathBuf),
    /// `$POLY_CONFIG`.
    Env(PathBuf),
    /// User config under XDG config home.
    User(PathBuf),
    /// System-wide `/etc/poly/config.lua`.
    System(PathBuf),
    /// No config file found; built-in defaults apply. Carries the user path
    /// that would be watched/created.
    Default(Option<PathBuf>),
}

impl ConfigSource {
    /// The path to load, if any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConfigSource::Cli(p)
            | ConfigSource::Env(p)
            | ConfigSource::User(p)
            | ConfigSource::System(p) => return Some(p),
            ConfigSource::Default(_) => return None,
        }
    }

    /// The file to watch even when it does not exist (yet).
    ///
    /// [`Self::path`] answers "what did we load"; this answers "what should
    /// appear for the configuration to change", which is also the file that
    /// gets a default configuration written to it.
    pub fn watch_path(&self) -> Option<&Path> {
        match self {
            ConfigSource::Cli(p)
            | ConfigSource::Env(p)
            | ConfigSource::User(p)
            | ConfigSource::System(p) => return Some(p),
            ConfigSource::Default(path) => return path.as_deref(),
        }
    }
}

/// Environment lookups, abstracted for tests.
pub trait Env {
    fn var(&self, key: &str) -> Option<String>;
    fn exists(&self, path: &Path) -> bool;
}

struct RealEnv;

impl Env for RealEnv {
    fn var(&self, key: &str) -> Option<String> {
        return std::env::var(key).ok().filter(|v| !v.is_empty());
    }

    fn exists(&self, path: &Path) -> bool {
        return path.is_file();
    }
}

/// Resolve the config file using the process environment.
pub fn locate_config(cli: Option<&Path>) -> Result<ConfigSource, LocateError> {
    return locate_with(cli, &RealEnv);
}

pub(crate) fn locate_with(cli: Option<&Path>, env: &dyn Env) -> Result<ConfigSource, LocateError> {
    if let Some(path) = cli {
        if !env.exists(path) {
            return Err(LocateError::Missing {
                path: path.to_owned(),
                origin: "--config",
            });
        }
        return Ok(ConfigSource::Cli(path.to_owned()));
    }

    if let Some(path) = env.var("POLY_CONFIG") {
        let path = PathBuf::from(path);
        if !env.exists(&path) {
            return Err(LocateError::Missing {
                path,
                origin: "$POLY_CONFIG",
            });
        }
        return Ok(ConfigSource::Env(path));
    }

    let user = env
        .var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            return env.var("HOME").map(|h| {
                return PathBuf::from(h).join(".config");
            });
        })
        .map(|dir| {
            return dir.join("poly").join("config.lua");
        });

    if let Some(user) = &user
        && env.exists(user)
    {
        return Ok(ConfigSource::User(user.clone()));
    }

    let system = PathBuf::from("/etc/poly/config.lua");
    if env.exists(&system) {
        return Ok(ConfigSource::System(system));
    }

    return Ok(ConfigSource::Default(user));
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[derive(Default)]
    struct FakeEnv {
        vars: HashMap<&'static str, &'static str>,
        files: HashSet<PathBuf>,
    }

    impl Env for FakeEnv {
        fn var(&self, key: &str) -> Option<String> {
            return self.vars.get(key).map(|v| {
                return (*v).to_owned();
            });
        }

        fn exists(&self, path: &Path) -> bool {
            return self.files.contains(path);
        }
    }

    #[test]
    fn cli_takes_precedence() {
        let mut env = FakeEnv::default();
        env.vars.insert("POLY_CONFIG", "/env.lua");
        env.files.insert("/env.lua".into());
        env.files.insert("/cli.lua".into());
        let got = locate_with(Some(Path::new("/cli.lua")), &env).unwrap();
        assert_eq!(got, ConfigSource::Cli("/cli.lua".into()));
    }

    #[test]
    fn missing_explicit_path_is_error() {
        let env = FakeEnv::default();
        assert!(locate_with(Some(Path::new("/nope.lua")), &env).is_err());
    }

    #[test]
    fn xdg_then_home_then_system() {
        let mut env = FakeEnv::default();
        env.vars.insert("HOME", "/home/u");
        env.files.insert("/home/u/.config/poly/config.lua".into());
        let got = locate_with(None, &env).unwrap();
        assert_eq!(
            got,
            ConfigSource::User("/home/u/.config/poly/config.lua".into())
        );

        let mut env = FakeEnv::default();
        env.vars.insert("XDG_CONFIG_HOME", "/xdg");
        env.files.insert("/etc/poly/config.lua".into());
        let got = locate_with(None, &env).unwrap();
        assert_eq!(got, ConfigSource::System("/etc/poly/config.lua".into()));
    }

    #[test]
    fn falls_back_to_default() {
        let mut env = FakeEnv::default();
        env.vars.insert("XDG_CONFIG_HOME", "/xdg");
        let got = locate_with(None, &env).unwrap();
        assert_eq!(
            got,
            ConfigSource::Default(Some("/xdg/poly/config.lua".into()))
        );
        assert!(got.path().is_none());
    }
}
