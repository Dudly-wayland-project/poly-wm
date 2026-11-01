// Ported from niri (GPL-3.0-or-later): src/handlers/xdg_shell.rs (grab, unconstrain_popup)
//
//! Popup placement and popup grabs.
//!
//! Popups are positioned by their client-provided positioner, which Poly nudges
//! so the popup stays inside the parent's output. A popup that asked for an
//! input grab takes over the pointer (and the keyboard, if its root holds
//! focus) until the user clicks outside or presses Escape.

use smithay::desktop::{
    PopupGrab, PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab,
    find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
};
use smithay::input::pointer::Focus;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle, Serial};
use smithay::wayland::input_method::InputMethodSeat;
use smithay::wayland::shell::xdg::PopupSurface;
use tracing::{trace, warn};

use crate::core::focus::update_keyboard_focus;
use crate::core::handlers::layer_shell::{layer_and_output, work_area};
use crate::core::window::{root_surface, window_for_root};
use crate::state::State;

/// A running popup grab.
pub struct PopupGrabState {
    /// The toplevel (or layer surface) the grab belongs to.
    pub root: WlSurface,
    /// The grab itself; ends when the popup tree dies or the user clicks away.
    pub grab: PopupGrab<State>,
    /// Whether this grab also took the keyboard.
    pub has_keyboard_grab: bool,
}

/// Handle a client's request to grab input for a popup (`xdg_popup.grab`).
pub fn start_popup_grab(state: &mut State, popup: PopupSurface, _seat: &WlSeat, serial: Serial) {
    let popup_kind = PopupKind::Xdg(popup.clone());
    let Ok(root) = find_popup_root_surface(&popup_kind) else {
        warn!("popup grab without a root surface");
        return;
    };

    // A window only gets the grab while it holds keyboard focus; a layer
    // surface decides through its keyboard interactivity.
    let keyboard_focus = state
        .core
        .seat
        .get_keyboard()
        .and_then(|keyboard| return keyboard.current_focus());
    let can_receive_keyboard_focus = if let Some((_, layer)) = layer_and_output(state, &root) {
        layer.can_receive_keyboard_focus()
    } else if window_for_root(state, &root).is_some() {
        let focused = keyboard_focus
            .as_ref()
            .is_some_and(|focus| return root_surface(focus) == root);
        if !focused {
            trace!("ignoring popup grab of an unfocused window");
            let _ = PopupManager::dismiss_popup(&root, &popup_kind);
            return;
        }
        // An on-demand launcher or exclusive layer surface outranks windows.
        if crate::core::focus::higher_priority_layer_focus(state) {
            trace!("ignoring popup grab because a layer surface holds focus");
            let _ = PopupManager::dismiss_popup(&root, &popup_kind);
            return;
        }
        true
    } else {
        trace!("ignoring popup grab of an unknown surface");
        return;
    };

    let grab =
        match state
            .core
            .popups
            .grab_popup(root.clone(), popup_kind, &state.core.seat, serial)
        {
            Ok(grab) => grab,
            Err(err) => {
                trace!("ignoring popup grab: {err:?}");
                return;
            }
        };

    let keyboard = state.core.seat.get_keyboard();
    let pointer = state.core.seat.get_pointer();

    // Smithay cannot stack grabs, so an input-method keyboard grab keeps the
    // keyboard: the popup still works, the user just cannot type into it while
    // the IME is composing (same behavior as niri).
    let mut has_keyboard_grab = false;
    if can_receive_keyboard_focus {
        let keyboard_free = keyboard.as_ref().is_some_and(|keyboard| {
            return !keyboard.is_grabbed() || keyboard.has_grab(serial);
        });
        let ime_grabbed = state.core.seat.input_method().keyboard_grabbed();
        if let Some(keyboard) = &keyboard {
            if keyboard_free && !ime_grabbed {
                keyboard.set_grab(state, PopupKeyboardGrab::new(&grab), serial);
                has_keyboard_grab = true;
            }
        }
    }

    if let Some(pointer) = &pointer {
        // `Focus::Keep`: the client already saw the press that opened the menu.
        pointer.set_grab(state, PopupPointerGrab::new(&grab), serial, Focus::Keep);
    }

    trace!(?root, has_keyboard_grab, "new popup grab");
    state.core.popup_grab = Some(PopupGrabState {
        root,
        grab: grab.clone(),
        has_keyboard_grab,
    });

    // The grab keeps the root focused while it lives.
    update_keyboard_focus(state);
}

/// Drop popup grabs that have ended, and the keyboard grab they took.
pub fn refresh_popup_grab(state: &mut State) {
    let Some(grab) = state.core.popup_grab.as_ref() else {
        return;
    };
    if !grab.grab.has_ended() {
        return;
    }

    let serial = grab.grab.serial();
    let had_keyboard = grab.has_keyboard_grab;
    state.core.popup_grab = None;

    if had_keyboard {
        if let Some(keyboard) = state.core.seat.get_keyboard() {
            if keyboard.has_grab(serial) {
                keyboard.unset_grab(state);
            }
        }
    }

    update_keyboard_focus(state);
}

/// Nudge a popup's positioner so the popup fits on its parent's output.
pub fn unconstrain_popup(state: &State, popup: &PopupKind) {
    let PopupKind::Xdg(_) = popup else {
        // Input-method popups are placed by the IME (see `handlers::ime`).
        return;
    };
    let Ok(root) = find_popup_root_surface(popup) else {
        return;
    };

    // The positioner target is relative to the parent's geometry.
    let target = if let Some((output, layer)) = layer_and_output(state, &root) {
        // Layer-surface menus (e.g. a bar's context menu) may use the whole
        // output; subtract where the layer itself sits.
        let Some(output_geometry) = state.core.space.output_geometry(&output) else {
            return;
        };
        let map = layer_map_for_output(&output);
        let Some(layer_geometry) = map.layer_geometry(&layer) else {
            return;
        };
        let mut target: Rectangle<i32, Logical> = Rectangle::from_size(output_geometry.size);
        target.loc -= layer_geometry.loc;
        target
    } else if let Some(window) = window_for_root(state, &root) {
        // The positioner target is relative to the window's geometry, so the
        // output rectangle has to be translated by the window's on-screen
        // position (a workspace-local rect, not the raw geometry offset inside
        // the surface buffer). Without this, menus on a window near the screen
        // edge are never constrained and can slide off the output.
        let Some(location) = state.core.space.element_location(&window) else {
            return;
        };
        let center = location.to_f64() + window.geometry().size.to_f64() / 2.0;
        let output = state
            .core
            .output_at(center)
            .or_else(|| return state.core.space.outputs().next().cloned());
        let Some(output) = output else {
            return;
        };
        // Menu popups stay out of the area reserved by bars/layer surfaces,
        // like niri's working area.
        let Some(area) = work_area(state, &output) else {
            return;
        };
        Rectangle::new(area.loc - location, area.size)
    } else {
        return;
    };

    let mut target = target;
    target.loc -= get_popup_toplevel_coords(popup);

    if let PopupKind::Xdg(popup) = popup {
        popup.with_pending_state(|pending| {
            pending.geometry = pending.positioner.get_unconstrained_geometry(target);
        });
    }
}
