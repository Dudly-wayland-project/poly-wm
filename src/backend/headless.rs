// Ported from niri (GPL-3.0-or-later): src/backend/headless.rs

//! Headless backend: there are outputs, but nothing is drawn.
//!
//! Used by tests and CI (`poly --backend headless`). Clients can connect, map
//! windows and get frame callbacks; frames are "presented" immediately, since
//! there is nothing to wait for.

use std::time::{Duration, Instant};

use smithay::backend::renderer::element::RenderElementStates;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Monotonic, Point, Transform};
use smithay::wayland::presentation::Refresh;

use crate::backend::RenderResult;
use crate::core::Core;
use crate::core::redraw::RedrawState;
use crate::core::render;
use crate::state::State;

/// Size of the headless output.
const SIZE: (i32, i32) = (1920, 1080);
/// Refresh rate reported for the headless output.
const REFRESH: i32 = 60_000;

/// The headless outputs.
pub struct Headless {
    outputs: Vec<Output>,
}

impl Headless {
    /// Create the headless output.
    pub fn new(core: &mut Core) -> anyhow::Result<Self> {
        // There is no renderer to derive shm formats from; the two mandatory
        // formats are always safe to advertise, and clients must be able to
        // allocate buffers to commit anything at all.
        core.shm_state
            .update_formats([wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888]);

        let mode = Mode {
            size: SIZE.into(),
            refresh: REFRESH,
        };
        let output = Output::new(
            "headless".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Poly".into(),
                model: "Headless".into(),
                serial_number: "Unknown".into(),
            },
        );
        let global = output.create_global::<State>(&core.display_handle);
        core.output_state(&output).global = Some(global);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            None,
            Some(Point::from((0, 0))),
        );
        output.set_preferred(mode);
        core.space.map_output(&output, (0, 0));

        return Ok(Self {
            outputs: vec![output],
        });
    }

    /// "Present" a frame: there is nothing to draw, so this never fails.
    pub fn render(&mut self, core: &mut Core, output: &Output) -> RenderResult {
        if !self.outputs.contains(output) {
            return RenderResult::Skipped;
        }

        core.output_state(output).redraw_state = RedrawState::Idle;

        // Headless frames are presented as soon as they are rendered.
        core.frame_presented(output, Instant::now());
        let time = core.monotonic_now();

        // Nothing was drawn, so the element states are empty; presenting the
        // feedbacks anyway keeps clients' frame pacing in a sane rhythm.
        let mut feedback =
            render::take_presentation_feedbacks(core, output, &RenderElementStates::default());
        feedback.presented::<_, Monotonic>(
            time,
            Refresh::fixed(Duration::from_secs_f64(1_000.0 / REFRESH as f64)),
            0,
            wp_presentation_feedback::Kind::empty(),
        );
        render::send_frame_callbacks(core, output, time);

        // Nothing here waits for a VBlank, so this is also the frame boundary
        // that continues an ongoing animation.
        if core.layout.animations_running() {
            core.queue_redraw(output);
        }

        return RenderResult::Submitted;
    }
}
