// Ported from niri (GPL-3.0-or-later): src/handlers/xdg_shell.rs

//! xdg-shell: window and popup creation, plus the initial configure handshake.
//!
//! Mapping hands the window to the layout engine, which decides its rectangle
//! before it enters the space. This module also validates client-side
//! move/resize drags against the pointer grab that started them.

use smithay::desktop::{PopupKind, Window};
use smithay::input::pointer::{CursorImageStatus, Focus, GrabStartData};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge;
use smithay::reexports::wayland_server::Resource as _;
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Serial;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use tracing::warn;

use crate::core::input::click_grab::ClickGrab;
use crate::core::input::move_grab::{MoveGrab, window_rect};
use crate::core::input::resize_grab::{ResizeEdges, ResizeGrab, pick_edges};
use crate::core::layout::{FullscreenMode, LayoutManager};
use crate::core::window::{UnmappedWindow, window_for_root};
use crate::state::State;

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        return &mut self.core.xdg_shell_state;
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        let window = Window::new_wayland_window(surface);
        let existing = self
            .core
            .unmapped_windows
            .insert(wl_surface, UnmappedWindow::new(window));
        if existing.is_some() {
            warn!("a toplevel surface was created twice");
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        crate::core::popups::unconstrain_popup(self, &PopupKind::Xdg(surface.clone()));
        let kind = PopupKind::Xdg(surface);
        if let Err(err) = self.core.popups.track_popup(kind) {
            warn!("error tracking popup: {err:?}");
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Called both when a client closes a window and when it disconnects.
        let root = surface.wl_surface().clone();
        crate::core::window::on_window_destroyed(self, &root);
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        crate::core::window::update_foreign_toplevel(self, surface.wl_surface());
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        crate::core::window::update_foreign_toplevel(self, surface.wl_surface());
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|pending| {
            pending.geometry = positioner.get_geometry();
            pending.positioner = positioner;
        });
        crate::core::popups::unconstrain_popup(self, &PopupKind::Xdg(surface.clone()));
        surface.send_repositioned(token);
    }

    /// A client-side decoration drag (`xdg_toplevel.move`).
    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        let wl_surface = surface.wl_surface().clone();
        let Some(start_data) = interactive_start_data(self, &wl_surface, serial) else {
            return;
        };
        let Some(window) = window_for_root(self, &wl_surface) else {
            return;
        };
        let Some(grab) = MoveGrab::new(self, &window, start_data) else {
            return;
        };
        // The drag owns the pointer until the button is released and the client
        // must not see the motion.
        let Some(pointer) = self.core.seat.get_pointer() else {
            return;
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
        self.core.queue_redraw_all();
    }

    /// A client-side decoration drag on a window edge (`xdg_toplevel.resize`).
    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        serial: Serial,
        edges: ResizeEdge,
    ) {
        let wl_surface = surface.wl_surface().clone();
        let Some(start_data) = interactive_start_data(self, &wl_surface, serial) else {
            return;
        };
        let Some(window) = window_for_root(self, &wl_surface) else {
            return;
        };

        let mut edges = ResizeEdges::from_xdg(edges);
        if edges.is_empty() {
            // `none`, or an edge newer than this compositor knows: fall back to
            // the edges under the pointer, like the `resize-window` binding.
            let Some(rect) = window_rect(self, &window) else {
                return;
            };
            let pick = self.core.config.window_drag.resize_pick;
            edges = pick_edges(rect, start_data.location, pick);
        }
        if edges.is_empty() {
            return;
        }

        let Some(grab) = ResizeGrab::new(self, &window, edges, start_data) else {
            return;
        };
        let Some(pointer) = self.core.seat.get_pointer() else {
            return;
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
        self.core.cursor_status = CursorImageStatus::Named(edges.cursor_icon());
        self.core.queue_redraw_all();
    }

    /// A client asked for an input grab on a popup (menus, comboboxes).
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        crate::core::popups::start_popup_grab(self, surface, &seat, serial);
    }

    // Maximize and minimize are not implemented: acknowledge them with a
    // configure that does not set the state, so clients are not left waiting.
    // Tiling and fullscreen are owned by the layout engine.
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        surface.send_configure();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let root = surface.wl_surface().clone();
        LayoutManager::set_fullscreen_for_root(self, &root, Some(FullscreenMode::Fullscreen));
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let root = surface.wl_surface().clone();
        LayoutManager::set_fullscreen_for_root(self, &root, None);
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        surface.send_configure();
    }
}

/// The grab data a `move_request`/`resize_request` is validated against.
///
/// A request is honored only when
///
/// * it names the serial of the pointer's current grab,
/// * that grab is our implicit click grab — a drag-and-drop grab also holds a
///   press, and GTK asks to move the window when a drag starts, so anything
///   else must be refused, and
/// * the surface belongs to the client that had the pointer.
fn interactive_start_data(
    state: &State,
    wl_surface: &WlSurface,
    serial: Serial,
) -> Option<GrabStartData<State>> {
    let pointer = state.core.seat.get_pointer()?;
    return pointer
        .with_grab(|grab_serial, grab| {
            if grab_serial != serial {
                return None;
            }
            if !grab.as_any().is::<ClickGrab<State>>() {
                return None;
            }
            let start_data = grab.start_data();
            let (focus, _) = start_data.focus.as_ref()?;
            if !focus.id().same_client_as(&wl_surface.id()) {
                return None;
            }
            return Some(start_data.clone());
        })
        .flatten();
}
