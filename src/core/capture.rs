//! Per-session state for `ext-image-copy-capture-v1` (`docs/plan.md` §15, M8).
//!
//! A capture client that keeps a session open (screen recording, a mirror, a
//! portal screencast) asks for one frame at a time, but the frames of a session
//! are all pictures of the same output at successive points in time. That makes
//! it worth keeping state across frames:
//!
//! - one offscreen [`GlesTexture`] per session, so a frame does not allocate (and
//!   throw away) a full-screen texture every time;
//! - one [`OutputDamageTracker`] per session, whose state lets a frame redraw
//!   only what changed instead of the whole output.
//!
//! Drawing and copying are two different questions, and a session answers them
//! separately:
//!
//! - *What has to be drawn?* Only what changed since the last drawn frame. The
//!   texture survives between frames and every frame repaints exactly the damage
//!   the tracker reports, so the texture is never more than one frame behind;
//!   feeding the tracker `age = 1` says exactly that and keeps it from replaying
//!   older damage.
//! - *What has to be copied into the client's buffer?* Everything that changed
//!   since *that buffer* was last written. shm clients rotate through two or
//!   three buffers while reading the *whole* buffer every time (`grim`,
//!   `wf-recorder`, OBS all do), so every buffer has to end up holding the
//!   complete current picture. `pending` keeps that per-buffer backlog.
//!
//! The backlog cannot be left to the tracker's own age bookkeeping: with
//! rotating buffers the age per frame is 3, 2, 1, 3, … and
//! `damage_output_internal` truncates its `old_damage` history to `age`
//! (`backend/renderer/damage/mod.rs`), so every age-1 frame throws away the
//! history an age-3 frame needs and that frame falls back to repainting the whole
//! output. Feeding it one fixed age and accumulating the damage per buffer avoids
//! that entirely and draws strictly less.
//!
//! A frame that finds nothing to draw *and* nothing to copy is
//! [`CaptureOutcome::Unchanged`]: only the damage events are skipped, the buffer
//! still holds the current picture. A buffer the session never wrote gets a full
//! readback, which is what makes a brand new buffer complete.

use std::collections::HashMap;

use anyhow::Context;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Element;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTarget, GlesTexture};
use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::shm;
use tracing::trace;

use crate::core::render::OutputElements;

/// Distinct buffers a session may rotate through before we stop tracking them
/// individually. Real clients use two or three; a client that keeps allocating
/// new ones would otherwise grow the map without bound, and every unknown buffer
/// costs a full readback anyway.
const MAX_TRACKED_BUFFERS: usize = 8;

/// More damage rectangles than this are read back as one bounding box: the cost
/// of a readback is dominated by the `glReadPixels` call itself (it stalls the
/// pipeline), not only by the number of pixels.
const MAX_READBACK_REGIONS: usize = 16;

/// Identifies a live capture session's state.
///
/// The protocol objects a session is made of are Smithay's, and `SessionRef` has
/// no public object id to key a map by, so a ticket is stored in the session's
/// user data instead. GL types cannot go there at all (user data requires
/// `Send + Sync` for the map to be shareable), which is why the state itself
/// lives in [`CaptureState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CaptureKey(u64);

/// The capture sessions of the compositor.
#[derive(Default)]
pub struct CaptureState {
    sessions: HashMap<CaptureKey, SessionCapture>,
    next_key: u64,
}

impl CaptureState {
    pub fn new() -> Self {
        return Self::default();
    }

    /// Reserve a ticket for a new session. The session's GL state is created
    /// lazily, on the first frame: only then is the output's mode known.
    pub fn create(&mut self) -> CaptureKey {
        let key = CaptureKey(self.next_key);
        self.next_key += 1;
        return key;
    }

    /// Drop the state of a session the client destroyed.
    pub fn remove(&mut self, key: CaptureKey) {
        self.sessions.remove(&key);
        return;
    }

    /// The state of `key`, (re)created when the output changed size or scale.
    pub fn session(
        &mut self,
        key: CaptureKey,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
    ) -> anyhow::Result<&mut SessionCapture> {
        let stale = match self.sessions.get(&key) {
            Some(session) => session.size != size || session.scale != scale,
            None => true,
        };
        if stale {
            // Texture and tracker have to be replaced together: a fresh tracker
            // reports a full first frame, which is what repaints the blank
            // texture.
            self.sessions
                .insert(key, SessionCapture::new(renderer, size, scale)?);
        }
        return self
            .sessions
            .get_mut(&key)
            .context("the capture session vanished right after it was created");
    }
}

/// What a frame did to the client's buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// The whole buffer (of this size) was written.
    Full(Size<i32, Buffer>),
    /// Only these buffer regions were written.
    Damage(Vec<Rectangle<i32, Buffer>>),
    /// Nothing changed and the buffer is still current.
    Unchanged,
}

impl CaptureOutcome {
    /// The damage to hand to `Frame::success`.
    ///
    /// The protocol measures damage against the *previous* `ready` event of the
    /// session, and a client that accumulates frames needs to be told about
    /// every difference in the buffer it got — so a full frame reports the whole
    /// buffer (`Some(vec![])` would mean "nothing changed", which is only true
    /// for `Unchanged`). Reporting more than we wrote is always safe.
    pub fn frame_damage(&self) -> Vec<Rectangle<i32, Buffer>> {
        return match self {
            Self::Full(size) => vec![Rectangle::from_size(*size)],
            Self::Damage(regions) => regions.clone(),
            Self::Unchanged => Vec::new(),
        };
    }
}

/// The persistent state of one capture session.
pub struct SessionCapture {
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    tracker: OutputDamageTracker,
    texture: GlesTexture,
    /// `wl_buffer` → the damage that piled up since we last wrote it, i.e. the
    /// part of the picture that buffer is still missing. A buffer that is not in
    /// here has never been written and needs everything.
    pending: HashMap<ObjectId, Vec<Rectangle<i32, Physical>>>,
}

impl SessionCapture {
    fn new(
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
    ) -> anyhow::Result<Self> {
        let buffer_size: Size<i32, Buffer> = Size::from((size.w, size.h));
        let texture = renderer.create_buffer(Fourcc::Xrgb8888, buffer_size)?;
        return Ok(Self {
            size,
            scale,
            // `Transform::Normal` for the same reason the frame is reported as
            // normal: the backend's own transform is not the client's business
            // (`handlers/image_copy_capture.rs`). Rotated outputs are not
            // supported (`docs/plan.md` §15).
            tracker: OutputDamageTracker::new(size, scale, Transform::Normal),
            texture,
            pending: HashMap::new(),
        });
    }

    /// Draw the current scene and copy the result into `buffer`.
    pub fn capture_frame(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: &[OutputElements<GlesRenderer>],
        background: [f32; 4],
        buffer: &WlBuffer,
    ) -> anyhow::Result<CaptureOutcome> {
        let buffer_size: Size<i32, Buffer> = Size::from((self.size.w, self.size.h));
        let id = buffer.id();
        let known = self.pending.contains_key(&id);
        if tracing::enabled!(tracing::Level::TRACE) {
            let summary: Vec<_> = elements
                .iter()
                .map(|element| return (format!("{:?}", element.id()), element.geometry(self.scale)))
                .collect();
            trace!(?summary, "capture elements");
        }
        let mut target = renderer.bind(&mut self.texture)?;
        // `render_output` draws into the bound framebuffer, which keeps the
        // previous picture, so the damage it reports is exactly what it
        // repainted. `age = 1`: the texture is at most one drawn frame behind
        // (see the module docs), so no older damage may be replayed. A fresh
        // tracker has no history at all and reports a full first frame, which is
        // what repaints the blank texture.
        let fresh = {
            let result =
                self.tracker
                    .render_output(renderer, &mut target, 1, elements, background)?;
            result
                .damage
                .map(|damage| return damage.to_vec())
                .unwrap_or_default()
        };

        // What this buffer is missing: its own backlog plus this frame's changes.
        let mut needed = match self.pending.get(&id) {
            Some(pending) => pending.clone(),
            None => Vec::new(),
        };
        needed.extend(fresh.iter().copied());
        if known && needed.is_empty() {
            // Nothing was drawn and the buffer already holds the picture: no
            // readback, no copy. `Ready` without damage says exactly that.
            trace!(?id, "capture frame unchanged");
            return Ok(CaptureOutcome::Unchanged);
        }

        let plan = if known {
            plan_readback(&needed, self.size)
        } else {
            // Never written: every pixel of it is stale.
            ReadbackPlan::Full
        };
        trace!(
            ?id,
            known,
            fresh = fresh.len(),
            needed = needed.len(),
            fresh_area = fresh.iter().map(|rect| return area(*rect)).sum::<i64>(),
            needed_area = needed.iter().map(|rect| return area(*rect)).sum::<i64>(),
            total = area(Rectangle::from_size(self.size)),
            regions = match &plan {
                ReadbackPlan::Full => 1,
                ReadbackPlan::Regions(regions) => regions.len(),
            },
            full = matches!(plan, ReadbackPlan::Full),
            "capture frame"
        );
        let regions = match &plan {
            ReadbackPlan::Full => vec![Rectangle::from_size(buffer_size)],
            ReadbackPlan::Regions(regions) => regions.clone(),
        };
        copy_into_shm(buffer, buffer_size, renderer, &target, &regions)?;

        // The new backlog: every other buffer is now missing this frame's
        // changes, and this one is missing nothing.
        if !known && self.pending.len() >= MAX_TRACKED_BUFFERS {
            // Forget everything: the buffers written next get a full readback,
            // which is correct, just slower. Without this a client that keeps
            // allocating buffers would grow the map without bound.
            self.pending.clear();
        }
        for (other, pending) in self.pending.iter_mut() {
            if *other == id {
                continue;
            }
            pending.extend(fresh.iter().copied());
            coalesce(pending);
        }
        self.pending.insert(id, Vec::new());

        return Ok(match plan {
            ReadbackPlan::Full => CaptureOutcome::Full(buffer_size),
            ReadbackPlan::Regions(regions) => CaptureOutcome::Damage(regions),
        });
    }
}

/// What of a freshly rendered frame has to be copied to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadbackPlan {
    /// The whole buffer.
    Full,
    /// Only these regions; the rest of the buffer is still current.
    Regions(Vec<Rectangle<i32, Buffer>>),
}

/// Turn the damage rectangle soup into a small number of readbacks.
///
/// Every extra readback costs a pipeline stall, and reading a region that did
/// not change is free of correctness problems (the pixels are current), so the
/// plan trades rectangles for area: rects inside other rects are dropped, and
/// once the damage covers half the output, or is scattered over many rects, it
/// is cheaper to read more pixels than to make more calls.
pub fn plan_readback(
    damage: &[Rectangle<i32, Physical>],
    size: Size<i32, Physical>,
) -> ReadbackPlan {
    let full = Rectangle::from_size(size);
    let full_area = area(full);
    if full_area <= 0 {
        return ReadbackPlan::Full;
    }
    let mut regions: Vec<Rectangle<i32, Physical>> = Vec::with_capacity(damage.len());
    for rect in damage {
        let Some(clipped) = rect.intersection(full) else {
            continue;
        };
        if area(clipped) <= 0 {
            continue;
        }
        // Drop rects an already kept rect covers, and rects that cover kept ones.
        if regions.iter().any(|kept| covers(*kept, clipped)) {
            continue;
        }
        regions.retain(|kept| !covers(clipped, *kept));
        regions.push(clipped);
    }
    if regions.is_empty() {
        return ReadbackPlan::Full;
    }
    let total: i64 = regions.iter().map(|rect| area(*rect)).sum();
    if total * 2 >= full_area {
        return ReadbackPlan::Full;
    }
    if regions.len() > MAX_READBACK_REGIONS {
        let bbox = bounding_box(&regions);
        if area(bbox) * 4 >= full_area * 3 {
            return ReadbackPlan::Full;
        }
        return ReadbackPlan::Regions(vec![to_buffer(bbox)]);
    }
    return ReadbackPlan::Regions(regions.iter().map(|rect| to_buffer(*rect)).collect());
}

/// Copy the given regions of the framebuffer into the client's shm buffer.
///
/// The regions are in buffer coordinates (the tracker is fed `Transform::Normal`,
/// so image space and GL space agree: scene `(0, 0)` is NDC `(-1, -1)`, which is
/// GL row 0, and `glReadPixels` returns rows in increasing GL order — the
/// top-down order the client wants). The client picks stride and offset, so only
/// the region's own `width * 4` bytes of each row can be written.
fn copy_into_shm(
    buffer: &WlBuffer,
    size: Size<i32, Buffer>,
    renderer: &mut GlesRenderer,
    target: &GlesTarget<'_>,
    regions: &[Rectangle<i32, Buffer>],
) -> anyhow::Result<()> {
    let row_len = usize::try_from(size.w)? * 4;
    let height = usize::try_from(size.h)?;
    // One `with_buffer_contents_mut` for all regions: it locks the pool, so
    // calling it once per region would be needlessly slow.
    shm::with_buffer_contents_mut(buffer, |ptr, pool_len, data| {
        anyhow::ensure!(
            data.format == wl_shm::Format::Xrgb8888
                && data.width == size.w
                && data.height == size.h,
            "capture buffer does not match the announced constraints"
        );
        let offset = usize::try_from(data.offset)?;
        let stride = usize::try_from(data.stride)?;
        anyhow::ensure!(
            stride >= row_len && offset + stride * (height - 1) + row_len <= pool_len,
            "capture buffer does not fit in its shm pool"
        );
        // SAFETY: `ptr` is valid for `pool_len` bytes (the client's pool) and
        // the bounds checks below keep every write inside it.
        let dst = unsafe { ptr.add(offset) };
        for region in regions {
            let x = usize::try_from(region.loc.x)?;
            let y0 = usize::try_from(region.loc.y)?;
            let region_row = usize::try_from(region.size.w)? * 4;
            let region_height = usize::try_from(region.size.h)?;
            // `x` is a pixel and the offsets below are bytes, so scale it here
            // once: getting this wrong shifts the whole region horizontally.
            let byte_x = x * 4;
            anyhow::ensure!(
                byte_x + region_row <= row_len && y0 + region_height <= height,
                "capture damage lies outside the buffer"
            );
            let mapping = renderer.copy_framebuffer(target, *region, Fourcc::Xrgb8888)?;
            let pixels = renderer.map_texture(&mapping)?;
            anyhow::ensure!(
                pixels.len() >= region_row * region_height,
                "short framebuffer readback for the capture"
            );
            for y in 0..region_height {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(y * region_row),
                        dst.add(pixel_offset(stride, x, y0 + y)),
                        region_row,
                    );
                }
            }
        }
        return Ok::<(), anyhow::Error>(());
    })
    .map_err(|err| anyhow::anyhow!("could not access the capture buffer: {err}"))??;
    return Ok(());
}

fn area(rect: Rectangle<i32, Physical>) -> i64 {
    return i64::from(rect.size.w) * i64::from(rect.size.h);
}

/// Byte offset of the pixel (`x`, `y`) inside a buffer with `stride` bytes per
/// row.
///
/// The readback plan carries *pixel* coordinates while every offset in
/// [`copy_into_shm`] is in *bytes*, so `x` has to be scaled here. Mixing the two
/// up shifts a whole region sideways by three quarters of its `x`, which a full
/// readback never shows because its `x` is zero — only partial readbacks do.
fn pixel_offset(stride: usize, x: usize, y: usize) -> usize {
    return y * stride + x * 4;
}

fn covers(outer: Rectangle<i32, Physical>, inner: Rectangle<i32, Physical>) -> bool {
    return outer.loc.x <= inner.loc.x
        && outer.loc.y <= inner.loc.y
        && outer.loc.x + outer.size.w >= inner.loc.x + inner.size.w
        && outer.loc.y + outer.size.h >= inner.loc.y + inner.size.h;
}

fn bounding_box(rects: &[Rectangle<i32, Physical>]) -> Rectangle<i32, Physical> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for rect in rects {
        min_x = min_x.min(rect.loc.x);
        min_y = min_y.min(rect.loc.y);
        max_x = max_x.max(rect.loc.x + rect.size.w);
        max_y = max_y.max(rect.loc.y + rect.size.h);
    }
    return Rectangle::new(
        Point::from((min_x, min_y)),
        Size::from((max_x - min_x, max_y - min_y)),
    );
}

/// `Physical` and `Buffer` coordinates are both plain pixels; only the marker
/// type changes.
fn to_buffer(rect: Rectangle<i32, Physical>) -> Rectangle<i32, Buffer> {
    return Rectangle::new(
        Point::from((rect.loc.x, rect.loc.y)),
        Size::from((rect.size.w, rect.size.h)),
    );
}

/// Keep a buffer's backlog bounded: past a few rectangles, one bounding box is
/// cheaper to read back, and reading more than the damage is always safe.
fn coalesce(pending: &mut Vec<Rectangle<i32, Physical>>) {
    if pending.len() <= MAX_READBACK_REGIONS {
        return;
    }
    let bbox = bounding_box(pending);
    pending.clear();
    pending.push(bbox);
    return;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        return Rectangle::new(Point::from((x, y)), Size::from((w, h)));
    }

    fn size(w: i32, h: i32) -> Size<i32, Physical> {
        return Size::from((w, h));
    }

    #[test]
    fn empty_damage_reads_back_everything() {
        assert_eq!(plan_readback(&[], size(100, 100)), ReadbackPlan::Full);
    }

    #[test]
    fn small_damage_is_read_back_as_is() {
        let plan = plan_readback(&[rect(10, 20, 30, 40)], size(1000, 1000));
        assert_eq!(
            plan,
            ReadbackPlan::Regions(vec![Rectangle::new(
                Point::from((10, 20)),
                Size::from((30, 40))
            )])
        );
    }

    #[test]
    fn contained_rectangles_are_dropped() {
        let plan = plan_readback(
            &[rect(0, 0, 100, 100), rect(10, 10, 20, 20)],
            size(1000, 1000),
        );
        assert_eq!(
            plan,
            ReadbackPlan::Regions(vec![Rectangle::new(
                Point::from((0, 0)),
                Size::from((100, 100))
            )])
        );
    }

    #[test]
    fn damage_outside_the_output_is_clipped_away() {
        let plan = plan_readback(&[rect(-50, -50, 20, 20)], size(100, 100));
        assert_eq!(plan, ReadbackPlan::Full);
        let plan = plan_readback(&[rect(90, 90, 50, 50)], size(100, 100));
        assert_eq!(
            plan,
            ReadbackPlan::Regions(vec![Rectangle::new(
                Point::from((90, 90)),
                Size::from((10, 10))
            )])
        );
    }

    #[test]
    fn covering_half_the_output_reads_back_everything() {
        let plan = plan_readback(&[rect(0, 0, 100, 50)], size(100, 100));
        assert_eq!(plan, ReadbackPlan::Full);
    }

    #[test]
    fn many_scattered_rectangles_collapse_into_a_bounding_box() {
        let damage: Vec<_> = (0..MAX_READBACK_REGIONS as i32 + 1)
            .map(|i| return rect(i * 2, i * 2, 1, 1))
            .collect();
        let plan = plan_readback(&damage, size(1000, 1000));
        assert_eq!(
            plan,
            ReadbackPlan::Regions(vec![Rectangle::new(
                Point::from((0, 0)),
                Size::from((
                    MAX_READBACK_REGIONS as i32 * 2 + 1,
                    MAX_READBACK_REGIONS as i32 * 2 + 1
                ))
            )])
        );
    }

    #[test]
    fn a_pixel_offset_counts_rows_in_bytes_and_columns_in_pixels() {
        // Regression: the column used to be added unscaled, so a partial
        // readback of a window at x = 510 landed at x = 127 in the client's
        // buffer (510 / 4). A full readback could never catch it, because its
        // x is always 0.
        assert_eq!(pixel_offset(5120, 510, 0), 2040);
        assert_eq!(pixel_offset(5120, 0, 2), 10240);
        assert_eq!(pixel_offset(5120, 1, 1), 5124);
    }

    #[test]
    fn a_buffers_backlog_is_collapsed_once_it_gets_big() {
        let mut pending: Vec<_> = (0..MAX_READBACK_REGIONS as i32 + 1)
            .map(|i| return rect(i * 2, i * 3, 1, 1))
            .collect();
        coalesce(&mut pending);
        assert_eq!(
            pending,
            vec![Rectangle::new(
                Point::from((0, 0)),
                Size::from((
                    MAX_READBACK_REGIONS as i32 * 2 + 1,
                    MAX_READBACK_REGIONS as i32 * 3 + 1
                ))
            )]
        );

        let mut small = vec![rect(1, 2, 3, 4)];
        coalesce(&mut small);
        assert_eq!(small, vec![rect(1, 2, 3, 4)]);
    }
}
