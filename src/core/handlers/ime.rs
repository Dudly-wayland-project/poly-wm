// Ported from niri (GPL-3.0-or-later): src/handlers/mod.rs (InputMethodHandler)
//
//! Text input and input methods.
//!
//! Smithay does the heavy lifting: keyboard focus changes carry text-input
//! enter/leave, and the input method's keyboard grab is routed through the
//! seat automatically. Poly only has to place the candidate popup under the
//! text cursor and keep it tracked so the focused window renders it.

use smithay::desktop::{PopupKind, PopupManager};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::input_method::{InputMethodHandler, PopupSurface};
use tracing::debug;

use crate::core::window::{root_surface, window_for_root};
use crate::state::State;

impl InputMethodHandler for State {
    fn new_popup(&mut self, surface: PopupSurface) {
        // The candidate window goes under the caret, like every other IME.
        position_ime_popup(&surface);
        if let Err(err) = self
            .core
            .popups
            .track_popup(PopupKind::InputMethod(surface))
        {
            debug!("error tracking an IME popup: {err:?}");
            return;
        }
        self.core.queue_redraw_all();
    }

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::InputMethod(surface));
        }
        self.core.queue_redraw_all();
    }

    fn popup_repositioned(&mut self, surface: PopupSurface) {
        // The client moved its text cursor: place the popup again.
        position_ime_popup(&surface);
        self.core.queue_redraw_all();
    }

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        // The IME popup is positioned relative to the focused window's
        // geometry (this also feeds `PopupKind::geometry` during rendering).
        let root = root_surface(parent);
        return window_for_root(self, &root)
            .map(|window| return window.geometry())
            .unwrap_or_default();
    }
}

/// Put the candidate window just below the text cursor rectangle.
///
/// The popup's real size is only known once it has a buffer, so this is a
/// best-effort placement: below the caret, never at negative coordinates.
fn position_ime_popup(surface: &PopupSurface) {
    let cursor = surface.text_input_rectangle();

    let mut location = cursor.loc + Point::from((0, cursor.size.h));
    location.x = location.x.max(0);
    location.y = location.y.max(0);
    surface.set_location(location);
}
