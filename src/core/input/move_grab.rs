// The grab state machine follows niri (GPL-3.0-or-later): src/input/move_grab.rs
// niri keeps the tiled/floating/cross-output strategies in its layout engine;
// Poly implements the tiled reorder, floating and cross-output movement
// strategies here.

//! Interactive window movement (`Mod+MouseLeft`, or a client's CSD drag).
//!
//! The grab collects pointer motion, and applies it once per frame: the window
//! follows the pointer only after the configured threshold (`window_drag.
//! threshold`) is crossed, and only ever by the offset from where the drag
//! started — no accumulation drift.

use poly_config::TiledDrag;
use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, GestureHoldBeginEvent, GestureHoldEndEvent,
    GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
    GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab,
    PointerInnerHandle, RelativeMotionEvent,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use tracing::trace;

use crate::core::layout::{LayoutManager, TiledSlot};
use crate::state::State;

/// What kind of window the drag started on.
enum MoveMode {
    /// A window that was already floating.
    Floating {
        /// The output it belonged to before the drag.
        output: Option<smithay::output::Output>,
    },
    /// A tiled window that will be reordered inside the layout
    /// (`window_drag.tiled = "reorder"`).
    Reorder {
        /// Where it must go back on Escape.
        slot: TiledSlot,
        /// Whether the threshold has been crossed and the window follows the
        /// pointer.
        active: bool,
    },
    /// A tiled window that will be pulled out once the threshold is crossed.
    Tiled {
        /// Where it must go back on Escape.
        slot: TiledSlot,
        /// Whether it has actually left the layout.
        floated: bool,
    },
    /// `window_drag.tiled = "ignore"`: the layout owns the geometry.
    Ignore,
}

/// Which axis a Shift lock keeps.
#[derive(Debug, Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

/// A window being dragged around.
pub struct MoveGrab {
    start_data: GrabStartData<State>,
    window: Window,
    /// The window size during the drag.
    size: Size<i32, Logical>,
    /// Where the window was when the grab started.
    original_location: Point<i32, Logical>,
    /// Pointer motion since then.
    delta: Point<f64, Logical>,
    /// The latest pointer position in compositor space.
    pointer: Point<f64, Logical>,
    /// Whether the movement threshold has been crossed.
    started: bool,
    /// What the drag does to the window.
    mode: MoveMode,
    /// The axis chosen by the axis-lock modifier, while it is held.
    axis: Option<Axis>,
}

impl MoveGrab {
    /// Set up a grab for `window`; `None` when it is not on screen or is
    /// fullscreen.
    pub fn new(
        state: &mut State,
        window: &Window,
        start_data: GrabStartData<State>,
    ) -> Option<Self> {
        let (rect, tiled, output) = {
            let managed = state.core.layout.by_window(window)?;
            if managed.fullscreen.is_some() {
                return None;
            }
            (
                managed.rect_at(std::time::Instant::now()),
                managed.tiled,
                managed.output.clone(),
            )
        };
        let mode = if tiled {
            match state.core.config.window_drag.tiled {
                TiledDrag::Reorder => MoveMode::Reorder {
                    slot: LayoutManager::tiled_slot(state, window)?,
                    active: false,
                },
                TiledDrag::Float => MoveMode::Tiled {
                    slot: LayoutManager::tiled_slot(state, window)?,
                    floated: false,
                },
                TiledDrag::Ignore => MoveMode::Ignore,
            }
        } else {
            MoveMode::Floating { output }
        };
        let pointer = start_data.location;
        return Some(Self {
            start_data,
            window: window.clone(),
            size: rect.size,
            original_location: rect.loc,
            delta: Point::from((0.0, 0.0)),
            pointer,
            started: false,
            mode,
            axis: None,
        });
    }

    /// Apply the current offset to the window.
    fn update(&mut self, state: &mut State) {
        if !self.started {
            let threshold = state.core.config.window_drag.threshold;
            if self.delta.x.abs() < threshold && self.delta.y.abs() < threshold {
                return;
            }
            self.started = true;
            match &mut self.mode {
                MoveMode::Tiled { slot, floated } => {
                    // The window leaves `ctx.windows` and the layout reflows once;
                    // its own rectangle does not jump.
                    LayoutManager::float_window(state, &self.window, slot.rect);
                    LayoutManager::relayout(state);
                    *floated = true;
                }
                MoveMode::Reorder { active, .. } => {
                    // The window stays in the layout; only its rendered
                    // rectangle detaches and follows the pointer.
                    *active = LayoutManager::begin_tiled_reorder(state, &self.window);
                }
                MoveMode::Floating { .. } | MoveMode::Ignore => {}
            }
        }
        if matches!(self.mode, MoveMode::Ignore) {
            return;
        }
        if let MoveMode::Reorder { active: false, .. } = self.mode {
            return;
        }

        // Axis lock applies while its modifier is held; the dominant direction
        // at the moment the threshold is crossed picks the axis.
        let axis_held = axis_lock_held(state);
        if axis_held && self.axis.is_none() {
            self.axis = Some(if self.delta.x.abs() >= self.delta.y.abs() {
                Axis::Horizontal
            } else {
                Axis::Vertical
            });
        } else if !axis_held {
            self.axis = None;
        }
        let (dx, dy) = match self.axis {
            Some(Axis::Horizontal) => (self.delta.x, 0.0),
            Some(Axis::Vertical) => (0.0, self.delta.y),
            None => (self.delta.x, self.delta.y),
        };

        if let MoveMode::Reorder { active: true, .. } = self.mode {
            let location =
                self.original_location + Point::from((dx.round() as i32, dy.round() as i32));
            let rect = Rectangle::new(location, self.size);
            LayoutManager::update_tiled_reorder(state, &self.window, self.pointer, rect);
            return;
        }

        let requested =
            self.original_location + Point::from((dx.round() as i32, dy.round() as i32));
        let location = clamp_to_visible(state, requested, self.size);

        // Follow the pointer onto another output as soon as it crosses the
        // boundary; the final position is already resolved on release.
        if let Some((output, _)) = LayoutManager::work_area_at(state, self.pointer) {
            LayoutManager::assign_output(state, &self.window, output);
        }

        trace!("moving window to {location:?}");
        let rect = Rectangle::new(location, self.size);
        LayoutManager::move_floating(state, &self.window, rect);
        state.core.queue_redraw_all();
    }

    /// Put the window back where the drag started (Escape).
    fn restore(&mut self, state: &mut State) {
        match &self.mode {
            MoveMode::Floating { output } => {
                LayoutManager::move_floating(
                    state,
                    &self.window,
                    Rectangle::new(self.original_location, self.size),
                );
                if let Some(output) = output {
                    LayoutManager::assign_output(state, &self.window, output.clone());
                }
            }
            MoveMode::Reorder { slot, .. } | MoveMode::Tiled { slot, .. } => {
                LayoutManager::cancel_tiled_reorder(state, &self.window, slot);
            }
            MoveMode::Ignore => {}
        }
        state.core.queue_redraw_all();
    }
}

impl PointerGrab<State> for MoveGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, self.start_data.focus.clone(), event);
        self.pointer = event.location;
        self.delta = event.location - self.start_data.location;
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            // The drag is done: keep the window where it is.
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
        self.update(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<State> {
        return &self.start_data;
    }

    fn unset(&mut self, data: &mut State) {
        // Escape sets the flag before unsetting the grab: put the window back.
        if std::mem::take(&mut data.core.grab_cancelled) {
            self.restore(data);
        } else if let MoveMode::Reorder { active: true, .. } = self.mode {
            LayoutManager::finish_tiled_reorder(data, &self.window);
        }
        data.core.cursor_status = CursorImageStatus::default_named();
        data.core.queue_redraw_all();
    }
}

/// Keep at least `clamp(size/4, 10, 75)` pixels of the window inside the
/// bounding box of the output work areas, while still
/// allowing the rest to hang off the screen.
fn clamp_to_visible(
    state: &State,
    location: Point<i32, Logical>,
    size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let Some(bounding) = state
        .core
        .space
        .outputs()
        .filter_map(|output| return LayoutManager::work_area(state, output))
        .reduce(|acc, area| return acc.merge(area))
    else {
        return location;
    };

    let visible_w = (size.w / 4).clamp(10, 75).min(size.w.max(1));
    let visible_h = (size.h / 4).clamp(10, 75).min(size.h.max(1));
    let min_x = bounding.loc.x - (size.w - visible_w);
    let max_x = bounding.loc.x + bounding.size.w - visible_w;
    let min_y = bounding.loc.y - (size.h - visible_h);
    let max_y = bounding.loc.y + bounding.size.h - visible_h;
    return Point::from((
        location.x.clamp(min_x, max_x),
        location.y.clamp(min_y, max_y),
    ));
}

/// Whether the configured axis-lock modifier is held.
fn axis_lock_held(state: &State) -> bool {
    let Some(lock) = state.core.config.window_drag.axis_lock else {
        return false;
    };
    let Some(keyboard) = state.core.seat.get_keyboard() else {
        return false;
    };
    let modifiers = keyboard.modifier_state();
    return match lock {
        poly_config::ModKey::Ctrl => modifiers.ctrl,
        poly_config::ModKey::Shift => modifiers.shift,
        poly_config::ModKey::Alt => modifiers.alt,
        poly_config::ModKey::Super => modifiers.logo,
        poly_config::ModKey::IsoLevel3Shift | poly_config::ModKey::IsoLevel5Shift => false,
    };
}

/// The rectangle of a window on screen, for the resize edge pick.
pub fn window_rect(state: &State, window: &Window) -> Option<Rectangle<i32, Logical>> {
    if let Some(rect) = LayoutManager::window_rect(state, window) {
        return Some(rect);
    }
    let location = state.core.space.element_location(window)?;
    let size = window.geometry().size;
    return Some(Rectangle::new(location, size));
}
