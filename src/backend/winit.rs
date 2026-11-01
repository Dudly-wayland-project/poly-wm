// Ported from niri (GPL-3.0-or-later): src/backend/winit.rs
// Ported from anvil (GPL-3.0-or-later): anvil/src/winit.rs

//! Nested backend: Poly runs as a window inside the host session.
//!
//! This is the debugging backend (`poly --backend winit`): input comes from the
//! host window, and frames are drawn into it with GLES2.

use std::time::Instant;

use anyhow::anyhow;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportDma as _, ImportEgl as _, ImportMemWl as _};
use smithay::backend::winit::{WinitEvent, WinitGraphicsBackend, init_from_attributes};
use smithay::desktop::layer_map_for_output;
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::winit::dpi::LogicalSize;
use smithay::reexports::winit::window::WindowAttributes;
use smithay::utils::{Monotonic, Transform};
use smithay::wayland::dmabuf::DmabufGlobal;
use smithay::wayland::presentation::Refresh;
use tracing::{debug, info, trace, warn};

use crate::backend::{Backend, RenderResult};
use crate::core::redraw::RedrawState;
use crate::core::render;
use crate::core::{self, Core};
use crate::state::State;

/// Refresh rate reported for the nested window.
///
/// Only used when the host monitor does not report one.
const REFRESH: i32 = 60_000;

/// The host monitor's refresh rate in millihertz.
///
/// A nested session runs at whatever the host display does; assuming 60 Hz
/// makes animations jitter against a 144 Hz host because the frame clock and
/// the animation watchdog both use the mode.
fn host_refresh(window: &dyn smithay::reexports::winit::window::Window) -> i32 {
    let Some(monitor) = window.current_monitor() else {
        return REFRESH;
    };
    let Some(mode) = monitor.current_video_mode() else {
        return REFRESH;
    };
    return mode
        .refresh_rate_millihertz()
        .and_then(|rate| return i32::try_from(rate.get()).ok())
        .unwrap_or(REFRESH);
}

/// The nested window; there is exactly one output.
pub struct Winit {
    output: Output,
    backend: WinitGraphicsBackend<GlesRenderer>,
    damage_tracker: OutputDamageTracker,
    /// Whether the host is waiting for a frame from us. The host's redraw event
    /// is the frame boundary of a nested window: drawing more frames than the
    /// host consumes would just pile up buffers.
    host_frame_ready: bool,
    /// A redraw was queued while the host was not ready to show a frame.
    ///
    /// The queued state cannot be left in place: `refresh_and_flush_clients`
    /// runs on every event-loop iteration and would retry the render each time
    /// (measured: 439 attempts for 16 host frames while a client mapped). The
    /// state is cleared here and the host's next `Redraw` re-queues it.
    deferred_redraw: bool,
    /// Kept so the dmabuf global can be disabled when a more capable one
    /// becomes available.
    _dmabuf_global: DmabufGlobal,
}

impl Winit {
    /// Create the nested window, its output, and hook up the event source.
    pub fn new(handle: &LoopHandle<'static, State>, core: &mut Core) -> anyhow::Result<Self> {
        let attributes = WindowAttributes::default()
            .with_surface_size(LogicalSize::new(1280.0, 800.0))
            .with_title("Poly");
        let (mut backend, event_loop) = init_from_attributes::<GlesRenderer>(attributes)
            .map_err(|err| return anyhow!("error creating the winit window: {err}"))?;

        let renderer = backend.renderer();
        if let Err(err) = renderer.bind_wl_display(&core.display_handle) {
            // wl_drm is deprecated; most clients no longer need it.
            debug!("error binding the EGL display to wl_display: {err}");
        }
        core.shm_state.update_formats(renderer.shm_formats());
        let dmabuf_global = core
            .dmabuf_state
            .create_global::<State>(&core.display_handle, renderer.dmabuf_formats());

        let mode = Mode {
            size: backend.window_size(),
            refresh: host_refresh(backend.window()),
        };
        let output = Output::new(
            "winit".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Poly".into(),
                model: "Winit".into(),
                serial_number: "Unknown".into(),
            },
        );
        let global = output.create_global::<State>(&core.display_handle);
        core.output_state(&output).global = Some(global);
        // The winit window's origin is bottom-left, like OpenGL's.
        output.change_current_state(
            Some(mode),
            Some(Transform::Flipped180),
            Some(Scale::Fractional(backend.scale_factor())),
            None,
        );
        output.set_preferred(mode);
        core.space.map_output(&output, (0, 0));

        let damage_tracker = OutputDamageTracker::from_output(&output);

        let event_output = output.clone();
        handle
            .insert_source(event_loop, move |event, _, state| {
                trace!(?event, "winit event");
                match event {
                    WinitEvent::Resized { size, scale_factor } => {
                        debug!("winit window resized to {size:?}");
                        let refresh = match &state.backend {
                            Backend::Winit(winit) => host_refresh(winit.backend.window()),
                            _ => REFRESH,
                        };
                        let mode = Mode { size, refresh };
                        // The host decides the size, and normally the scale
                        // too — but `outputs.<name>.scale` overrides the scale
                        // (`docs/config.md`), and this event arrives after
                        // startup, so it must not undo that.
                        let configured = state.core.config.output_for(&event_output.name());
                        let scale = match configured.and_then(|config| return config.scale) {
                            Some(poly_config::OutputScale::Fixed(factor)) => factor,
                            _ => scale_factor,
                        };
                        event_output.change_current_state(
                            Some(mode),
                            None,
                            Some(Scale::Fractional(scale)),
                            None,
                        );
                        event_output.set_preferred(mode);
                        // `LayerMap` caches its non-exclusive zone (created
                        // from the old mode); the layout work area reads that
                        // cache, so without rearranging it the windows keep
                        // the old size until a layer surface changes.
                        layer_map_for_output(&event_output).arrange();
                        crate::core::layout::LayoutManager::relayout(state);
                        state.core.queue_redraw(&event_output);
                    }
                    WinitEvent::Focus(_) => (),
                    WinitEvent::Input(event) => core::input::process_input_event(state, event),
                    // The host is ready for a frame; the drawing itself happens
                    // in the event loop callback, see `Winit::render`.
                    WinitEvent::Redraw => {
                        if let Backend::Winit(winit) = &mut state.backend {
                            winit.host_frame_ready = true;
                        }
                        // The host is ready to draw again: the previously
                        // submitted frame is on screen. That boundary is what
                        // the animation frame clock snaps to.
                        state.core.frame_presented(&event_output, Instant::now());
                        // The host frame boundary is what drives animation
                        // frames: queueing here (rather than from a wall-clock
                        // timer) keeps every animation step on a presented
                        // frame and removes the second clock that used to beat
                        // against the refresh rate. An idle session queues
                        // nothing, so a still screen still renders no frames.
                        // A redraw that was waiting for the host frame
                        // boundary is due again now; both reasons may hold at
                        // once, so take the flag unconditionally.
                        let deferred = match &mut state.backend {
                            Backend::Winit(winit) => std::mem::take(&mut winit.deferred_redraw),
                            _ => false,
                        };
                        if deferred || state.core.layout.animations_running() {
                            state.core.queue_redraw(&event_output);
                        }
                    }
                    WinitEvent::CloseRequested => state.core.stop_signal.stop(),
                }
            })
            .map_err(|err| return anyhow!("error inserting the winit event source: {err:?}"))?;

        info!(
            "winit output ready ({}x{} at scale {})",
            mode.size.w,
            mode.size.h,
            backend.scale_factor()
        );

        return Ok(Self {
            output,
            backend,
            damage_tracker,
            // The host is waiting for the first frame.
            host_frame_ready: true,
            deferred_redraw: false,
            _dmabuf_global: dmabuf_global,
        });
    }

    /// Access the GLES renderer for one-shot offscreen work (close snapshots).
    ///
    /// The nested backend has exactly one output, so the output argument is
    /// irrelevant.
    pub fn with_gles_renderer_for_output<R>(
        &mut self,
        _output: &Output,
        f: impl FnOnce(&mut GlesRenderer) -> R,
    ) -> Option<R> {
        return Some(f(self.backend.renderer()));
    }

    /// Render one output and report what happened.
    pub fn render(&mut self, core: &mut Core, output: &Output) -> RenderResult {
        if output != &self.output {
            return RenderResult::Skipped;
        }

        // The host's redraw event is the frame boundary of a nested window;
        // until it arrives, the queued redraw has to wait for it. Keep it out
        // of the redraw state while it waits: the event loop retries every
        // iteration, and each retry would cost a full `output_elements` walk.
        if !self.host_frame_ready {
            if core.output_state(&self.output).redraw_state.is_queued() {
                core.output_state(&self.output).redraw_state = RedrawState::Idle;
                self.deferred_redraw = true;
            }
            return RenderResult::Skipped;
        }

        // Consume the queued redraw; whatever queues one while we draw is
        // rendered on the next host frame.
        core.output_state(&self.output).redraw_state = RedrawState::Idle;

        // The age of the buffer we are about to draw into, needed to repair the
        // damage of the frames it was last used for. It is only meaningful
        // while the previous frame is still bound, so query it before binding.
        let age = self.backend.buffer_age().unwrap_or(0);
        let now = core.begin_frame(&self.output);

        let (damage, mut feedback) = {
            let (renderer, mut framebuffer) = match self.backend.bind() {
                Ok(bound) => bound,
                Err(err) => {
                    warn!("error binding the winit window: {err:?}");
                    return RenderResult::Skipped;
                }
            };

            let elements = render::output_elements(core, renderer, &self.output, true, now);
            let background = core
                .output_background(&self.output)
                .map(|color| return color.to_floats())
                .unwrap_or([0.1, 0.1, 0.1, 1.0]);
            let result = match self.damage_tracker.render_output(
                renderer,
                &mut framebuffer,
                age,
                &elements,
                background,
            ) {
                Ok(result) => result,
                Err(err) => {
                    warn!("error rendering to the winit window: {err:?}");
                    return RenderResult::Skipped;
                }
            };

            let damage = result.damage.cloned();
            let feedback = render::take_presentation_feedbacks(core, &self.output, &result.states);
            (damage, feedback)
        };

        let time = core.monotonic_now();

        let Some(damage) = damage else {
            // Nothing changed, so the host keeps the last frame. Clients still
            // get frame callbacks: to them this is a frame boundary.
            trace!("no damage, not presenting a frame");
            render::send_frame_callbacks(core, &self.output, time);
            return RenderResult::NoDamage;
        };
        trace!("presenting a frame with {} damage rects", damage.len());
        if let Err(err) = self.backend.submit(Some(&damage)) {
            warn!("error presenting the winit window: {err:?}");
            return RenderResult::Skipped;
        }

        // `submit` asked the host for a frame callback on the buffer we just
        // presented; the redraw request makes it report that callback as the
        // next `WinitEvent::Redraw`, pacing us to one frame per host frame.
        self.host_frame_ready = false;
        self.backend.window().request_redraw();

        feedback.presented::<_, Monotonic>(
            time,
            Refresh::Unknown,
            0,
            wp_presentation_feedback::Kind::empty(),
        );
        render::send_frame_callbacks(core, &self.output, time);

        // The animation for the frame we just presented has to advance exactly
        // once, on the next host frame. `host_frame_ready` turns true on the
        // host's `Redraw`, and the event source queues a redraw there, so an
        // ongoing animation reaches the next frame the host offers.
        return RenderResult::Submitted;
    }
}
