// Ported from niri (GPL-3.0-or-later): src/niri.rs (`RedrawState`)

//! Per-output redraw scheduling.
//!
//! The compositor does not render continuously: each output is in one of a few
//! explicit states, and frames are only produced when something queued a redraw
//! or when we are waiting for the VBlank of a frame we already submitted.

use std::time::{Duration, Instant};

use smithay::output::Output;
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::wayland_server::backend::GlobalId;
use tracing::{debug, trace};

use crate::backend::RenderResult;
use crate::core::Core;
use crate::core::LockState;
use crate::state::State;

/// Frame interval used when an output does not report a refresh rate.
pub const FALLBACK_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
/// The slowest frame interval we ever schedule (30 Hz).
pub const MAX_FRAME_INTERVAL: Duration = Duration::from_micros(33_334);
/// The fastest frame interval we ever schedule (240 Hz).
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_micros(4_167);

/// Upper bound between animation watchdog ticks.
///
/// Animations are advanced once per presented frame by the backends, so this
/// timer only exists for the case where nothing drives frames at all (headless,
/// session paused, a backend that waits for input). It deliberately stays far
/// slower than a frame: a watchdog on the frame grid would queue redraws on its
/// own wall-clock phase and race the real frame loop.
pub const ANIMATION_WATCHDOG: Duration = Duration::from_millis(100);

/// How long one frame of `output` lasts.
///
/// The mode reports millhertz; a mode without a refresh rate (or an absurd one)
/// falls back to 60 Hz. The result is clamped, so a bogus mode can neither spin
/// the animation timer nor stall it at one frame per second.
pub fn frame_interval(output: &Output) -> Duration {
    let Some(mode) = output.current_mode() else {
        return FALLBACK_FRAME_INTERVAL;
    };
    if mode.refresh <= 0 {
        return FALLBACK_FRAME_INTERVAL;
    }
    let interval = Duration::from_secs_f64(1_000.0 / mode.refresh as f64);
    return interval.clamp(MIN_FRAME_INTERVAL, MAX_FRAME_INTERVAL);
}

/// The frame interval animations should be advanced at.
///
/// The animation clock is shared by every output, so it has to run at the
/// fastest refresh rate among them: a 144 Hz output next to a 60 Hz one must
/// not be held back to 60 Hz.
pub fn animation_interval(core: &Core) -> Duration {
    return core
        .space
        .outputs()
        .map(frame_interval)
        .min()
        .unwrap_or(FALLBACK_FRAME_INTERVAL);
}

/// Where an output is in its redraw cycle.
#[derive(Debug, Default)]
pub enum RedrawState {
    /// Nothing is queued.
    #[default]
    Idle,
    /// A redraw is queued.
    Queued,
    /// A frame was submitted to KMS; we are waiting for its VBlank.
    WaitingForVBlank {
        /// Whether a new redraw was queued while waiting.
        redraw_needed: bool,
    },
    /// Nothing was submitted to KMS; a timer fires at the estimated VBlank.
    WaitingForEstimatedVBlank(RegistrationToken),
    /// A redraw was queued on top of the estimated-VBlank wait.
    WaitingForEstimatedVBlankAndQueued(RegistrationToken),
}

impl RedrawState {
    /// Queue a redraw, keeping any wait we are already in.
    pub fn queue_redraw(self) -> Self {
        return match self {
            RedrawState::Idle => RedrawState::Queued,
            RedrawState::WaitingForEstimatedVBlank(token) => {
                RedrawState::WaitingForEstimatedVBlankAndQueued(token)
            }
            // A redraw is already queued.
            value @ (RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)) => {
                value
            }
            // We are waiting for the VBlank, request a redraw afterwards.
            RedrawState::WaitingForVBlank { .. } => RedrawState::WaitingForVBlank {
                redraw_needed: true,
            },
        };
    }

    /// Whether a redraw is waiting to be rendered.
    pub fn is_queued(&self) -> bool {
        return matches!(
            self,
            RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
        );
    }
}

/// Predicts when the frame being rendered will be on screen.
///
/// Ported from niri (`src/frame_clock.rs`). Animations are sampled at
/// [`FrameClock::next_presentation_time`] instead of `Instant::now()` so that
/// consecutive frames are exactly one refresh interval apart: the event loop's
/// dispatch latency no longer shows up as uneven animation steps.
#[derive(Debug, Default)]
pub struct FrameClock {
    /// The last VBlank the output reported (or the last submitted frame when
    /// the backend has no presentation timestamp).
    last_presentation: Option<Instant>,
    /// The output's refresh interval, once it has a mode.
    refresh_interval: Option<Duration>,
}

impl FrameClock {
    /// Set the refresh interval; a mode change invalidates the VBlank grid.
    pub fn set_refresh_interval(&mut self, interval: Option<Duration>) {
        if self.refresh_interval == interval {
            return;
        }
        self.refresh_interval = interval;
        self.last_presentation = None;
    }

    /// Record a presented frame.
    pub fn presented(&mut self, time: Instant) {
        self.last_presentation = Some(time);
    }

    /// The next VBlank on the output's grid at or after `now`.
    ///
    /// Without a refresh interval or a presentation yet there is no grid to
    /// snap to; the frame time is simply `now`.
    pub fn next_presentation_time(&self, now: Instant) -> Instant {
        let Some(interval) = self
            .refresh_interval
            .filter(|value| return !value.is_zero())
        else {
            return now;
        };
        let Some(last) = self.last_presentation else {
            return now;
        };
        // An early VBlank (or a clock that stepped back) must not produce a
        // time before the last presentation. niri advances `now` by one
        // interval, which still yields the next grid point.
        let mut now = now;
        if now <= last {
            now += interval;
            if now < last {
                now = last + interval;
            }
        }
        let steps = now.saturating_duration_since(last).as_nanos() / interval.as_nanos() + 1;
        // A long stall may skip several frames; clamping keeps the Duration
        // multiplication below in range (u32 frames is over four years at 60 Hz).
        let steps = u32::try_from(steps).unwrap_or(u32::MAX);
        return last + interval * steps;
    }
}

/// Compositor state attached to one output.
#[derive(Debug, Default)]
pub struct OutputState {
    /// The `wl_output` global, so that we can disable it when the output goes away.
    pub global: Option<GlobalId>,
    /// Redraw scheduling state.
    pub redraw_state: RedrawState,
    /// When the frame currently being rendered will be presented.
    pub frame_clock: FrameClock,
    /// Incremented for every frame whose callbacks were sent; used to throttle
    /// frame callbacks so a surface gets at most one per presented frame.
    pub frame_callback_sequence: u64,
    /// Solid colour drawn behind the windows of this output
    /// (`outputs.<name>.background`); `None` keeps the backend's default.
    pub background: Option<poly_config::Color>,
}

impl State {
    /// Stat the shader files and expire Toasts (the 500 ms housekeeping timer).
    ///
    /// Recompiling needs the GL context, so this only sets the dirty flag and
    /// queues a redraw; `Shaders::prepare` picks it up on the next frame.
    pub fn poll_shader_files(&mut self) {
        if self.core.shaders.poll_files() {
            tracing::debug!("a shader file changed; recompiling on the next frame");
            self.core.queue_redraw_all();
        }
        if self.core.toasts.remove_expired(std::time::Instant::now()) {
            self.core.queue_redraw_all();
        }
    }

    /// Render every output that has a redraw queued.
    pub fn redraw_queued_outputs(&mut self) {
        for output in self.core.space.outputs().cloned().collect::<Vec<_>>() {
            let queued = self
                .core
                .output_state
                .get(&output)
                .is_some_and(|state| return state.redraw_state.is_queued());
            if queued {
                // Trace level: a redraw can be queued for every input event.
                trace!("redrawing output {}", output.name());
                let result = self.backend.render(&mut self.core, &output);
                trace!("render of {} returned {result:?}", output.name());
                self.on_output_rendered(&output, result);
            }
        }
    }

    /// Bring client state up to date, render what is due and flush clients.
    ///
    /// This is the per-iteration callback of the event loop, mirroring niri's
    /// `refresh_and_flush_clients`. It lives on [`State`] (not on [`Core`])
    /// because a pending session lock is confirmed only after the locked frame
    /// has actually been rendered.
    pub fn refresh_and_flush_clients(&mut self) {
        self.core.space.refresh();
        crate::core::layout::LayoutManager::tick(self);
        self.core.popups.cleanup();

        self.redraw_queued_outputs();

        // A shader reading `u_time` (or the cursor) depends on wall-clock time,
        // so a frame is due again as soon as the last one finished.
        if self.core.continuous_redraw {
            self.core.queue_redraw_all();
        }

        if let Err(err) = self.core.display_handle.flush_clients() {
            debug!("error flushing wayland clients: {err}");
        }
    }

    /// Confirm a pending session lock once every output showed a locked frame.
    ///
    /// The protocol requires the `locked` event to be sent only after a new
    /// locked frame has been presented on all outputs; sending it earlier can
    /// leave session content on screen while the client already thinks the
    /// screen is locked (niri does the same bookkeeping).
    fn on_output_rendered(&mut self, output: &Output, result: RenderResult) {
        if !matches!(self.core.lock_state, LockState::WaitingForSurfaces(_)) {
            return;
        }
        if result != RenderResult::Submitted {
            return;
        }

        let Some(lock_surface) = self.core.lock_surface_for_output(output) else {
            // No lock surface yet: the compositor's solid background is the
            // "locked" frame, so this output still counts.
            self.mark_locked_frame_rendered(output);
            return;
        };
        let lock_surface = lock_surface.wl_surface().clone();
        trace!(
            "locked frame on {}: lock surface has buffer={}",
            output.name(),
            super::handlers::session_lock::lock_surface_has_buffer(&lock_surface)
        );
        self.mark_locked_frame_rendered(output);
    }

    /// Record that `output` presented a locked frame and confirm once all did.
    fn mark_locked_frame_rendered(&mut self, output: &Output) {
        self.core.lock_rendered_outputs.insert(output.clone());
        let all_rendered = self
            .core
            .space
            .outputs()
            .all(|output| return self.core.lock_rendered_outputs.contains(output));
        if all_rendered {
            self.confirm_lock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{Mode, PhysicalProperties};

    fn output_with(refresh: i32) -> Output {
        let output = Output::new(
            "test".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "Poly".into(),
                model: "Test".into(),
                serial_number: "Unknown".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (1280, 800).into(),
                refresh,
            }),
            None,
            None,
            None,
        );
        return output;
    }

    /// Assert that `actual` is within one microsecond of `expected_ms`.
    fn assert_ms(actual: Duration, expected_ms: f64) {
        let delta = actual.as_secs_f64() * 1000.0 - expected_ms;
        assert!(
            delta.abs() < 0.001,
            "{actual:?} is not {expected_ms} ms ({delta:+} ms off)"
        );
    }

    #[test]
    fn frame_interval_follows_the_mode_refresh() {
        // 60 Hz.
        assert_ms(frame_interval(&output_with(60_000)), 1000.0 / 60.0);
        // 144 Hz: the old fixed 16 ms timer held this down to ~62 fps.
        assert_ms(frame_interval(&output_with(144_000)), 1000.0 / 144.0);
        // 30 Hz.
        assert_ms(frame_interval(&output_with(30_000)), 1000.0 / 30.0);
    }

    #[test]
    fn frame_interval_is_clamped_and_falls_back() {
        // A missing mode is 60 Hz.
        let output = Output::new(
            "test".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "Poly".into(),
                model: "Test".into(),
                serial_number: "Unknown".into(),
            },
        );
        assert_eq!(frame_interval(&output), FALLBACK_FRAME_INTERVAL);
        // A bogus refresh rate is 60 Hz, not a division by zero or a spin loop.
        assert_eq!(frame_interval(&output_with(0)), FALLBACK_FRAME_INTERVAL);
        assert_eq!(frame_interval(&output_with(-1)), FALLBACK_FRAME_INTERVAL);
        // Absurd modes stay inside the clamp.
        assert_eq!(frame_interval(&output_with(1_000_000)), MIN_FRAME_INTERVAL);
        assert_eq!(frame_interval(&output_with(100)), MAX_FRAME_INTERVAL);
    }

    #[test]
    fn frame_clock_snaps_to_the_vblank_grid() {
        let mut clock = FrameClock::default();
        let start = Instant::now();
        let interval = Duration::from_millis(10);

        // No presentation yet: there is no grid to snap to.
        clock.set_refresh_interval(Some(interval));
        assert_eq!(clock.next_presentation_time(start), start);

        clock.presented(start);
        // Called right after the VBlank: the frame lands on the next one.
        assert_eq!(
            clock.next_presentation_time(start + Duration::from_millis(1)),
            start + interval
        );
        // Called after two intervals: skip the VBlank that already passed.
        assert_eq!(
            clock.next_presentation_time(start + Duration::from_millis(11)),
            start + interval * 2
        );
        // A later presentation re-anchors the grid.
        let later = start + Duration::from_millis(15);
        clock.presented(later);
        assert_eq!(
            clock.next_presentation_time(later + Duration::from_millis(1)),
            later + interval
        );
    }

    #[test]
    fn frame_clock_handles_early_vblanks_and_interval_changes() {
        let mut clock = FrameClock::default();
        let start = Instant::now();
        clock.set_refresh_interval(Some(Duration::from_millis(10)));
        clock.presented(start + Duration::from_millis(5));
        // An early VBlank must not produce a time before the last one.
        assert_eq!(
            clock.next_presentation_time(start),
            start + Duration::from_millis(15)
        );

        // A mode change invalidates the old grid.
        clock.set_refresh_interval(Some(Duration::from_millis(20)));
        assert_eq!(clock.next_presentation_time(start), start);
        clock.presented(start);
        assert_eq!(
            clock.next_presentation_time(start + Duration::from_millis(1)),
            start + Duration::from_millis(20)
        );

        // A zero interval cannot be used as a divisor.
        clock.set_refresh_interval(Some(Duration::ZERO));
        assert_eq!(clock.next_presentation_time(start), start);
        clock.set_refresh_interval(None);
        assert_eq!(clock.next_presentation_time(start), start);
    }
}
