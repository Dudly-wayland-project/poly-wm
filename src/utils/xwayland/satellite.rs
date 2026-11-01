// Ported from niri (GPL-3.0-or-later): src/utils/xwayland/satellite.rs
// Copyright (c) Ivan Molodetskikh and niri contributors.

//! Starting `xwayland-satellite` when an X11 client needs it.
//!
//! An X11 client announces itself by connecting to one of the sockets
//! [`super::setup_connection`] created. Nothing accepts those connections until
//! the satellite runs, so the very same sockets are the trigger: once one of
//! them turns readable, the satellite is started on a helper thread and given
//! the socket descriptors with `-listenfd`, and it accepts the waiting client
//! itself.
//!
//! The satellite is expected to crash or be killed at some point (it is also
//! how a user picks up a new version), so an exit is not an error: the socket
//! watches are re-armed and the next client starts it again.
//!
//! `xwayland-satellite` only supports this through `--test-listenfd-support`,
//! which is probed once at startup: a version that cannot inherit the sockets
//! would leave X11 clients hanging forever, so the integration is switched off
//! (with a Toast) instead.

use std::io::{BufRead as _, BufReader};
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel::Sender;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, Mode, PostAction, RegistrationToken};
use smithay::reexports::rustix::io::{FdFlags, fcntl_setfd};
use tracing::{debug, info, warn};

use crate::state::State;
use crate::utils::spawn;
use crate::utils::xwayland::{X11Connection, expand_tilde};

/// A satellite that exits within this long after being started counts as a
/// failed start, not as a crash after a working session.
const FAST_FAILURE: Duration = Duration::from_secs(2);
/// How many failed starts in a row a satellite that is meant to keep running
/// gets before the integration falls back to on-demand activation.
const MAX_FAST_FAILURES: u32 = 3;
/// How long to wait before starting a satellite again in "keep it running"
/// mode, so that a satellite that cannot start does not turn into a busy loop.
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// The X11 integration: the sockets and the watch that starts the satellite.
pub struct Satellite {
    /// The sockets X11 clients connect to, and the lock file for the display.
    x11: X11Connection,
    /// The watch on the abstract socket, while nobody is serving it.
    abstract_token: Option<RegistrationToken>,
    /// The watch on the filesystem socket, while nobody is serving it.
    unix_token: Option<RegistrationToken>,
    /// A pending restart, in "keep it running" mode.
    respawn_timer: Option<RegistrationToken>,
    /// Lets the spawner thread report that the satellite is gone.
    to_main: Sender<ToMain>,
    /// Whether an `xwayland-satellite` process is running right now.
    running: bool,
    /// Whether the satellite is (now) started on demand rather than kept
    /// running. Starts as the configured value and turns on after the satellite
    /// fails to start often enough; `poly msg xwayland` reports this one.
    on_demand: bool,
    /// Consecutive starts that failed within [`FAST_FAILURE`].
    fast_failures: u32,
}

/// What the spawner thread tells the compositor.
enum ToMain {
    /// The satellite is gone; `ran_for` is `None` when it never started.
    Exited { ran_for: Option<Duration> },
}

/// Which of the two watches fired.
#[derive(Clone, Copy)]
enum Watch {
    Abstract,
    Unix,
}

impl Satellite {
    /// The value X11 clients read from `DISPLAY` (`:0`, `:1`, …).
    pub fn display_name(&self) -> &str {
        return &self.x11.display_name;
    }

    /// Whether the satellite process is running right now.
    pub fn is_running(&self) -> bool {
        return self.running;
    }

    /// Whether the first client starts the satellite, and only then.
    pub fn is_on_demand(&self) -> bool {
        return self.on_demand;
    }
}

/// Set the X11 integration up, if the configuration wants it.
///
/// Called at startup, and again after a configuration reload, so that turning
/// `xwayland.enable` on in an editor starts it without a restart. Idempotent:
/// calling it while the integration exists only refreshes the `DISPLAY` that
/// children are spawned with.
pub fn setup(state: &mut State) {
    if let Some(satellite) = &state.core.satellite {
        // Every child spawned from now on — after a reload, too — sees it.
        spawn::set_child_env("DISPLAY", satellite.display_name());
        return;
    }

    let config = state.core.config.xwayland.clone();
    if !config.enable {
        debug!("xwayland: disabled by the configuration");
        return;
    }

    if let Some(reason) = test_ondemand(&config.satellite_path) {
        warn!("{reason}");
        state.core.toasts.push(&reason, Instant::now());
        return;
    }

    let x11 = match super::setup_connection() {
        Ok(x11) => x11,
        Err(err) => {
            // Not fatal: a compositor without X11 still runs every Wayland
            // client, and the reason reaches the user on screen.
            let message = format!("xwayland: error opening the X11 sockets: {err:?}");
            warn!("{message}");
            state.core.toasts.push(&message, Instant::now());
            return;
        }
    };
    info!("xwayland: DISPLAY={}", x11.display_name);
    spawn::set_child_env("DISPLAY", &x11.display_name);

    let handle = state.core.event_loop_handle.clone();
    let (to_main, rx) = calloop::channel::channel();
    if let Err(err) = handle.insert_source(rx, |event, _, state: &mut State| match event {
        calloop::channel::Event::Msg(ToMain::Exited { ran_for }) => on_exit(state, ran_for),
        calloop::channel::Event::Closed => (),
    }) {
        warn!("xwayland: error inserting the satellite channel source: {err:?}");
        return;
    }

    let eager = config.spawn_at_startup || !config.on_demand;
    state.core.satellite = Some(Satellite {
        x11,
        abstract_token: None,
        unix_token: None,
        respawn_timer: None,
        to_main,
        running: false,
        // `spawn_at_startup` only makes the first start eager: an exit still
        // re-arms the watch instead of restarting the satellite.
        on_demand: config.on_demand,
        fast_failures: 0,
    });

    if eager {
        // `on_demand` off means "keep it running": start it before anybody
        // connects, and restart it when it goes away.
        spawn_process(state);
    } else {
        setup_watch(state);
    }
}

/// Why the satellite cannot inherit our listening sockets, if it cannot.
///
/// `None` means the probe passed.
fn test_ondemand(path: &str) -> Option<String> {
    let path = expand_tilde(path);

    let mut process = Command::new(&path);
    process
        .args([":0", "--test-listenfd-support"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE");

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Some(format!(
                "xwayland: cannot run {}: {err}, X11 clients stay unavailable",
                path.display()
            ));
        }
    };

    let status = match child.wait() {
        Ok(status) => status,
        Err(err) => {
            return Some(format!(
                "xwayland: cannot run {}: {err}, X11 clients stay unavailable",
                path.display()
            ));
        }
    };

    if !status.success() {
        // Older releases cannot take over the sockets, which is what makes
        // starting the satellite *on connection* possible at all.
        return Some(format!(
            "xwayland: {} does not support on-demand activation ({status}), X11 clients stay unavailable",
            path.display()
        ));
    }

    return None;
}

/// Arm the watches that start the satellite when a client connects.
///
/// When `xwayland-satellite` fails before it accepts the pending connection,
/// the socket stays readable: without clearing the backlog the loop would
/// restart the satellite immediately, over and over. The pending clients are
/// dropped instead — a satellite that could not accept them a moment ago will
/// not do better now — and the next connection starts it again.
fn setup_watch(state: &mut State) {
    let handle = state.core.event_loop_handle.clone();
    let Some(satellite) = state.core.satellite.as_mut() else {
        return;
    };

    if let Some(token) = satellite.abstract_token.take() {
        warn!("xwayland: the abstract socket watch was still registered; removing it");
        handle.remove(token);
    }
    if let Some(token) = satellite.unix_token.take() {
        warn!("xwayland: the unix socket watch was still registered; removing it");
        handle.remove(token);
    }

    if let Some(fd) = &satellite.x11.abstract_fd {
        let fd = match fd.try_clone() {
            Ok(fd) => fd,
            Err(err) => {
                warn!("xwayland: error duplicating the abstract socket: {err}");
                return;
            }
        };
        let source = Generic::new(
            clear_out_pending_connections(fd),
            Interest::READ,
            Mode::Level,
        );
        let token = handle.insert_source(source, |_, _, state: &mut State| {
            on_connection(state, Watch::Abstract);
            return Ok(PostAction::Remove);
        });
        match token {
            Ok(token) => satellite.abstract_token = Some(token),
            Err(err) => {
                warn!("xwayland: error watching the abstract socket: {err:?}");
                return;
            }
        }
    }

    let fd = match satellite.x11.unix_fd.try_clone() {
        Ok(fd) => fd,
        Err(err) => {
            warn!("xwayland: error duplicating the unix socket: {err}");
            return;
        }
    };
    let source = Generic::new(
        clear_out_pending_connections(fd),
        Interest::READ,
        Mode::Level,
    );
    match handle.insert_source(source, |_, _, state: &mut State| {
        on_connection(state, Watch::Unix);
        return Ok(PostAction::Remove);
    }) {
        Ok(token) => satellite.unix_token = Some(token),
        Err(err) => warn!("xwayland: error watching the unix socket: {err:?}"),
    }
}

/// Accept and drop every connection waiting on the socket.
///
/// Returns the descriptor so it can be registered again.
fn clear_out_pending_connections(fd: OwnedFd) -> OwnedFd {
    let listener = UnixListener::from(fd);

    if let Err(err) = listener.set_nonblocking(true) {
        warn!("xwayland: error making the X11 socket non-blocking: {err:?}");
        return OwnedFd::from(listener);
    }

    while listener.accept().is_ok() {}

    if let Err(err) = listener.set_nonblocking(false) {
        warn!("xwayland: error making the X11 socket blocking again: {err:?}");
    }

    return OwnedFd::from(listener);
}

/// A client connected to a watched socket: start the satellite for it.
fn on_connection(state: &mut State, from: Watch) {
    let handle = state.core.event_loop_handle.clone();
    let Some(satellite) = state.core.satellite.as_mut() else {
        return;
    };

    // The source that fired is removed by its own `PostAction::Remove`; the
    // other one has to go too, or the same connection would start a second
    // satellite through it.
    match from {
        Watch::Abstract => satellite.abstract_token = None,
        Watch::Unix => satellite.unix_token = None,
    }
    if let Some(token) = satellite.abstract_token.take() {
        handle.remove(token);
    }
    if let Some(token) = satellite.unix_token.take() {
        handle.remove(token);
    }

    debug!("xwayland: a client connected to the X11 socket, starting the satellite");
    spawn_process(state);
}

/// Start the satellite on a helper thread.
///
/// The caller guarantees that no socket watch is registered any more: while the
/// satellite runs, the sockets belong to it.
fn spawn_process(state: &mut State) {
    let path = state.core.config.xwayland.satellite_path.clone();
    let handle = state.core.event_loop_handle.clone();
    let Some(satellite) = state.core.satellite.as_mut() else {
        return;
    };
    if satellite.running {
        return;
    }
    // A pending restart is superseded by this start.
    if let Some(token) = satellite.respawn_timer.take() {
        handle.remove(token);
    }

    let abstract_fd = match &satellite.x11.abstract_fd {
        Some(fd) => match fd.try_clone() {
            Ok(fd) => Some(fd),
            Err(err) => {
                warn!("xwayland: error duplicating the abstract socket: {err}");
                return;
            }
        },
        None => None,
    };
    let unix_fd = match satellite.x11.unix_fd.try_clone() {
        Ok(fd) => fd,
        Err(err) => {
            warn!("xwayland: error duplicating the unix socket: {err}");
            return;
        }
    };
    let display_name = satellite.x11.display_name.clone();
    let to_main = satellite.to_main.clone();
    let path = expand_tilde(&path);

    let process = build_command(&path, &display_name);
    satellite.running = true;

    // Spawning and waiting takes a moment, so it happens off the main thread;
    // the satellite then runs for as long as X11 clients need it.
    let spawned = thread::Builder::new()
        .name("xwl-s spawner".to_owned())
        .spawn(move || {
            let ran_for = spawn_and_wait(&path, process, abstract_fd, unix_fd);
            let _ = to_main.send(ToMain::Exited { ran_for });
        });

    if let Err(err) = spawned {
        warn!("xwayland: error starting the spawner thread: {err:?}");
        satellite.running = false;
        let _ = satellite.to_main.send(ToMain::Exited { ran_for: None });
    }
}

/// The satellite command line, before the socket descriptors are added.
fn build_command(path: &PathBuf, display_name: &str) -> Command {
    let mut process = Command::new(path);
    // The satellite sets `DISPLAY` for the Xwayland it starts; it must not
    // inherit ours (which is the satellite's own), nor the host's when nested.
    process.arg(display_name).env_remove("DISPLAY");
    // The satellite is a Wayland client of *this* compositor: `utils::spawn`
    // keeps that socket in a child-only environment, so pass it on explicitly.
    if let Some(display) = spawn::child_env_value("WAYLAND_DISPLAY") {
        process.env("WAYLAND_DISPLAY", display);
    }
    // A `WAYLAND_SOCKET` from the session we are nested in belongs to the host.
    process
        .env_remove("WAYLAND_SOCKET")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    return process;
}

/// Pass the listening sockets to the satellite, wait for it, and report how
/// long it ran.
///
/// `None` means the process never started.
fn spawn_and_wait(
    path: &PathBuf,
    mut process: Command,
    abstract_fd: Option<OwnedFd>,
    unix_fd: OwnedFd,
) -> Option<Duration> {
    let abstract_raw = abstract_fd.as_ref().map(|fd| return fd.as_raw_fd());
    let unix_raw = unix_fd.as_raw_fd();

    process.arg("-listenfd").arg(unix_raw.to_string());
    if let Some(abstract_raw) = abstract_raw {
        process.arg("-listenfd").arg(abstract_raw.to_string());
    }

    // SAFETY: the closure runs between fork and exec, where only
    // async-signal-safe calls are allowed: unblocking signals, restoring the
    // rlimit, and `fcntl` on descriptors we hold.
    unsafe {
        process.pre_exec(move || {
            crate::utils::signals::unblock_all()?;
            spawn::restore_nofile_rlimit();
            // The descriptors must survive exec for the satellite to inherit
            // them; they stay open here until after `spawn` has returned.
            let unix_fd = BorrowedFd::borrow_raw(unix_raw);
            fcntl_setfd(unix_fd, FdFlags::empty())?;
            if let Some(abstract_raw) = abstract_raw {
                let abstract_fd = BorrowedFd::borrow_raw(abstract_raw);
                fcntl_setfd(abstract_fd, FdFlags::empty())?;
            }
            Ok(())
        });
    }

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!("xwayland: error starting {path:?}: {err}");
            return None;
        }
    };
    let started = Instant::now();
    // Spawned: our copies of the sockets can go, the satellite has its own.
    drop(abstract_fd);
    drop(unix_fd);

    // The satellite's own messages are the only clue when X11 misbehaves, so
    // they are forwarded to our log instead of being thrown away.
    if let Some(stderr) = child.stderr.take() {
        thread::Builder::new()
            .name("xwl-s log".to_owned())
            .spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    match line {
                        Ok(line) => debug!("xwayland-satellite: {line}"),
                        Err(err) => {
                            debug!("xwayland: error reading the satellite output: {err}");
                            return;
                        }
                    }
                }
            })
            .ok();
    }

    let status = match child.wait() {
        Ok(status) => status,
        Err(err) => {
            warn!("xwayland: error waiting for {path:?}: {err}");
            return Some(started.elapsed());
        }
    };

    // Most likely a crash, or the user replacing the binary: not an error of
    // ours, but worth a line in the log.
    warn!("xwayland: {path:?} exited with {status}");
    return Some(started.elapsed());
}

/// The satellite went away: start it again, or wait for the next client.
fn on_exit(state: &mut State, ran_for: Option<Duration>) {
    let handle = state.core.event_loop_handle.clone();

    let (on_demand, gave_up) = {
        let Some(satellite) = state.core.satellite.as_mut() else {
            return;
        };
        satellite.running = false;
        let fast = match ran_for {
            Some(ran_for) => ran_for < FAST_FAILURE,
            None => true,
        };
        match fast {
            true => satellite.fast_failures = satellite.fast_failures.saturating_add(1),
            false => satellite.fast_failures = 0,
        }
        // Only a satellite that is supposed to keep running can give up: with
        // on-demand activation the next client is the next attempt.
        let gave_up = !satellite.on_demand && satellite.fast_failures >= MAX_FAST_FAILURES;
        if gave_up {
            // Stop respawning a program that does not work: from here on, the
            // next client decides when to try again, and the log stays quiet.
            satellite.on_demand = true;
        }
        (satellite.on_demand, gave_up)
    };

    if gave_up {
        let message =
            "xwayland: the satellite keeps failing to start, X11 now starts on demand".to_owned();
        warn!("{message}");
        state.core.toasts.push(&message, Instant::now());
        setup_watch(state);
        return;
    }

    if on_demand {
        // The next client to connect starts it again.
        setup_watch(state);
        return;
    }

    let timer = Timer::from_duration(RESTART_DELAY);
    let token = handle.insert_source(timer, |_, _, state: &mut State| {
        if let Some(satellite) = state.core.satellite.as_mut() {
            satellite.respawn_timer = None;
        }
        spawn_process(state);
        return TimeoutAction::Drop;
    });
    match token {
        Ok(token) => {
            if let Some(satellite) = state.core.satellite.as_mut() {
                satellite.respawn_timer = Some(token);
            }
        }
        Err(err) => warn!("xwayland: error arming the satellite restart: {err:?}"),
    }
}
