// Ported from niri (GPL-3.0-or-later): src/utils/xwayland/mod.rs
// Copyright (c) Ivan Molodetskikh and niri contributors.

//! The X11 side of `xwayland-satellite` (`docs/plan.md` §13).
//!
//! X11 clients find an X server through a socket named after a display number:
//! `:0` is the abstract socket `/tmp/.X11-unix/X0` (Linux) plus the filesystem
//! socket of the same name, and a `/tmp/.X0-lock` file claims the number for a
//! process. Poly creates both sockets itself and hands them to
//! `xwayland-satellite` through `-listenfd`, so that a client connecting to an
//! X11 socket — and nothing else — is what starts Xwayland
//! ([`satellite`](self::satellite)).
//!
//! Creating the sockets is the compositor's job, not the satellite's: whoever
//! owns the display number owns `/tmp/.X{n}-lock`, and the satellite only ever
//! runs as long as some client needs it.
//!
//! The socket and lock file are removed when the [`X11Connection`] is dropped,
//! so a clean exit leaves nothing behind for the next session to trip over.

use std::os::fd::OwnedFd;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::path::PathBuf;

use anyhow::{Context as _, anyhow, ensure};
use smithay::reexports::rustix::fs::{OFlags, lstat, mkdir, open, unlink};
use smithay::reexports::rustix::io::{Errno, write};
use smithay::reexports::rustix::process::{getpid, getuid};

pub mod satellite;

/// Where display sockets live on the filesystem.
const TMP_UNIX_DIR: &str = "/tmp";
/// The directory holding the filesystem sockets, one per display.
const X11_TMP_UNIX_DIR: &str = "/tmp/.X11-unix";

/// The X11 sockets of one display number, owned by this process.
pub(crate) struct X11Connection {
    /// `:N`, the value X11 clients read from `DISPLAY`.
    pub(crate) display_name: String,
    /// The abstract socket, Linux only; there are none on other systems.
    pub(crate) abstract_fd: Option<OwnedFd>,
    /// The filesystem socket `/tmp/.X11-unix/XN`.
    pub(crate) unix_fd: OwnedFd,
    /// Removes the filesystem socket on drop.
    _unix_guard: Unlink,
    /// Removes `/tmp/.XN-lock` on drop.
    _lock_guard: Unlink,
}

/// Deletes a path when dropped.
struct Unlink(String);

impl Drop for Unlink {
    fn drop(&mut self) {
        let _ = unlink(&self.0);
        return;
    }
}

/// `~` at the start of a path expands to `$HOME`; anything else is left alone.
///
/// The satellite program is looked up in `PATH` when it is a bare name, so this
/// deliberately does not resolve relative paths against the configuration file
/// the way `poly.include` does.
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        if path == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return PathBuf::from(home).join(rest);
        }
    }
    return PathBuf::from(path);
}

// Adapted from Mutter:
// https://gitlab.gnome.org/GNOME/mutter/-/blob/48.3.1/src/wayland/meta-xwayland.c?ref_type=tags#L513
fn ensure_x11_unix_dir() -> anyhow::Result<()> {
    match mkdir(X11_TMP_UNIX_DIR, 0o1777.into()) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => {
            ensure_x11_unix_perms().context("wrong X11 directory permissions")?;
            return Ok(());
        }
        Err(err) => Err(err).context("error creating the X11 directory"),
    }
}

/// `/tmp/.X11-unix` must be a sticky, world-writable directory we may use.
///
/// A directory owned by somebody else and writable only by them would let that
/// user impersonate an X server for every client on the machine.
fn ensure_x11_unix_perms() -> anyhow::Result<()> {
    let x11_tmp = lstat(X11_TMP_UNIX_DIR).context("error checking X11 directory permissions")?;
    let tmp = lstat(TMP_UNIX_DIR).context("error checking /tmp directory permissions")?;

    ensure!(
        x11_tmp.st_uid == tmp.st_uid || x11_tmp.st_uid == getuid().as_raw(),
        "wrong ownership for the X11 directory"
    );
    ensure!(
        (x11_tmp.st_mode & 0o022) == 0o022,
        "the X11 directory is not writable"
    );
    ensure!(
        (x11_tmp.st_mode & 0o1000) == 0o1000,
        "the X11 directory is missing the sticky bit"
    );

    return Ok(());
}

/// Claim a free display number by creating its lock file.
///
/// The lock only works because creating it is atomic (`O_EXCL`): whoever wins
/// the race owns the number.
fn pick_x11_display(start: u32) -> anyhow::Result<(u32, OwnedFd, Unlink)> {
    for n in start..start + 50 {
        let lock_path = format!("/tmp/.X{n}-lock");
        let flags = OFlags::WRONLY | OFlags::CLOEXEC | OFlags::CREATE | OFlags::EXCL;
        let Ok(lock_fd) = open(&lock_path, flags, 0o444.into()) else {
            // FIXME: check whether the process holding the lock is still alive
            // and reuse the number when it is not.
            continue;
        };
        return Ok((n, lock_fd, Unlink(lock_path)));
    }

    return Err(anyhow!("no free X11 display number after 50 attempts"));
}

fn bind_to_socket(addr: &SocketAddr) -> anyhow::Result<UnixListener> {
    let listener = UnixListener::bind_addr(addr).context("error binding the socket")?;
    return Ok(listener);
}

#[cfg(target_os = "linux")]
fn bind_to_abstract_socket(display: u32) -> anyhow::Result<UnixListener> {
    use std::os::linux::net::SocketAddrExt;

    let name = format!("/tmp/.X11-unix/X{display}");
    let addr = SocketAddr::from_abstract_name(name).unwrap();
    return bind_to_socket(&addr);
}

fn bind_to_unix_socket(display: u32) -> anyhow::Result<(UnixListener, Unlink)> {
    let name = format!("/tmp/.X11-unix/X{display}");
    let addr = SocketAddr::from_pathname(&name).unwrap();
    // A socket file left behind by a crashed session is not connected to
    // anything any more: our lock file says the number is ours.
    let _ = unlink(&name);
    let guard = Unlink(name);
    return bind_to_socket(&addr).map(|listener| return (listener, guard));
}

fn open_display_sockets(
    display: u32,
) -> anyhow::Result<(Option<UnixListener>, UnixListener, Unlink)> {
    #[cfg(target_os = "linux")]
    let abstract_socket =
        Some(bind_to_abstract_socket(display).context("error binding the abstract socket")?);
    #[cfg(not(target_os = "linux"))]
    let abstract_socket = None;

    let (unix_socket, guard) =
        bind_to_unix_socket(display).context("error binding the unix socket")?;
    return Ok((abstract_socket, unix_socket, guard));
}

/// Create the sockets and the lock file of a free display number.
pub(crate) fn setup_connection() -> anyhow::Result<X11Connection> {
    ensure_x11_unix_dir()?;

    let mut n = 0;
    let mut attempt = 0;
    let (display, lock_guard, abstract_socket, unix_socket, unix_guard) = loop {
        let (display, lock_fd, lock_guard) = pick_x11_display(n)?;

        // The lock file holds the owning PID, like Xwayland writes it: tools
        // such as `xlsclients` read it to tell a live server from a stale lock.
        let pid_string = format!("{:>10}\n", getpid().as_raw_nonzero());
        if let Err(err) = write(&lock_fd, pid_string.as_bytes()) {
            return Err(err).context("error writing the PID to the X11 lock file");
        }
        drop(lock_fd);

        match open_display_sockets(display) {
            Ok((abstract_socket, unix_socket, guard)) => {
                break (display, lock_guard, abstract_socket, unix_socket, guard);
            }
            Err(err) => {
                // The lock said the number is free but the sockets say
                // otherwise: try the next one, a stale socket is out of reach.
                if attempt == 50 {
                    return Err(err)
                        .context("error opening the X11 sockets after taking the lock file");
                }
                n = display + 1;
                attempt += 1;
                continue;
            }
        }
    };

    return Ok(X11Connection {
        display_name: format!(":{display}"),
        abstract_fd: abstract_socket.map(OwnedFd::from),
        unix_fd: OwnedFd::from(unix_socket),
        _unix_guard: unix_guard,
        _lock_guard: lock_guard,
    });
}
