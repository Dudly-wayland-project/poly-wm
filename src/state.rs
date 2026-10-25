//! Top-level compositor state and event loop assembly.

use std::path::{Path, PathBuf};

use anyhow::Context;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use tracing::{debug, info};

use crate::backend::Backend;
use crate::cli::{BackendKind, RunArgs};
use crate::core::Core;
use crate::utils::{signals, spawn};

/// The compositor: a backend plus the backend-agnostic core.
pub struct State {
    /// How frames reach the screen and where input comes from.
    pub backend: Backend,
    /// Compositor state and wayland protocol state.
    pub core: Core,
    /// `ext-image-copy-capture` sessions (`crate::core::capture`). Kept out of
    /// [`Core`] so that a capture frame can borrow it and the core at the same
    /// time; the state holds GL objects, which cannot live in session user data.
    pub capture: crate::core::capture::CaptureState,
}

/// Create the display, start the backend and run the event loop.
pub fn run(args: RunArgs) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<'static, State> =
        EventLoop::try_new().context("error creating the event loop")?;

    // Block termination signals before any thread exists, so that every thread
    // (and every future one) inherits the mask. Children unblock again, see
    // `utils::spawn`.
    signals::block_early().context("error blocking termination signals")?;
    spawn::store_and_increase_nofile_rlimit();

    let display: Display<State> = Display::new().context("error creating the wayland display")?;
    let display_handle = display.handle();
    let socket_name = crate::core::display::init(&event_loop, display, &display_handle)?;
    spawn::set_child_env("WAYLAND_DISPLAY", &socket_name.to_string_lossy());
    // Children are running under Poly, which is what apps read to pick a
    // desktop integration (and what `poly portal on` writes into the portal's
    // environment). `environment` in the configuration overrides it.
    spawn::set_child_env("XDG_CURRENT_DESKTOP", crate::utils::portal::DESKTOP_NAME);

    signals::listen(&event_loop.handle())?;

    let kind = args.backend.resolve();

    // Configuration: locate and load, reporting every problem. A broken file
    // is not fatal — the compositor starts with the built-in defaults and the
    // errors in the log (`docs/plan.md` §11).
    let mut config_source = poly_config::locate_config(args.config.as_deref())?;
    // A first run has nothing to edit: write the annotated template so that
    // "edit `~/.config/poly/config.lua`" is true, and load it right away
    // instead of the built-in defaults.
    let generated = crate::core::config::write_default_if_missing(&config_source)?;
    if generated.is_some() {
        config_source = poly_config::locate_config(args.config.as_deref())?;
    }
    let loaded = crate::core::config::load(&config_source);
    let config_errors = crate::core::config::toast_messages(&loaded);
    // Best-effort `errors.dbus` from the file, which also exists when the file
    // did not load (`docs/plan.md` §14).
    let config_notifications = loaded.error_notifications;
    let config = loaded.config.unwrap_or_default();
    let layout_programs = loaded.layout_programs;
    // The `poly.include` closure, so the watcher knows which files to stat.
    let config_includes = loaded.includes;
    match config_source.path() {
        Some(path) => info!("configuration: {}", path.display()),
        None => info!("configuration: none found, using the built-in defaults"),
    }

    let mut core = Core::new(
        &event_loop,
        display_handle,
        config,
        layout_programs,
        config_source,
        default_mod_key(kind),
    )?;
    core.config_includes = config_includes;

    // The IPC socket lives next to the wayland socket and tells clients how to
    // reach us (`docs/plan.md` §12). Binding happens before the backend starts,
    // so a compositor that cannot create its socket fails before it takes over
    // the TTY.
    let ipc_path = ipc_socket_path(&socket_name)?;
    spawn::set_child_env(poly_ipc::SOCKET_ENV, &ipc_path.to_string_lossy());
    core.ipc.bind(&ipc_path)?;

    info!("starting the {kind:?} backend");
    let backend = Backend::init(&event_loop.handle(), &mut core, kind)?;

    let mut state = State {
        backend,
        core,
        capture: crate::core::capture::CaptureState::new(),
    };
    // The `outputs` section is applied before the first frame, so the very first
    // frame already shows the configured mode, scale, transform and background.
    crate::core::output::apply_config(&mut state);
    // X11 sockets come up before anything is spawned, so that the startup
    // commands and the `--` command already find `DISPLAY` (`docs/plan.md`
    // §13).
    crate::utils::xwayland::satellite::setup(&mut state);
    // Editing the file (or a file it includes) reloads it from now on.
    crate::core::config::watch(&mut state);
    state.core.queue_redraw_all();
    // Now that there is a `State` to answer with, the listener joins the loop.
    state.core.ipc.start(&event_loop.handle())?;
    // The clients that connect from here on get this from the first snapshot;
    // any that is already listening sees the event.
    crate::core::ipc::config_loaded(&mut state, !config_errors.is_empty());

    // Configuration problems found at startup are shown on screen: without a
    // terminal there is otherwise no trace of a typo (`docs/plan.md` §10.5).
    let now = std::time::Instant::now();
    // A first run says where the configuration it just wrote lives; a user
    // without a terminal would otherwise never find it.
    if let Some(path) = &generated {
        let message = format!("wrote a default configuration to {}", path.display());
        state.core.toasts.push(&message, now);
    }
    for message in &config_errors {
        state.core.toasts.push(message, now);
    }
    crate::core::notify::configuration_errors(&state, config_notifications, &config_errors);

    // Startup commands run once the compositor can be talked to.
    let startup_spawns = state.core.config.spawn_at_startup.clone();
    for entry in startup_spawns {
        match entry {
            poly_config::StartupSpawn::Exec(argv) => spawn::spawn(argv),
            poly_config::StartupSpawn::Shell(command) => spawn::spawn_sh(command),
        }
    }

    if !args.command.is_empty() {
        let command = args.command.join(" ");
        spawn::spawn_sh(command);
    }

    let ret = event_loop
        .run(None, &mut state, |state| {
            // A hotplug happened while a backend held only `&mut Core`; this is
            // the first point where the whole `State` is available again, so
            // `outputs` is applied here, before the frame below is rendered.
            if state.core.output_config_pending {
                state.core.output_config_pending = false;
                crate::core::output::apply_config(state);
            }
            // Capture sessions have to follow their output (stop when it goes
            // away, re-send constraints when it resizes), and both need the
            // whole `State` — the hotplug paths only had `&mut Core`.
            crate::core::handlers::refresh_sessions(state);
            // Periodic housekeeping, in niri's order: drop finished popup
            // grabs, abandon dead lock requests, re-evaluate keyboard focus,
            // then render and flush.
            crate::core::popups::refresh_popup_grab(state);
            crate::core::handlers::session_lock::refresh_lock_state(state);
            crate::core::focus::update_keyboard_focus(state);
            // The event stream is diffed after the focus bookkeeping, so that a
            // focus change is never one frame late.
            crate::core::ipc::refresh(state);
            state.idle_inhibit_cleanup();
            state.refresh_and_flush_clients()
        })
        .context("error running the event loop");

    drop(state);
    if let Err(err) = std::fs::remove_file(&ipc_path) {
        debug!("error removing the IPC socket: {err}");
    }

    info!("poly stopped");
    return ret;
}

/// `$XDG_RUNTIME_DIR/poly.$WAYLAND_DISPLAY.$PID.sock`.
fn ipc_socket_path(socket_name: &std::ffi::OsStr) -> anyhow::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is not set, cannot place the IPC socket")?;
    return Ok(crate::core::ipc::socket_path(
        Path::new(&runtime_dir),
        socket_name,
        std::process::id(),
    ));
}

/// The `Mod` key when the configuration does not pick one.
///
/// Nested in another compositor (winit) the host normally keeps Super for
/// itself, so Alt is the friendlier default there; on a real TTY session Super
/// matches niri's default.
fn default_mod_key(kind: BackendKind) -> poly_config::ModKey {
    return match kind {
        BackendKind::Winit | BackendKind::Auto => poly_config::ModKey::Alt,
        BackendKind::Udev | BackendKind::Headless => poly_config::ModKey::Super,
    };
}
