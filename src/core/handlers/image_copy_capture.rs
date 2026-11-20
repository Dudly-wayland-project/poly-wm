//! `ext-image-copy-capture-v1` + `ext-image-capture-source-v1`.
//!
//! This is what `grim`, `wl-mirror`, screen recorders and `xdg-desktop-portal`
//! screencasts use on Poly, and they prefer this pair over
//! `wlr-screencopy-unstable-v1` whenever both are offered. The older protocol is
//! implemented too, for the clients that only know it
//! (`handlers/screencopy.rs`); it shares the rendering and readback code but not
//! this session machinery, because a screencopy frame is copied once and thrown
//! away.
//!
//! Capture is on demand: a frame is rendered as soon as the client calls
//! `capture`, from the live window/layer state, into the client's own shm
//! buffer (`render::capture_output`). Each session keeps an offscreen texture
//! and a damage tracker (`crate::core::capture`), so a capture never shows a
//! stale frame while a stream of captures only redraws, reads back and copies
//! what changed — and costs nothing while the screen is idle.
//!
//! The protocol only supports `Xrgb8888` shm buffers for now, which is enough
//! for every client that matters today.
//!
//! Toplevel capture (`grim -T`) needs `ext-foreign-toplevel-image-capture-
//! source-v1`; Poly only implements output capture, so those clients see no
//! toplevel source manager and refuse to run.

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_pointer::WlPointer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Buffer, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::image_capture_source::{
    ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
    OutputCaptureSourceState,
};
use smithay::wayland::image_copy_capture::{
    BufferConstraints, CaptureFailureReason, CursorSession, CursorSessionRef, Frame, FrameRef,
    ImageCopyCaptureHandler, ImageCopyCaptureState, Session, SessionRef,
};
use tracing::{debug, info, warn};

use crate::core::Core;
use crate::core::capture::{CaptureKey, CaptureOutcome};
use crate::state::State;

impl ImageCaptureSourceHandler for State {}

impl OutputCaptureSourceHandler for State {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        return &mut self.core.output_capture_source_state;
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        // Remember which output this source stands for; `capture_constraints`
        // and `frame` read it back through the source's user data.
        source.user_data().insert_if_missing(|| {
            return output.downgrade();
        });
    }
}

impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        return &mut self.core.image_copy_capture_state;
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        let Some(output) = source_output(source) else {
            debug!("capture refused: the source is not an output of ours");
            return None;
        };
        return output_constraints(&output);
    }

    fn new_session(&mut self, session: Session) {
        // A ticket for the per-session state in `State::capture`. It goes into
        // the session's user data because `SessionRef` has no object id to key a
        // map by, and the GL objects themselves cannot live there (user data has
        // to stay `Send + Sync`).
        let key = self.capture.create();
        session.user_data().insert_if_missing(|| return key);
        // Smithay stops the session as soon as this value is dropped, so it has
        // to live until the client destroys the session object.
        self.core.image_copy_sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let Some(output) = source_output(&session.source()) else {
            warn!("capture frame dropped: the session's output is gone");
            frame.fail(CaptureFailureReason::Unknown);
            return;
        };
        let Some(key) = session.user_data().get::<CaptureKey>().copied() else {
            warn!("capture frame dropped: the session has no capture state");
            frame.fail(CaptureFailureReason::Unknown);
            return;
        };
        let draw_cursor = session.draw_cursor();
        let buffer = frame.buffer();
        // The renderer belongs to the backend, this state to the core; splitting
        // the borrow is what lets the element list be built while rendering and
        // the session state be updated in the same call
        // (`crate::core::render::capture_output`).
        let State {
            backend,
            core,
            capture,
        } = self;
        let captured = backend
            .with_gles_renderer_for_output(&output, |renderer| {
                return crate::core::render::capture_output(
                    core,
                    capture,
                    key,
                    renderer,
                    &output,
                    draw_cursor,
                    &buffer,
                );
            })
            .unwrap_or_else(|| {
                return Err(anyhow::anyhow!("no renderer for output {}", output.name()));
            });
        match captured {
            Ok(CaptureOutcome::Deferred) => {
                // The pixels are copied out but still on the GPU. Holding the
                // frame is what lets `success` be sent once they land instead of
                // blocking the event loop until the fence signals; the protocol
                // does not bound how soon `ready` follows `capture`.
                self.capture.waiting.insert(
                    key,
                    crate::core::capture::WaitingFrame {
                        frame,
                        output: output.clone(),
                        timestamp: core.monotonic_now(),
                    },
                );
            }
            Ok(outcome) => {
                debug!(
                    output = output.name(),
                    ?outcome,
                    "captured output for a client"
                );
                // `Full` reports the whole buffer, `Damage` only the regions
                // that were written, `Unchanged` none at all — the client's
                // buffer already holds the current picture. The protocol
                // measures damage against the previous `ready` of the session,
                // and we always report a superset of what we wrote.
                frame.success(
                    Transform::Normal,
                    outcome.frame_damage(),
                    core.monotonic_now(),
                );
            }
            Err(err) => {
                warn!("error capturing output {}: {err}", output.name());
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        // Nothing is queued for a frame: it is either rendered inside `frame`
        // or already dead. A frame object that is destroyed before `capture`
        // simply never reaches us.
        let _ = frame;
    }

    /// What a client has to allocate to receive the pointer sprite.
    ///
    /// Without this the whole cursor session is refused (Smithay marks it
    /// stopped), and a client that only wants the cursor — a screenshot tool
    /// drawing it onto a capture, a portal showing it in a preview — falls back
    /// to waiting out its timeout with no cursor at all.
    fn cursor_capture_constraints(
        &mut self,
        source: &ImageCaptureSource,
        _pointer: &WlPointer,
    ) -> Option<BufferConstraints> {
        let output = source_output(source)?;
        let integer = output.current_scale().integer_scale();
        let size = crate::core::render::cursor_capture_size(&self.core, integer)?;
        // `Argb8888` and not `Xrgb8888`: the sprite is composited by the
        // client, so everything around it has to stay transparent.
        return Some(BufferConstraints {
            size: Size::from((size.w, size.h)),
            shm: vec![wl_shm::Format::Argb8888],
            dma: None,
        });
    }

    fn new_cursor_session(&mut self, session: CursorSession) {
        // The hotspot has to be in place before `enter`, which carries it.
        if let Some(output) = source_output(&session.source()) {
            let integer = output.current_scale().integer_scale();
            session.set_cursor_hotspot(crate::core::render::cursor_capture_hotspot(
                &self.core, integer,
            ));
            announce_cursor_position(&session, &self.core, &output);
        }
        // Dropping the value stops the session, so it has to outlive this call.
        self.core.cursor_sessions.push(session);
    }

    fn cursor_frame(&mut self, session: &CursorSessionRef, frame: Frame) {
        let Some(output) = source_output(&session.source()) else {
            warn!("cursor frame dropped: the session's output is gone");
            frame.fail(CaptureFailureReason::Unknown);
            return;
        };
        let buffer = frame.buffer();
        let State {
            backend,
            core,
            capture: _,
        } = self;
        let captured = backend
            .with_gles_renderer_for_output(&output, |renderer| {
                return crate::core::render::capture_cursor(core, renderer, &output, &buffer);
            })
            .unwrap_or_else(|| {
                return Err(anyhow::anyhow!("no renderer for output {}", output.name()));
            });
        match captured {
            Ok(()) => {
                // `None` means "everything is new", which is what a buffer the
                // client is about to composite from scratch contains.
                frame.success(
                    Transform::Normal,
                    None::<Vec<Rectangle<i32, Buffer>>>,
                    core.monotonic_now(),
                );
            }
            Err(err) => {
                warn!("error capturing the cursor of {}: {err}", output.name());
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }

    fn cursor_session_destroyed(&mut self, session: CursorSessionRef) {
        self.core
            .cursor_sessions
            .retain(|live| return **live != session);
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        if let Some(key) = session.user_data().get::<CaptureKey>().copied() {
            // A frame whose readback was still queued can never be completed
            // now; dropping it without `success` would leave the client waiting
            // for a `ready`, so it is failed explicitly.
            if let Some(waiting) = self.capture.waiting.remove(&key) {
                waiting.frame.fail(CaptureFailureReason::Unknown);
            }
            self.capture.remove(key);
        }
        self.core
            .image_copy_sessions
            .retain(|live| return **live != session);
        self.core.image_copy_capture_state.cleanup();
    }
}

/// The output a capture source stands for, if it is one of ours.
fn source_output(source: &ImageCaptureSource) -> Option<Output> {
    return source
        .user_data()
        .get::<smithay::output::WeakOutput>()
        .and_then(|weak| return weak.upgrade());
}

/// The buffer constraints a client capturing `output` has to satisfy.
fn output_constraints(output: &Output) -> Option<BufferConstraints> {
    let Some(mode) = output.current_mode() else {
        debug!("capture refused: output {} has no mode", output.name());
        return None;
    };
    if mode.size.w <= 0 || mode.size.h <= 0 {
        return None;
    }
    // Buffer coordinates, i.e. the size the client has to allocate. Grim and
    // friends only use shm, and every one of them understands `Xrgb8888`;
    // dmabuf would need the render node of the output's GPU.
    return Some(BufferConstraints {
        size: Size::from((mode.size.w, mode.size.h)),
        shm: vec![wl_shm::Format::Xrgb8888],
        dma: None,
    });
}

/// Keep the capture sessions in sync with the outputs they capture.
///
/// Two things the protocol has no other way to express:
///
/// * A session outlives the output unless the compositor stops it. Smithay
///   stops a session when its `Session` value is dropped (that is what
///   `Session::stop` does), so an output that went away means dropping the
///   session here — otherwise `xdg-desktop-portal-wlr`-style clients keep
///   capturing an output that no longer exists and never learn about it.
/// * When the output changes size the client has to be told, because it
///   allocates buffers up front and re-uses them. `update_constraints` is the
///   only way to send a new `buffer_size`, and without it a client keeps
///   handing us buffers of the old size while the renderer paints the new one.
///
/// This runs from the event loop instead of the hotplug paths because those
/// only borrow `&mut Core`, and the capture state (per-session offscreen
/// textures) lives on `State`.
pub(crate) fn refresh_sessions(state: &mut State) {
    // Frames whose readback was queued on the GPU: land the ones the GPU has
    // finished with and keep the rest for the next iteration. This runs before
    // the stop/resize bookkeeping below, so a frame that is due is reported
    // before its output can be torn down under it.
    finish_deferred_frames(state);

    let mut stopped = Vec::new();
    let mut resized = Vec::new();
    for (index, session) in state.core.image_copy_sessions.iter().enumerate() {
        // A source whose output object is already gone upgrades to `None`; the
        // session has to be stopped either way (`space` also drops the output
        // when it is merely disabled, which keeps the object alive).
        let Some(output) = source_output(&session.source()) else {
            stopped.push((index, String::from("an output that no longer exists")));
            continue;
        };
        if !state.core.space.outputs().any(|live| live == &output) {
            stopped.push((index, output.name()));
            continue;
        }
        let Some(constraints) = output_constraints(&output) else {
            continue;
        };
        // Constraints carry no `PartialEq`; only the size can change for the
        // shm-only setup we advertise, and `shm` never does.
        let stale = constraints_stale(session.current_constraints().as_ref(), &constraints);
        if stale {
            resized.push((index, output.name(), constraints));
        }
    }
    for (index, name, constraints) in resized {
        debug!(
            output = name,
            size = ?constraints.size,
            "re-sending capture constraints after an output resize"
        );
        if let Some(session) = state.core.image_copy_sessions.get(index) {
            session.update_constraints(constraints);
        }
    }
    // Drops the session, which is what sends `stopped` and fails the frame in
    // flight. The per-session capture state stays until `session_destroyed`, so
    // a client that still destroys its session object releases the texture.
    for (index, name) in stopped.into_iter().rev() {
        // A queued readback belongs to a frame this session will never answer;
        // it has to be failed before the session value is dropped, because that
        // is what makes the protocol object go away.
        if let Some(key) = state
            .core
            .image_copy_sessions
            .get(index)
            .and_then(|session| return session.user_data().get::<CaptureKey>().copied())
        {
            if let Some(waiting) = state.capture.waiting.remove(&key) {
                waiting.frame.fail(CaptureFailureReason::Unknown);
            }
        }
        state.core.image_copy_sessions.remove(index);
        info!("stopped a capture session for {name}");
    }
}

/// Where the pointer hotspot sits on `output`, in buffer pixels.
///
/// Land every capture frame whose readback the GPU finished.
///
/// A frame that is not ready stays in [`CaptureState::waiting`] and is retried on
/// the next iteration; one whose session went away is failed, because its pixels
/// can never be completed.
fn finish_deferred_frames(state: &mut State) {
    if state.capture.waiting.is_empty() {
        return;
    }
    let keys: Vec<_> = state.capture.waiting.keys().copied().collect();
    for key in keys {
        let Some(waiting) = state.capture.waiting.get(&key) else {
            continue;
        };
        let output = waiting.output.clone();
        let timestamp = waiting.timestamp;
        let landed = {
            let State {
                backend,
                core: _,
                capture,
            } = state;
            backend
                .with_gles_renderer_for_output(&output, |renderer| {
                    return capture.finish_session_readback(key, renderer);
                })
                .unwrap_or_else(|| {
                    return Err(anyhow::anyhow!("no renderer for output {}", output.name()));
                })
        };
        match landed {
            // Still queued: try again next iteration.
            Ok(None) => continue,
            Ok(Some(outcome)) => {
                let Some(waiting) = state.capture.waiting.remove(&key) else {
                    continue;
                };
                debug!(
                    output = output.name(),
                    ?outcome,
                    "landed a deferred capture frame"
                );
                waiting
                    .frame
                    .success(Transform::Normal, outcome.frame_damage(), timestamp);
            }
            Err(err) => {
                warn!("error landing a capture frame on {}: {err}", output.name());
                if let Some(waiting) = state.capture.waiting.remove(&key) {
                    waiting.frame.fail(CaptureFailureReason::Unknown);
                }
            }
        }
    }
}

/// Where the pointer hotspot sits on `output`, in buffer pixels.
///
/// This is what `ext_image_copy_capture_cursor_session_v1.position` reports:
/// the hotspot relative to the capture source's origin, so the client can
/// composite the sprite where it belongs without knowing the output layout.
fn cursor_position(core: &Core, output: &Output) -> Option<Point<i32, Buffer>> {
    let geometry = core.space.output_geometry(output)?;
    let pointer = core.pointer_location();
    if !geometry.to_f64().contains(pointer) {
        return None;
    }
    let scale = Scale::from(output.current_scale().fractional_scale());
    let position = (pointer - geometry.loc.to_f64())
        .to_physical(scale)
        .to_i32_round();
    return Some(Point::from((position.x, position.y)));
}

/// Tell a freshly created cursor session where the pointer is.
///
/// A pointer that is not on the captured output still has to be mentioned once:
/// a client decides there is no cursor to draw when it sees `leave`, and a
/// session that never sends `enter` or `leave` leaves it waiting out a timeout.
fn announce_cursor_position(session: &CursorSessionRef, core: &Core, output: &Output) {
    match cursor_position(core, output) {
        Some(position) => session.set_cursor_pos(Some(position)),
        None => {
            session.set_cursor_pos(Some(Point::from((0, 0))));
            session.set_cursor_pos(None);
        }
    }
}

/// Keep the live cursor sessions in sync with the pointer.
///
/// The protocol has no "the cursor changed" announcement of its own: position,
/// hotspot and buffer constraints are pull-shaped, so they are refreshed from
/// the event loop. `set_cursor_pos` and `set_cursor_hotspot` are no-ops when
/// nothing changed, and the constraints are only re-sent when the sprite
/// changes size — a client that allocated a 24x24 buffer would otherwise be
/// handed a 32x32 frame.
pub(crate) fn refresh_cursor_sessions(core: &Core) {
    if core.cursor_sessions.is_empty() {
        return;
    }
    let mut updates = Vec::with_capacity(core.cursor_sessions.len());
    for session in &core.cursor_sessions {
        let Some(output) = source_output(&session.source()) else {
            updates.push(None);
            continue;
        };
        let integer = output.current_scale().integer_scale();
        updates.push(Some((
            cursor_position(core, &output),
            crate::core::render::cursor_capture_hotspot(core, integer),
            crate::core::render::cursor_capture_size(core, integer),
        )));
    }
    for (session, update) in core.cursor_sessions.iter().zip(updates) {
        let Some((position, hotspot, size)) = update else {
            continue;
        };
        session.set_cursor_hotspot(hotspot);
        session.set_cursor_pos(position);
        let Some(size) = size else {
            continue;
        };
        let constraints = BufferConstraints {
            size: Size::from((size.w, size.h)),
            shm: vec![wl_shm::Format::Argb8888],
            dma: None,
        };
        if constraints_stale(session.current_constraints().as_ref(), &constraints) {
            session.update_constraints(constraints);
        }
    }
}

/// Whether the client has to be told about the constraints again.
///
/// `BufferConstraints` has no `PartialEq` and the shm format list never changes
/// here, so the size is the whole story: a client that allocated 1920x1080
/// buffers keeps handing them over while the output is 2560x1600.
pub(crate) fn constraints_stale(
    current: Option<&BufferConstraints>,
    wanted: &BufferConstraints,
) -> bool {
    return match current {
        Some(current) => current.size != wanted.size,
        None => true,
    };
}

#[cfg(test)]
mod tests {
    use smithay::utils::Buffer as BufferCoords;

    use super::*;

    fn constraints(width: i32, height: i32) -> BufferConstraints {
        return BufferConstraints {
            size: Size::<i32, BufferCoords>::from((width, height)),
            shm: vec![wl_shm::Format::Xrgb8888],
            dma: None,
        };
    }

    #[test]
    fn a_session_that_never_got_constraints_is_stale() {
        assert!(constraints_stale(None, &constraints(1920, 1080)));
    }

    #[test]
    fn unchanged_constraints_are_not_resent() {
        let same = constraints(1920, 1080);
        assert!(!constraints_stale(Some(&same), &constraints(1920, 1080)));
    }

    #[test]
    fn a_resized_output_makes_the_session_stale() {
        let old = constraints(1920, 1080);
        assert!(constraints_stale(Some(&old), &constraints(2560, 1600)));
    }
}
