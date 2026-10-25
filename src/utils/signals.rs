// Ported from niri (GPL-3.0-or-later): src/utils/signals.rs
// Copyright (c) Ivan Molodetskikh and niri contributors.

//! Termination signal handling.
//!
//! We block SIGINT/SIGTERM/SIGHUP early (so every thread inherits the mask) and
//! read them through a calloop signalfd source. Spawned children get an empty
//! mask again (see [`unblock_all`] and `utils::spawn`).

use std::{io, mem};

use anyhow::{Context, anyhow};
use calloop::LoopHandle;
use calloop::signals::{Signal, Signals};
use tracing::info;

use crate::state::State;

/// Route termination signals into the event loop: they stop the compositor.
pub fn listen(handle: &LoopHandle<'static, State>) -> anyhow::Result<()> {
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP])
        .context("error creating signalfd")?;
    handle
        .insert_source(signals, |event, _, state| {
            info!("quitting due to receiving signal {:?}", event.signal());
            state.core.stop_signal.stop();
        })
        .map_err(|err| return anyhow!("error inserting signal source: {}", err.error))?;
    return Ok(());
}

/// Block termination signals for this and all future threads.
pub fn block_early() -> io::Result<()> {
    return set_sigmask(&preferred_sigset()?);
}

/// Clear the signal mask. Async-signal-safe; used in `pre_exec`.
pub fn unblock_all() -> io::Result<()> {
    return set_sigmask(&empty_sigset()?);
}

fn empty_sigset() -> io::Result<libc::sigset_t> {
    let mut sigset = mem::MaybeUninit::uninit();
    // SAFETY: sigemptyset initializes the pointed-to set; we only read it on success.
    if unsafe { libc::sigemptyset(sigset.as_mut_ptr()) } == 0 {
        // SAFETY: initialized by the successful sigemptyset above.
        return Ok(unsafe { sigset.assume_init() });
    }
    return Err(io::Error::last_os_error());
}

fn preferred_sigset() -> io::Result<libc::sigset_t> {
    let mut set = empty_sigset()?;
    for signum in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `set` is an initialized sigset and `signum` is a valid signal number.
        if unsafe { libc::sigaddset(&mut set, signum) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    return Ok(set);
}

fn set_sigmask(set: &libc::sigset_t) -> io::Result<()> {
    // SAFETY: `set` points to an initialized sigset; the old mask is not requested.
    let rv = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, set, std::ptr::null_mut()) };
    if rv == 0 {
        return Ok(());
    }
    return Err(io::Error::from_raw_os_error(rv));
}
