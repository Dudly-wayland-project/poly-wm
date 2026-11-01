// Ported from niri (GPL-3.0-or-later): src/utils/spawning.rs
// Copyright (c) Ivan Molodetskikh and niri contributors.

//! Spawning child processes.
//!
//! Two entry points, matching the two spawn flavours of the config:
//! [`spawn`] execs a program directly with an argv, [`spawn_sh`] runs a
//! command line through `sh -c`. Both run on a helper thread and double-fork,
//! so the compositor never blocks on or reaps its children.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{io, thread};

use tracing::{debug, warn};

use crate::utils::signals;

/// Extra environment applied to every child (WAYLAND_DISPLAY, POLY_SOCKET, …).
///
/// Kept here instead of mutating our own process environment, which is unsound
/// once other threads exist.
static CHILD_ENV: RwLock<Vec<(String, String)>> = RwLock::new(Vec::new());

static ORIGINAL_NOFILE_RLIMIT_CUR: AtomicU64 = AtomicU64::new(0);
static ORIGINAL_NOFILE_RLIMIT_MAX: AtomicU64 = AtomicU64::new(0);

/// Raise the nofile soft limit to the hard limit (we hold many client fds and
/// dmabufs) and remember the original so children get it back.
pub fn store_and_increase_nofile_rlimit() {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rlim` is a valid, writable rlimit struct.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) } != 0 {
        let err = io::Error::last_os_error();
        warn!("error getting nofile rlimit: {err:?}");
        return;
    }

    ORIGINAL_NOFILE_RLIMIT_CUR.store(rlim.rlim_cur, Ordering::SeqCst);
    ORIGINAL_NOFILE_RLIMIT_MAX.store(rlim.rlim_max, Ordering::SeqCst);

    rlim.rlim_cur = rlim.rlim_max;
    // SAFETY: `rlim` is a valid rlimit struct.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) } != 0 {
        let err = io::Error::last_os_error();
        warn!("error setting nofile rlimit: {err:?}");
    }
}

/// Restore the original nofile limit. Async-signal-safe; used in `pre_exec`.
pub(crate) fn restore_nofile_rlimit() {
    let rlim_cur = ORIGINAL_NOFILE_RLIMIT_CUR.load(Ordering::SeqCst);
    let rlim_max = ORIGINAL_NOFILE_RLIMIT_MAX.load(Ordering::SeqCst);
    if rlim_cur == 0 {
        return;
    }
    let rlim = libc::rlimit { rlim_cur, rlim_max };
    // SAFETY: `rlim` is a valid rlimit struct; failure is harmless here.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) };
}

/// Set (or replace) an environment variable for future children.
pub fn set_child_env(key: &str, value: &str) {
    let mut env = match CHILD_ENV.write() {
        Ok(env) => env,
        Err(poisoned) => poisoned.into_inner(),
    };
    env.retain(|(k, _)| return k != key);
    env.push((key.to_owned(), value.to_owned()));
}

/// Set an environment variable, or stop overriding it when `value` is `None`.
///
/// `None` means "let the child inherit whatever we have", which is what an
/// unset configuration option should do: a value the user exported in their
/// session keeps working.
pub fn set_child_env_opt(key: &str, value: Option<&str>) {
    let Some(value) = value else {
        let mut env = match CHILD_ENV.write() {
            Ok(env) => env,
            Err(poisoned) => poisoned.into_inner(),
        };
        env.retain(|(k, _)| return k != key);
        return;
    };
    set_child_env(key, value);
}

/// The value a child would get for `key`, if one is set.
///
/// Used by the XWayland satellite, which is spawned with `Command` of its own
/// but still has to reach *our* Wayland socket (`utils::xwayland`).
pub fn child_env_value(key: &str) -> Option<String> {
    let env = match CHILD_ENV.read() {
        Ok(env) => env,
        Err(poisoned) => poisoned.into_inner(),
    };
    return env
        .iter()
        .find(|(name, _)| return name == key)
        .map(|(_, value)| return value.clone());
}

/// Exec `argv[0]` with the remaining arguments.
pub fn spawn(argv: Vec<String>) {
    if argv.is_empty() {
        warn!("refusing to spawn an empty command");
        return;
    }
    let res = thread::Builder::new()
        .name("poly-spawn".to_owned())
        .spawn(move || spawn_sync(&argv));
    if let Err(err) = res {
        warn!("error starting spawn thread: {err:?}");
    }
}

/// Run `command` through `sh -c`.
pub fn spawn_sh(command: String) {
    spawn(vec!["sh".to_owned(), "-c".to_owned(), command]);
}

fn spawn_sync(argv: &[String]) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    debug!("spawning {argv:?}");

    let mut process = Command::new(program);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("NOTIFY_SOCKET")
        .env_remove("WAYLAND_SOCKET")
        // Nested under a host session the inherited DISPLAY points at the host
        // X server, so it is dropped: X11 support comes from
        // `xwayland-satellite`, and `utils::xwayland` puts *its* `DISPLAY` into
        // `CHILD_ENV` once the X11 sockets exist (applied below, which wins).
        .env_remove("DISPLAY");

    {
        let env = match CHILD_ENV.read() {
            Ok(env) => env,
            Err(poisoned) => poisoned.into_inner(),
        };
        for (key, value) in env.iter() {
            process.env(key, value);
        }
    }

    // SAFETY: the closure only calls async-signal-safe functions
    // (pthread_sigmask, fork, _exit) between fork and exec.
    unsafe {
        process.pre_exec(|| {
            signals::unblock_all()?;
            // Double-fork so that the grandchild is reparented to init and we
            // never need to waitpid() it.
            match libc::fork() {
                -1 => return Err(io::Error::last_os_error()),
                0 => {
                    restore_nofile_rlimit();
                    return Ok(());
                }
                _ => libc::_exit(0),
            }
        });
    }

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!("error spawning {program:?}: {err}");
            return;
        }
    };

    // Reaps the intermediate child, which exits right after forking.
    if let Err(err) = child.wait() {
        warn!("error waiting for intermediate child of {program:?}: {err}");
    }
}
