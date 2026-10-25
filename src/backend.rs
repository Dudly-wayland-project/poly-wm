// Ported from niri (GPL-3.0-or-later): src/backend/mod.rs

//! Backends: how frames reach the screen and how input arrives.
//!
//! The winit backend nests Poly inside another session (debugging), the udev
//! backend drives DRM/KMS directly from a TTY, and the headless backend has no
//! output at all (tests and CI).

pub mod headless;
pub mod udev;
pub mod winit;

use anyhow::anyhow;
use smithay::output::Output;
use smithay::reexports::calloop::LoopHandle;

use crate::cli::BackendKind;
use crate::core::Core;
use crate::state::State;

/// Outcome of rendering one output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderResult {
    /// A frame was drawn (and handed to the display).
    Submitted,
    /// Nothing changed, no frame was drawn.
    NoDamage,
    /// Rendering failed (inactive session, lost device, ...).
    Skipped,
}

/// The active backend.
///
/// There is exactly one of these per process, and the backends are wildly
/// different in size, so boxing the large variant would only add indirection.
#[allow(clippy::large_enum_variant)]
pub enum Backend {
    /// Nested window inside another wayland/X11 session.
    Winit(winit::Winit),
    /// Native TTY session via libseat + DRM/KMS.
    Udev(udev::Udev),
    /// No display.
    Headless(headless::Headless),
}

impl Backend {
    /// Start the requested backend.
    pub fn init(
        handle: &LoopHandle<'static, State>,
        core: &mut Core,
        kind: BackendKind,
    ) -> anyhow::Result<Self> {
        return match kind {
            BackendKind::Winit => Ok(Backend::Winit(winit::Winit::new(handle, core)?)),
            BackendKind::Udev => Ok(Backend::Udev(udev::Udev::new(handle, core)?)),
            BackendKind::Headless => Ok(Backend::Headless(headless::Headless::new(core)?)),
            BackendKind::Auto => Err(anyhow!("the backend must be resolved before starting it")),
        };
    }

    /// Render one output and report what happened.
    pub fn render(&mut self, core: &mut Core, output: &Output) -> RenderResult {
        return match self {
            Backend::Winit(backend) => backend.render(core, output),
            Backend::Udev(backend) => backend.render(core, output),
            Backend::Headless(backend) => backend.render(core, output),
        };
    }

    /// Switch to another virtual terminal (only the udev backend can do this).
    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Backend::Udev(backend) => backend.change_vt(vt),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    /// Switch `output` to `mode` at the backend level.
    ///
    /// Only the DRM backend owns the mode: it has to program the crtc, and the
    /// mode it picks must be one of the modes the connector reports. The nested
    /// window's size belongs to the host compositor, so a mode change there is
    /// refused instead of silently desynchronising the output from the window.
    pub fn set_output_mode(
        &mut self,
        output: &Output,
        mode: smithay::output::Mode,
    ) -> Result<(), String> {
        return match self {
            Backend::Udev(backend) => backend.set_output_mode(output, mode),
            Backend::Winit(_) => Err(format!(
                "output {} is a nested window; its size comes from the host compositor",
                output.name()
            )),
            // The headless output has no hardware behind it: the compositor-side
            // mode change is all there is, and it is useful for testing layouts.
            Backend::Headless(_) => Ok(()),
        };
    }

    /// Set variable refresh rate on `output`'s connector.
    ///
    /// Only the DRM backend owns a connector: VRR is a property of the hardware
    /// (FreeSync/Adaptive-Sync). The nested window's refresh rate belongs to the
    /// host compositor, and the headless output has no hardware at all, so both
    /// refuse the change instead of pretending it worked.
    pub fn set_output_vrr(&mut self, output: &Output, vrr: bool) -> Result<(), String> {
        return match self {
            Backend::Udev(backend) => backend.set_output_vrr(output, vrr),
            Backend::Winit(_) => Err(format!(
                "output {} is a nested window; its refresh rate belongs to the host compositor",
                output.name()
            )),
            Backend::Headless(_) => Err(format!(
                "output {} has no hardware to enable VRR on",
                output.name()
            )),
        };
    }

    /// Check whether this backend can rotate or mirror `output` itself.
    ///
    /// The DRM backend renders through `DrmCompositor`, which reads the
    /// transform off the output. The nested window keeps `Flipped180` for its
    /// own framebuffer and has one damage tracker built from that transform, so
    /// a second rotation would only desynchronise it from the host window.
    pub fn can_transform_output(&self, output: &Output) -> Result<(), String> {
        return match self {
            Backend::Udev(_) | Backend::Headless(_) => Ok(()),
            Backend::Winit(_) => Err(format!(
                "output {} is a nested window; its orientation belongs to the host compositor",
                output.name()
            )),
        };
    }

    /// Run `f` with the GLES renderer that draws `output`, if the backend has
    /// one.
    ///
    /// The close animation bakes the last window frame into a texture while the
    /// client surface still exists. The texture must be created by the same
    /// renderer that will draw it, so udev looks the renderer up from the
    /// output's own DRM node (multi-GPU). The headless backend has no GL
    /// context and returns `None`; it also does not draw frames, so there is no
    /// visible close animation to produce.
    pub fn with_gles_renderer_for_output<R>(
        &mut self,
        output: &Output,
        f: impl FnOnce(&mut smithay::backend::renderer::gles::GlesRenderer) -> R,
    ) -> Option<R> {
        return match self {
            Backend::Winit(backend) => backend.with_gles_renderer_for_output(output, f),
            Backend::Udev(backend) => backend.with_gles_renderer_for_output(output, f),
            Backend::Headless(_) => None,
        };
    }
}
