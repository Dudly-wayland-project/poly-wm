// Ported from niri (GPL-3.0-or-later): src/input/click_grab.rs
// (niri's version is Smithay's `ClickGrab` without the grab location updates.)

//! The implicit grab that is active while a pointer button is held.
//!
//! Smithay installs this grab on every button press through
//! [`smithay::input::SeatHandler::click_grab`]. Its jobs:
//!
//! * forward motion/buttons/axis to the surface that had focus when the press
//!   happened, so a click keeps its target even if the pointer leaves it, and
//! * remember the press (`serial`, button, location) so that
//!   `xdg_toplevel.move`/`resize` requests can be validated against it.
//!
//! Unlike Smithay's version it does *not* update the focus location on motion:
//! for a partially off-screen window, following the surface would turn a click
//! into a short drag and could trigger unintended drag-and-drop.

use smithay::input::SeatHandler;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::utils::{Logical, Point};

/// The focus of the grab: where the press started.
pub struct ClickGrab<D: SeatHandler> {
    start_data: GrabStartData<D>,
    focus: Option<(D::PointerFocus, Point<f64, Logical>)>,
}

impl<D: SeatHandler> ClickGrab<D> {
    /// Start the grab from a button press.
    pub fn new(start_data: GrabStartData<D>) -> Self {
        return Self {
            focus: start_data.focus.clone(),
            start_data,
        };
    }
}

impl<D: SeatHandler + 'static> PointerGrab<D> for ClickGrab<D> {
    fn motion(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        _focus: Option<(<D as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // Keep the original focus, do not follow the pointer.
        handle.motion(data, self.focus.clone(), event);
    }

    fn relative_motion(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        focus: Option<(<D as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            // No more buttons are pressed, release the implicit grab.
            handle.unset_grab(self, data, event.serial, event.time, false);
        }
    }

    fn axis(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>, details: AxisFrame) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<D> {
        return &self.start_data;
    }

    fn unset(&mut self, _data: &mut D) {}
}
