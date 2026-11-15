// Ported from niri (GPL-3.0-or-later): src/niri.rs (update_keyboard_focus)
//
//! The keyboard focus model.
//!
//! Focus is not simply "who was clicked last": session-lock surfaces take
//! everything, popup grabs keep their root focused, on-demand layer surfaces
//! (launchers) hold focus until dismissed, exclusive layer surfaces (lock
//! screens, overlays) beat windows, and windows remember their own focus so
//! that closing a launcher gives the keyboard back to the window that had it.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, SERIAL_COUNTER};
use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer};
use tracing::trace;

use crate::core::handlers::layer_shell::{effective_keyboard_interactivity, layer_and_output};
use crate::state::State;

/// The root surface of the window that should receive the keyboard, if any.
pub fn focused_window_root(state: &State) -> Option<WlSurface> {
    let root = state.core.last_window_focus.as_ref()?;
    if !root.alive() {
        return None;
    }
    // `window_for_root` still finds minimized windows (they stay managed), but
    // they are not on screen and must not hold the keyboard.
    let managed = state.core.layout.by_root(root)?;
    if managed.minimized {
        return None;
    }
    return Some(root.clone());
}

/// The layer surface holding on-demand focus, if it is still usable.
fn on_demand_focus(state: &State) -> Option<WlSurface> {
    let surface = state.core.layer_focus.as_ref()?;
    if !surface.alive() {
        return None;
    }
    let (output, layer) = layer_and_output(state, surface)?;
    if layer.layer() == Layer::Top
        && state.core.config.fullscreen.cover_top_layer
        && state.core.layout.has_fullscreen_on(&output)
    {
        return None;
    }
    if effective_keyboard_interactivity(state, &layer) != KeyboardInteractivity::OnDemand {
        return None;
    }
    return Some(surface.clone());
}

/// The topmost mapped layer surface of one layer, if it takes exclusive focus.
fn exclusive_focus(state: &State, layer: Layer) -> Option<WlSurface> {
    for output in state.core.space.outputs() {
        let map = smithay::desktop::layer_map_for_output(output);
        for layer_surface in map.layers_on(layer).rev() {
            if map.layer_geometry(layer_surface).is_none() {
                continue;
            }
            if layer == Layer::Top
                && state.core.config.fullscreen.cover_top_layer
                && state.core.layout.has_fullscreen_on(output)
            {
                continue;
            }
            if effective_keyboard_interactivity(state, layer_surface)
                != KeyboardInteractivity::Exclusive
            {
                continue;
            }
            return Some(layer_surface.wl_surface().clone());
        }
    }
    return None;
}

/// Whether a layer surface outranks windows for the keyboard right now.
pub fn higher_priority_layer_focus(state: &State) -> bool {
    if on_demand_focus(state).is_some() {
        return true;
    }
    for layer in [Layer::Overlay, Layer::Top] {
        if exclusive_focus(state, layer).is_some() {
            return true;
        }
    }
    return false;
}

/// The lock surface that should hold the keyboard while the session is locked.
fn lock_focus_surface(state: &State) -> Option<WlSurface> {
    // Prefer the lock surface on the output the pointer is on.
    let location = state.core.pointer_location();
    if let Some(output) = state.core.output_at(location) {
        if let Some((_, lock_surface)) = state
            .core
            .lock_surfaces
            .iter()
            .find(|(lock_output, _)| return lock_output == &output)
        {
            return Some(lock_surface.wl_surface().clone());
        }
    }
    let (_, lock_surface) = state.core.lock_surfaces.first()?;
    return Some(lock_surface.wl_surface().clone());
}

/// Who should hold the keyboard right now.
pub fn keyboard_focus(state: &State) -> Option<WlSurface> {
    if state.core.lock_state.is_locked() {
        return lock_focus_surface(state);
    }

    // A keyboard-grabbing popup keeps its root (the toplevel that opened it)
    // focused; the grab itself routes events into the popup tree.
    if let Some(grab) = &state.core.popup_grab {
        if grab.has_keyboard_grab {
            return Some(grab.root.clone());
        }
    }

    if let Some(surface) = on_demand_focus(state) {
        return Some(surface);
    }

    for layer in [Layer::Overlay, Layer::Top] {
        if let Some(surface) = exclusive_focus(state, layer) {
            return Some(surface);
        }
    }

    if let Some(root) = focused_window_root(state) {
        return Some(root);
    }

    for layer in [Layer::Bottom, Layer::Background] {
        if let Some(surface) = exclusive_focus(state, layer) {
            return Some(surface);
        }
    }

    return None;
}

/// Give the keyboard to whoever the focus model points at.
///
/// Called from the event loop and whenever something meaningful changed;
/// explicit focus changes from clicks go through
/// [`crate::core::window::focus_window`] and [`focus_layer_surface`].
pub fn update_keyboard_focus(state: &mut State) {
    let focus = keyboard_focus(state);

    let current = state
        .core
        .seat
        .get_keyboard()
        .and_then(|keyboard| return keyboard.current_focus());
    if current == focus {
        return;
    }

    trace!(?focus, ?current, "updating keyboard focus");
    let serial = SERIAL_COUNTER.next_serial();
    if let Some(keyboard) = state.core.seat.get_keyboard() {
        keyboard.set_focus(state, focus, serial);
    }
}

/// Focus a window's root surface and remember it as the window focus.
pub fn set_window_focus(state: &mut State, root: Option<WlSurface>) {
    state.core.last_window_focus = root;
    state.core.layer_focus = None;
}

/// Give on-demand focus to a layer surface (called on click).
pub fn focus_layer_surface(state: &mut State, surface: &WlSurface) {
    state.core.layer_focus = Some(surface.clone());
    update_keyboard_focus(state);
}
