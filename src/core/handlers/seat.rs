// Ported from niri (GPL-3.0-or-later): src/handlers/mod.rs

//! Seat, clipboard and pointer-constraint handlers.
//!
//! The clipboard and drag-and-drop handling here is complete; the enrichments
//! (wlr-data-control, pointer gestures, tablet) arrive in M1/M2.

use smithay::input::dnd::{DnDGrab, DndGrabHandler, GrabType, Source};
use smithay::input::pointer::{CursorImageStatus, Focus, GrabStartData, PointerGrab};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Serial;
use smithay::wayland::pointer_constraints::PointerConstraintsHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState,
};

use tracing::debug;

use crate::core::input::click_grab::ClickGrab;
use crate::state::State;

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        return &mut self.core.seat_state;
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // Whatever the focused client asks for is drawn by `core::render`.
        self.core.cursor_status = image;
        self.core.queue_redraw_all();
    }

    /// The implicit grab of every button press.
    ///
    /// Our [`ClickGrab`] keeps the focus where the press started (so a click
    /// that leaves the window still reaches it) and is what `move_request` /
    /// `resize_request` are validated against (`docs/plan.md` §9.5).
    fn click_grab(&mut self, start_data: GrabStartData<Self>) -> impl PointerGrab<Self> {
        return ClickGrab::new(start_data);
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused
            .and_then(|surface| return self.core.display_handle.get_client(surface.id()).ok());
        debug!(?client, "keyboard focus changed");
        set_data_device_focus(&self.core.display_handle, seat, client);
    }
}

impl PointerConstraintsHandler for State {}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        return &mut self.core.data_device_state;
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        return &mut self.core.primary_selection_state;
    }
}

impl smithay::wayland::selection::wlr_data_control::DataControlHandler for State {
    fn data_control_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::wlr_data_control::DataControlState {
        return &mut self.core.wlr_data_control_state;
    }
}

impl smithay::wayland::selection::ext_data_control::DataControlHandler for State {
    fn data_control_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::ext_data_control::DataControlState {
        return &mut self.core.ext_data_control_state;
    }
}

impl DndGrabHandler for State {}

impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let Some(pointer) = seat.get_pointer() else {
                    source.cancel();
                    return;
                };
                let Some(start_data) = pointer.grab_start_data() else {
                    source.cancel();
                    return;
                };
                let grab =
                    DnDGrab::new_pointer(&self.core.display_handle, start_data, source, seat);
                pointer.set_grab(self, grab, serial, Focus::Keep);
            }
            // Touch drag-and-drop arrives with the touch input model in M1.
            GrabType::Touch => source.cancel(),
        }
    }
}
