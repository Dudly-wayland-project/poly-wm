//! `zwlr_screencopy_manager_v1` (wlr-screencopy-unstable-v1).
//!
//! The protocol is deprecated upstream in favour of
//! `ext-image-copy-capture-v1` (which Poly already speaks,
//! `handlers/image_copy_capture.rs`), but a lot of software still only knows
//! this one — older `grim` builds, `wf-recorder`, `flameshot`, some Electron
//! screen share paths. Supporting it costs little because the hard part is
//! shared: rendering a frame and reading it back is the same work, only the
//! bookkeeping differs.
//!
//! The shape of the protocol is one-shot. `capture_output` or
//! `capture_output_region` creates a frame object, the compositor announces the
//! buffer it wants (`buffer`, plus `buffer_done` on v3), the client calls `copy`
//! with one, and the frame is answered with `flags` + `ready` or `failed`. A
//! frame is consumed by its copy: there is no damage history to keep and no
//! second picture to produce, which is why this does not reuse the session
//! machinery in `crate::core::capture` — but it does reuse the offscreen
//! texture, because `wf-recorder` allocates a frame per refresh.
//!
//! Deliberately not implemented:
//!
//! - `linux_dmabuf` buffers. The `linux_dmabuf` event is what unsolicited
//!   dmabuf support is announced with, so not sending it keeps v3 clients on
//!   shm, which every one of them supports;
//! - `damage` events. They are optional, and a client that receives none has to
//!   assume the whole buffer changed — which is exactly what we write.
//!
//! Only `Xrgb8888` is offered, matching what `ext-image-copy-capture` accepts
//! here (`docs/plan.md` §17).

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::output::{Output, WeakOutput};
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::{
    Error, Flags, Request as FrameRequest, ZwlrScreencopyFrameV1,
};
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::{
    Request as ManagerRequest, ZwlrScreencopyManagerV1,
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::{Client, DataInit, DisplayHandle, New, Resource};
use smithay::utils::{Physical, Point, Rectangle, Size};
use smithay::wayland::{Dispatch2, GlobalDispatch2};
use tracing::{debug, trace, warn};

use crate::state::State;

/// The version Poly advertises: the newest one, whose only addition over v2 is
/// `buffer_done` (which tells the client that the buffer enumeration is over).
pub(crate) const VERSION: u32 = 3;

/// User data of the `zwlr_screencopy_manager_v1` object itself.
///
/// The manager is only a factory: everything it needs is in `State`, and the
/// frames it created stay valid after it is destroyed.
pub struct ScreencopyManagerData;

/// User data of the global. Nothing lives here; the type exists to implement
/// [`GlobalDispatch2`] for it.
pub struct ScreencopyGlobal;

/// Announce `zwlr_screencopy_manager_v1` on the display.
///
/// Called from `Core::new`, which is where every global is created (AGENTS.md
/// §2.10); the display handle keeps the global alive for the lifetime of the
/// compositor, so no state has to be stored for it.
pub(crate) fn create_global(display_handle: &DisplayHandle) {
    display_handle.create_global::<State, ZwlrScreencopyManagerV1, ScreencopyGlobal>(
        VERSION,
        ScreencopyGlobal,
    );
    return;
}

impl GlobalDispatch2<ZwlrScreencopyManagerV1, State> for ScreencopyGlobal {
    fn bind(
        &self,
        _state: &mut State,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ScreencopyManagerData);
        return;
    }
}

impl Dispatch2<ZwlrScreencopyManagerV1, State> for ScreencopyManagerData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: ManagerRequest,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        return match request {
            ManagerRequest::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                create_frame(state, data_init, frame, &output, overlay_cursor != 0, None);
            }
            ManagerRequest::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                create_frame(
                    state,
                    data_init,
                    frame,
                    &output,
                    overlay_cursor != 0,
                    Some(Region {
                        x,
                        y,
                        w: width,
                        h: height,
                    }),
                );
            }
            // A destructor: the frames it created live on, so there is nothing
            // to clean up.
            ManagerRequest::Destroy => {}
            // The enum is `#[non_exhaustive]`: a newer manager request is one we
            // cannot answer, and ignoring it is what the protocol prescribes.
            _ => {}
        };
    }
}

/// Everything a frame needs to remember between its creation and its copy.
///
/// This is object user data, so it has to be `Send + Sync`; the offscreen
/// texture and the damage tracker live in `State::capture` for that reason.
struct FrameData {
    /// The captured output, if the client named one of ours.
    output: Option<WeakOutput>,
    /// The requested region, kept to detect an output resize before the copy.
    region: Option<Region>,
    /// What the `buffer` event announced, in framebuffer pixels.
    src: Option<Rectangle<i32, Physical>>,
    /// `overlay_cursor` from the manager request.
    overlay_cursor: bool,
    /// A frame can be copied exactly once (`error.already_used`).
    copied: AtomicBool,
}

/// A `capture_output_region` rectangle, exactly as the client sent it.
///
/// It stays raw integers until [`scale_region`] validated it: `Size::new`
/// asserts on a negative size, and a client is free to send one, so building the
/// rectangle at the protocol boundary would let a malformed request panic the
/// compositor in a debug build (AGENTS.md §2.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Region {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Create a frame object, announce the buffer it should get and fail it when the
/// capture can never succeed.
///
/// The frame is always created: the client already holds the object (it passed
/// the `new_id`) and `failed` is the only way to tell it that the request was
/// refused.
fn create_frame(
    state: &mut State,
    data_init: &mut DataInit<'_, State>,
    frame: New<ZwlrScreencopyFrameV1>,
    wl_output: &WlOutput,
    overlay_cursor: bool,
    region: Option<Region>,
) {
    let output = Output::from_resource(wl_output);
    // An output that is not in the space is one the compositor is no longer
    // showing (unplugged, or disabled by a configuration change); capturing it
    // would produce an empty picture.
    let live = output.as_ref().filter(|output| {
        return state
            .core
            .space
            .outputs()
            .any(|live| return live == *output);
    });
    let src = live.and_then(|output| return frame_source_rect(output, region));

    let data = FrameData {
        output: output.as_ref().map(|output| return output.downgrade()),
        region,
        src,
        overlay_cursor,
        copied: AtomicBool::new(false),
    };
    let frame = data_init.init(frame, data);
    let version = frame.version();

    let Some(src) = src else {
        debug!("screencopy frame failed: no such output to capture");
        frame.failed();
        return;
    };
    // The client picks stride and offset itself; the minimum is one row of
    // pixels, which is what a packed `Xrgb8888` buffer has.
    frame.buffer(
        wl_shm::Format::Xrgb8888,
        src.size.w as u32,
        src.size.h as u32,
        (src.size.w * 4) as u32,
    );
    // Since v3 the enumeration is only over once `buffer_done` says so; a v2
    // client sends `copy` right after `buffer` and would wait forever.
    if version >= 3 {
        frame.buffer_done();
    }
    return;
}

/// The framebuffer rectangle a frame of `output` captures.
///
/// The protocol gives the region in output *logical* coordinates (the space
/// `xdg_output.logical_size` describes, which is what `grim -g` divides its
/// global coordinates by), while rendering happens in the output's buffer
/// pixels, so the region is scaled by the output scale. It is then clipped to
/// the output, as the protocol requires. An empty result means there is nothing
/// to capture — a region entirely off the output, or an output without a mode.
fn frame_source_rect(output: &Output, region: Option<Region>) -> Option<Rectangle<i32, Physical>> {
    let mode = output.current_mode()?;
    if mode.size.w <= 0 || mode.size.h <= 0 {
        return None;
    }
    let full = Rectangle::from_size(mode.size);
    let Some(region) = region else {
        return Some(full);
    };
    return scale_region(region, output.current_scale().fractional_scale(), full);
}

/// Scale a logical region into buffer pixels and clip it to the output.
///
/// Split out from [`frame_source_rect`] because it is the part that goes wrong
/// silently: a region that is not scaled still produces a buffer of a plausible
/// size, just full of the wrong pixels.
fn scale_region(
    region: Region,
    scale: f64,
    full: Rectangle<i32, Physical>,
) -> Option<Rectangle<i32, Physical>> {
    // A client can send any rectangle, including a negative size. `Size::new`
    // asserts on that in a debug build and would produce a nonsense buffer in a
    // release build, so nothing is built until the values are known to be sane
    // (AGENTS.md §2.11).
    if region.w <= 0 || region.h <= 0 {
        return None;
    }
    // Rounding half away from zero, so a 1.5-scaled region of 3 logical pixels
    // is 5 buffer pixels and not 4.
    let scaled = |value: i32| -> i32 { return (f64::from(value) * scale).round() as i32 };
    let src = Rectangle::new(
        Point::from((scaled(region.x), scaled(region.y))),
        Size::from((scaled(region.w), scaled(region.h))),
    );
    // `intersection` only rejects regions that share no pixel row and column;
    // a zero-width one still overlaps, and it would end up as a request for a
    // buffer no client can allocate.
    let src = src.intersection(full)?;
    if src.size.w <= 0 || src.size.h <= 0 {
        return None;
    }
    return Some(src);
}

impl Dispatch2<ZwlrScreencopyFrameV1, State> for FrameData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: FrameRequest,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        return match request {
            // `copy_with_damage` only differs in that the client would like
            // `damage` events too; it gets none, so it has to assume everything
            // changed, which is what the copy writes.
            FrameRequest::Copy { buffer } | FrameRequest::CopyWithDamage { buffer } => {
                self.copy(state, resource, &buffer);
            }
            FrameRequest::Destroy => {}
            // `#[non_exhaustive]`: nothing else exists at this version.
            _ => {}
        };
    }
}

impl FrameData {
    fn copy(&self, state: &mut State, frame: &ZwlrScreencopyFrameV1, buffer: &WlBuffer) {
        if self.copied.swap(true, Ordering::SeqCst) {
            // Not a crash of ours: a client that copies a consumed frame would
            // read bytes we never wrote, so the protocol asks for a hard error
            // and only this client's connection goes away (AGENTS.md §2.11).
            warn!("screencopy frame copied twice, closing the client");
            frame.post_error(Error::AlreadyUsed as u32, "this frame was already copied");
            return;
        }
        let Some(output) = self.output.as_ref().and_then(|weak| return weak.upgrade()) else {
            warn!("screencopy copy failed: the output no longer exists");
            frame.failed();
            return;
        };
        if !state
            .core
            .space
            .outputs()
            .any(|live| return live == &output)
        {
            warn!(
                output = output.name(),
                "screencopy copy failed: the output is no longer in use"
            );
            frame.failed();
            return;
        }
        // The output may have been resized between the frame and the copy. The
        // client allocated its buffer for the size we announced, so the frame
        // cannot be filled; `failed` makes it ask for a new frame.
        if frame_source_rect(&output, self.region) != self.src {
            warn!(
                output = output.name(),
                announced = ?self.src,
                current = ?frame_source_rect(&output, self.region),
                "screencopy copy failed: the output changed size after the frame was announced"
            );
            frame.failed();
            return;
        }
        let Some(src) = self.src else {
            // The frame was refused when it was created and the client was told
            // so; answering the copy as well keeps the object from hanging.
            frame.failed();
            return;
        };

        // The renderer belongs to the backend, the element list and the capture
        // target to the core, so `State` is destructured: the same split
        // `handlers/image_copy_capture.rs` uses.
        let State {
            backend,
            core,
            capture,
        } = state;
        let copied = backend
            .with_gles_renderer_for_output(&output, |renderer| {
                return crate::core::render::capture_region(
                    core,
                    capture,
                    renderer,
                    &output,
                    src,
                    self.overlay_cursor,
                    buffer,
                );
            })
            .unwrap_or_else(|| {
                return Err(anyhow::anyhow!("no renderer for output {}", output.name()));
            });
        match copied {
            Ok(()) => {
                trace!(output = output.name(), ?src, "copied a screencopy frame");
                // Flags stay empty: the picture is top-down, as the protocol's
                // default orientation.
                frame.flags(Flags::empty());
                let elapsed = core.start_time.elapsed();
                let secs = elapsed.as_secs();
                frame.ready(
                    (secs >> 32) as u32,
                    (secs & 0xffff_ffff) as u32,
                    elapsed.subsec_nanos(),
                );
            }
            Err(err) => {
                warn!(
                    "error copying a screencopy frame from {}: {err}",
                    output.name()
                );
                frame.failed();
            }
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logical(x: i32, y: i32, w: i32, h: i32) -> Region {
        return Region { x, y, w, h };
    }

    fn physical(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        return Rectangle::new(Point::from((x, y)), Size::from((w, h)));
    }

    #[test]
    fn a_region_is_scaled_into_buffer_pixels() {
        // `grim -g "100,100 400x300"` on a 2x output: the region is logical,
        // the buffer is measured in output pixels.
        assert_eq!(
            scale_region(logical(100, 100, 400, 300), 2.0, physical(0, 0, 3840, 2160)),
            Some(physical(200, 200, 800, 600))
        );
    }

    #[test]
    fn a_fractional_scale_rounds_to_the_nearest_pixel() {
        // 3 logical pixels at 1.5 are 4.5 buffer pixels; rounding up keeps the
        // client from losing the right-hand pixel column.
        assert_eq!(
            scale_region(logical(1, 1, 3, 3), 1.5, physical(0, 0, 1000, 1000)),
            Some(physical(2, 2, 5, 5))
        );
    }

    #[test]
    fn a_region_is_clipped_to_the_output() {
        // The protocol says the region is clipped to the output's extents: the
        // client's buffer is then only as large as the part that exists.
        assert_eq!(
            scale_region(
                logical(4900, 2100, 200, 200),
                1.0,
                physical(0, 0, 5000, 2200)
            ),
            Some(physical(4900, 2100, 100, 100))
        );
    }

    #[test]
    fn a_region_entirely_off_the_output_captures_nothing() {
        assert_eq!(
            scale_region(logical(6000, 0, 100, 100), 1.0, physical(0, 0, 5000, 2200)),
            None
        );
        // A region with no area is not a frame either.
        assert_eq!(
            scale_region(logical(10, 10, 0, 100), 1.0, physical(0, 0, 100, 100)),
            None
        );
        // A negative size comes from a client that passed garbage.
        assert_eq!(
            scale_region(logical(10, 10, -50, 20), 1.0, physical(0, 0, 100, 100)),
            None
        );
    }
}
