// The resize state machine follows niri (GPL-3.0-or-later): src/input/resize_grab.rs
// niri keeps the rect math in its layout engine (`InteractiveResize`); Poly
// keeps the rectangle in this grab but stores the resize state in the layout.

//! Interactive window resizing (`Mod+MouseRight`, or a client's CSD resize).
//!
//! The grab remembers the rectangle the window had when the drag started and
//! applies `original + delta` every frame: the pointer never chases a moving
//! edge, so a resize cannot drift. The layout throttles the configures it sends
//! (one per client commit) and draws the size the client acknowledged; see
//! `LayoutManager::begin_resize`.

use bitflags::bitflags;
use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, GestureHoldBeginEvent,
    GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
    GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData,
    MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus as _;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use tracing::trace;

use poly_config::ResizePick;

use crate::core::layout::{LayoutManager, ResizeAnchor};
use crate::state::State;

/// The smallest window a resize grab produces, unless the client asks for
/// more. The layout owns tiled sizes, so only floating windows get here.
const MIN_SIZE: i32 = 1;

bitflags! {
    /// Which window edges a resize grab moves.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ResizeEdges: u8 {
        /// The top edge.
        const TOP = 1;
        /// The bottom edge.
        const BOTTOM = 1 << 1;
        /// The left edge.
        const LEFT = 1 << 2;
        /// The right edge.
        const RIGHT = 1 << 3;
    }
}

impl ResizeEdges {
    /// Map the protocol's resize edge to ours.
    pub fn from_xdg(edge: ResizeEdge) -> Self {
        return match edge {
            ResizeEdge::Top => Self::TOP,
            ResizeEdge::Bottom => Self::BOTTOM,
            ResizeEdge::Left => Self::LEFT,
            ResizeEdge::Right => Self::RIGHT,
            ResizeEdge::TopLeft => Self::TOP | Self::LEFT,
            ResizeEdge::TopRight => Self::TOP | Self::RIGHT,
            ResizeEdge::BottomLeft => Self::BOTTOM | Self::LEFT,
            ResizeEdge::BottomRight => Self::BOTTOM | Self::RIGHT,
            _ => Self::empty(),
        };
    }

    /// The cursor that goes with these edges.
    pub fn cursor_icon(self) -> CursorIcon {
        return match (
            self.contains(Self::LEFT) || self.contains(Self::RIGHT),
            self.contains(Self::TOP),
            self.contains(Self::BOTTOM),
        ) {
            (true, true, _) => CursorIcon::NwseResize,
            (true, false, true) => CursorIcon::NeswResize,
            (true, false, false) => CursorIcon::EwResize,
            (false, true, _) => CursorIcon::NsResize,
            (false, false, true) => CursorIcon::NsResize,
            (false, false, false) => CursorIcon::Default,
        };
    }
}

/// Pick the edges to resize from the pointer position inside the window
/// (`window_drag.resize_pick`): the outer thirds, or a pixel band from the
/// window border.
pub fn pick_edges(
    rect: Rectangle<i32, Logical>,
    location: Point<f64, Logical>,
    pick: ResizePick,
) -> ResizeEdges {
    let band = match pick {
        ResizePick::Thirds => {
            Point::<i32, Logical>::from(((rect.size.w / 3).max(1), (rect.size.h / 3).max(1)))
        }
        ResizePick::Pixels(pixels) => {
            let pixels = pixels.round().max(1.0) as i32;
            Point::<i32, Logical>::from((pixels.min(rect.size.w), pixels.min(rect.size.h)))
        }
    };

    let x = (location.x - f64::from(rect.loc.x)).round() as i32;
    let y = (location.y - f64::from(rect.loc.y)).round() as i32;

    let mut edges = ResizeEdges::empty();
    if x < band.x {
        edges |= ResizeEdges::LEFT;
    } else if x > rect.size.w - band.x {
        edges |= ResizeEdges::RIGHT;
    }
    if y < band.y {
        edges |= ResizeEdges::TOP;
    } else if y > rect.size.h - band.y {
        edges |= ResizeEdges::BOTTOM;
    }
    return edges;
}

/// A window being resized.
pub struct ResizeGrab {
    start_data: GrabStartData<State>,
    window: Window,
    edges: ResizeEdges,
    /// The window rectangle when the grab started.
    original_rect: Rectangle<i32, Logical>,
    /// Pointer motion since then.
    delta: Point<f64, Logical>,
    /// Whether the movement threshold has been crossed.
    started: bool,
}

impl ResizeGrab {
    /// Set up a grab for `window`; `None` when it is not on screen.
    pub fn new(
        state: &mut State,
        window: &Window,
        edges: ResizeEdges,
        start_data: GrabStartData<State>,
    ) -> Option<Self> {
        let (original_rect, tiled, fullscreen) = {
            let managed = state.core.layout.by_window(window)?;
            (
                managed.rect_at(std::time::Instant::now()),
                managed.tiled,
                managed.fullscreen.is_some(),
            )
        };
        if fullscreen {
            return None;
        }
        // v1 resizes floating windows only: tiled geometry belongs to the Lua
        // layout.
        if tiled {
            return None;
        }
        return Some(Self {
            start_data,
            window: window.clone(),
            edges,
            original_rect,
            delta: Point::from((0.0, 0.0)),
            started: false,
        });
    }

    /// The rectangle the window should have with the current offset.
    fn new_rect(&self, state: &State) -> Rectangle<i32, Logical> {
        let mut rect = self.original_rect;
        let dx = self.delta.x.round() as i32;
        let dy = self.delta.y.round() as i32;

        if self.edges.contains(ResizeEdges::LEFT) {
            rect.loc.x += dx;
            rect.size.w -= dx;
        } else if self.edges.contains(ResizeEdges::RIGHT) {
            rect.size.w += dx;
        }
        if self.edges.contains(ResizeEdges::TOP) {
            rect.loc.y += dy;
            rect.size.h -= dy;
        } else if self.edges.contains(ResizeEdges::BOTTOM) {
            rect.size.h += dy;
        }

        // Respect the client's size hints, with our own floor as a fallback
        // (`window_drag` always needs something to drag).
        let hints = size_hints(&self.window);
        let min_w = hints.min_w.unwrap_or(MIN_SIZE).max(MIN_SIZE);
        let min_h = hints.min_h.unwrap_or(MIN_SIZE).max(MIN_SIZE);

        if rect.size.w < min_w {
            if self.edges.contains(ResizeEdges::LEFT) {
                rect.loc.x = self.original_rect.loc.x + self.original_rect.size.w - min_w;
            }
            rect.size.w = min_w;
        }
        if rect.size.h < min_h {
            if self.edges.contains(ResizeEdges::TOP) {
                rect.loc.y = self.original_rect.loc.y + self.original_rect.size.h - min_h;
            }
            rect.size.h = min_h;
        }
        if let Some(max_w) = hints.max_w {
            rect.size.w = rect.size.w.min(max_w);
        }
        if let Some(max_h) = hints.max_h {
            rect.size.h = rect.size.h.min(max_h);
        }

        let _ = state;
        return rect;
    }

    /// Apply the current offset: move the window, ask the client to resize.
    fn update(&mut self, state: &mut State) {
        if !self.started {
            let threshold = state.core.config.window_drag.threshold;
            if self.delta.x.abs() < threshold && self.delta.y.abs() < threshold {
                return;
            }
            self.started = true;
            // The layout anchors the rectangle on the edges the pointer does
            // not move: dragging the left edge keeps the right one fixed.
            let anchor = ResizeAnchor {
                fixed_right: self.edges.contains(ResizeEdges::LEFT),
                fixed_bottom: self.edges.contains(ResizeEdges::TOP),
            };
            if !LayoutManager::begin_resize(state, &self.window, self.original_rect, anchor) {
                return;
            }
        }

        let rect = self.new_rect(state);
        if rect == self.original_rect {
            return;
        }

        trace!("resize moves window to {:?}", rect.loc);
        LayoutManager::resize_floating(state, &self.window, rect);

        // The window's committed geometry may still be the old one, but the
        // client is on its way; repaint so the pending size shows up as soon as
        // it commits (`on_window_commit` queues a redraw too).
        state.core.queue_redraw_all();
    }

    /// Put the window back to its original rectangle (Escape).
    fn restore(&mut self, state: &mut State) {
        LayoutManager::cancel_resize(state, &self.window, self.original_rect);
    }
}

impl PointerGrab<State> for ResizeGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, self.start_data.focus.clone(), event);
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
        if std::mem::take(&mut data.core.grab_cancelled) {
            self.restore(data);
        } else if self.started {
            LayoutManager::end_resize(data, &self.window);
        }
        data.core.cursor_status = CursorImageStatus::default_named();
        data.core.queue_redraw_all();
    }
}

/// The size hints the client set through `xdg_toplevel`; `0` on an axis means
/// "no constraint" and becomes `None`.
#[derive(Debug, Clone, Copy, Default)]
struct SizeHints {
    min_w: Option<i32>,
    min_h: Option<i32>,
    max_w: Option<i32>,
    max_h: Option<i32>,
}

/// Read the client's minimum and maximum window size.
fn size_hints(window: &Window) -> SizeHints {
    let Some(surface) = window.wl_surface() else {
        return SizeHints::default();
    };
    return with_states(&surface, |states| {
        let mut guard = states.cached_state.get::<SurfaceCachedState>();
        let hint = guard.current();
        return SizeHints {
            min_w: (hint.min_size.w > 0).then_some(hint.min_size.w),
            min_h: (hint.min_size.h > 0).then_some(hint.min_size.h),
            max_w: (hint.max_size.w > 0).then_some(hint.max_size.w),
            max_h: (hint.max_size.h > 0).then_some(hint.max_size.h),
        };
    });
}
