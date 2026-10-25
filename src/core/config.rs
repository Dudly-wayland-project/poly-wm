//! Configuration lifecycle inside the compositor: the startup load, problem
//! reporting and `reload-config`.
//!
//! The Lua loader itself lives in the `poly-config` crate; this module is the
//! compositor-side half (`docs/plan.md` §11). Loading never fails startup: a
//! broken file means the built-in defaults plus errors in the log (and, from
//! M3, a Toast carrying the same messages).

use poly_config::{Config, ConfigLoadResult, ConfigSource};
use smithay::input::keyboard::XkbConfig;
use tracing::{debug, error, info, warn};

use crate::state::State;
use crate::utils::spawn;

/// Write the annotated default configuration when there is none, returning
/// where it went (`docs/plan.md` §11).
///
/// Only a first run — a user with `--config`, `$POLY_CONFIG` or an existing
/// file is never touched — and the file is created with `O_EXCL`, so two
/// compositors starting together do not race.
pub fn write_default_if_missing(
    source: &ConfigSource,
) -> Result<Option<std::path::PathBuf>, std::io::Error> {
    let written = poly_config::write_default(source)?;
    if let Some(path) = &written {
        info!("wrote the default configuration to {}", path.display());
    }
    return Ok(written);
}

/// Load the configuration and log every problem it has.
pub fn load(source: &ConfigSource) -> ConfigLoadResult {
    let result = poly_config::load(source);
    report(&result);
    return result;
}

/// Report a load outcome: problems at `error!`, unknown settings at `warn!`.
pub fn report(result: &ConfigLoadResult) {
    for warning in &result.warnings {
        warn!("configuration: {warning}");
    }
    let Err(err) = &result.config else {
        return;
    };
    for line in err.to_string().lines() {
        error!("configuration: {line}");
    }
}

/// The same problems as [`report`], shaped for Toasts: one message per
/// warning, and one for every error (multi-line, the Toast wraps it).
pub fn toast_messages(result: &ConfigLoadResult) -> Vec<String> {
    let mut messages = Vec::new();
    for warning in &result.warnings {
        messages.push(format!("configuration: {warning}"));
    }
    if let Err(err) = &result.config {
        messages.push(err.to_string().trim_end().to_owned());
    }
    return messages;
}

/// Show a load outcome on screen. Used at startup and on `reload-config`, so
/// a broken file is visible without a terminal (`docs/plan.md` §10.5).
pub fn show_toasts(state: &mut State, result: &ConfigLoadResult) {
    let now = std::time::Instant::now();
    let messages = toast_messages(result);
    for message in &messages {
        state.core.toasts.push(message, now);
    }
    if !state.core.toasts.is_empty() {
        state.core.queue_redraw_all();
    }
    // `errors = { dbus = true }` mirrors the same report to the desktop.
    crate::core::notify::configuration_errors(state, result.error_notifications, &messages);
}

/// `reload-config`: re-read the configuration and apply it.
///
/// A failed load keeps the previous configuration running; the file watcher
/// calls the same function, so an editor's mistake never takes the session
/// down.
pub fn reload(state: &mut State) {
    let source = state.core.config_source.clone();
    info!("reloading the configuration");
    let result = poly_config::load(&source);
    report(&result);
    show_toasts(state, &result);
    let failed = result.config.is_err();
    match result.config {
        Ok(config) => {
            apply_config(state, config, result.includes, result.layout_programs);
        }
        Err(_) => {
            error!("configuration reload failed; keeping the previous one");
        }
    }
    // Rearm on both paths: a failed load still has to keep watching the root
    // file, so that fixing the typo reloads again.
    watch(state);
    // Any client watching the configuration (a status bar showing a typo) learns
    // about it here, including a reload that changed nothing.
    crate::core::ipc::config_loaded(state, failed);
}

/// Start watching the root file and every file it includes.
///
/// Called after each successful load, so the include closure is always the one
/// the running configuration actually read.
pub fn watch(state: &mut State) {
    let mut files = Vec::new();
    if let Some(root) = state.core.config_source.watch_path() {
        files.push(root.to_owned());
    }
    // `includes` can repeat the root file (it is part of the closure the loader
    // reports); watching a file twice would only cost a `stat`.
    for include in &state.core.config_includes {
        if !files.contains(include) {
            files.push(include.clone());
        }
    }
    state.core.config_watcher.watch(files);
    debug!(
        "watching {} file(s) for changes: {}",
        state.core.config_watcher.paths().count(),
        state
            .core
            .config_watcher
            .paths()
            .map(|path| return path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// The 500 ms housekeeping poll: reload when a watched file was saved.
///
/// Polling instead of `inotify` keeps this working on any filesystem and for a
/// file that does not exist yet; the cost is one `stat` per watched file per
/// half second (`docs/plan.md` §15).
pub fn poll_watch(state: &mut State) {
    let Some(path) = state.core.config_watcher.changed() else {
        return;
    };
    info!("configuration file {} changed", path.display());
    reload(state);
}

/// Make a freshly loaded configuration the live one.
///
/// Bindings, input settings and the child environment switch right away; the
/// rest is read when it is needed (the layout engine in M3, and so on).
pub fn apply_config(
    state: &mut State,
    config: Config,
    includes: Vec<std::path::PathBuf>,
    layout_programs: Option<poly_config::LayoutPrograms>,
) {
    apply_child_environment(&config);

    state.core.config_includes = includes;
    state.core.layout.set_config(&config, layout_programs);
    state.core.config = config;
    let mod_key = state
        .core
        .config
        .general
        .mod_key
        .unwrap_or(state.core.default_mod_key);
    state.core.mod_key = mod_key;
    if state.core.config.window_drag.axis_lock == Some(mod_key) {
        warn!("window_drag.axis_lock uses the compositor modifier; axis lock is disabled");
        state.core.config.window_drag.axis_lock = None;
    }

    apply_keyboard(state);
    crate::core::layout::LayoutManager::relayout(state);
    // `outputs` is a full re-apply: `reload-config` is how runtime changes made
    // with `poly msg output` are put back to the file's values (`docs/plan.md`
    // §12).
    crate::core::output::apply_config(state);
    // A reload may add, remove or edit shader files; a new generation makes
    // `Shaders::prepare` recompile on the next frame (`docs/plan.md` §10.3).
    state.core.config_generation = state.core.config_generation.wrapping_add(1);
    state.core.shaders.mark_dirty();
    // `xwayland.enable` can be turned on by an edit; a satellite that is
    // already set up is left alone (switching it off needs a restart, the X11
    // sockets cannot be handed to somebody else and taken back).
    crate::utils::xwayland::satellite::setup(state);
    state.core.queue_redraw_all();
}

/// Push `environment` into the environment of every future child (`docs/
/// plan.md` §11: no mutation of our own environment, which is unsound once
/// threads exist).
pub fn apply_child_environment(config: &Config) {
    for (name, value) in &config.environment {
        spawn::set_child_env(name, value);
    }
}

/// Apply `input.keyboard` to the live keyboard: keymap and repeat settings.
fn apply_keyboard(state: &mut State) {
    let Some(keyboard) = state.core.seat.get_keyboard() else {
        return;
    };
    let config = state.core.config.input.keyboard.clone();

    let xkb_config = XkbConfig {
        rules: &config.rules,
        model: &config.model,
        layout: &config.layout,
        variant: &config.variant,
        options: config.options.clone(),
    };
    if let Err(err) = keyboard.set_xkb_config(state, xkb_config) {
        error!("invalid keyboard configuration ({err}); keeping the previous keymap");
    }
    keyboard.change_repeat_info(
        i32::from(config.repeat_rate),
        i32::from(config.repeat_delay),
    );
}
