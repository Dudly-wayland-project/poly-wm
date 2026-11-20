//! The readback half of a capture frame: reading the framebuffer back on the
//! GPU's clock instead of the event loop's.
//!
//! A capture frame is two operations that used to be one blocking call:
//!
//! 1. **Copy** the damaged regions out of the offscreen texture into a pixel
//!    pack buffer. `glReadPixels` only *starts* a DMA; the renderer may still be
//!    drawing into that texture.
//! 2. **Map** the pack buffer and copy the bytes into the client's shm pool.
//!    Mapping waits for the DMA to finish, so doing both back to back makes the
//!    event loop wait for a GPU operation it just queued.
//!
//! Splitting them lets the copy start on one iteration and the mapping happen on
//! a later one, once the GPU has caught up. A capture client that asks for the
//! next frame before its buffer is filled simply gets an answer later: the
//! protocol measures `ready` against the previous one and says nothing about how
//! soon it has to follow a `capture`.
//!
//! Nothing here is a fence of its own. The frame's [`SyncPoint`] is the one the
//! renderer already returns for the frame it just drew, so a readback that is
//! not reached is exactly a frame whose drawing has not landed yet.

use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::ExportMem;
use smithay::backend::renderer::gles::{GlesMapping, GlesRenderer, GlesTarget};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::utils::{Buffer, Rectangle};

/// The pixels of one region, already copied out of the framebuffer.
///
/// Held between the copy and the mapping so that the two can happen on different
/// event loop iterations.
pub struct ReadbackRegion {
    /// Where the pixels go in the client's buffer.
    pub dst: Rectangle<i32, Buffer>,
    /// The GPU-side copy of the framebuffer region.
    pub mapping: GlesMapping,
}

/// One frame's worth of copied-out regions, waiting to be written to shm.
pub struct PendingReadback {
    /// What was copied out, in the order the regions were requested.
    pub regions: Vec<ReadbackRegion>,
    /// The frame this readback belongs to.
    ///
    /// The copy is only valid once the drawing that produced it has finished,
    /// which is what this sync point reports.
    pub sync: SyncPoint,
}

impl PendingReadback {
    /// Whether the GPU is done with the copies.
    ///
    /// `false` means "try again next iteration"; the regions stay queued and the
    /// client's buffer is untouched, so nothing is lost by waiting.
    pub fn is_ready(&self) -> bool {
        return self.sync.is_reached();
    }
}

/// Copy `regions` of `target` into pixel pack buffers, ready to be mapped.
///
/// `origin` shifts the source rectangles, exactly like the synchronous path: a
/// session captures the whole output into a same-sized buffer, while a region
/// capture gets a buffer only as large as the region.
pub fn copy_regions(
    renderer: &mut GlesRenderer,
    target: &GlesTarget<'_>,
    origin: smithay::utils::Point<i32, Buffer>,
    regions: &[Rectangle<i32, Buffer>],
    frame: smithay::utils::Size<i32, Buffer>,
    format: Fourcc,
) -> anyhow::Result<Vec<ReadbackRegion>> {
    let mut copied = Vec::with_capacity(regions.len());
    for region in regions {
        let src = super::source_rect(*region, origin);
        anyhow::ensure!(
            src.loc.x >= 0
                && src.loc.y >= 0
                && src.loc.x + src.size.w <= frame.w
                && src.loc.y + src.size.h <= frame.h,
            "capture source lies outside the framebuffer"
        );
        let mapping = renderer.copy_framebuffer(target, src, format)?;
        copied.push(ReadbackRegion {
            dst: *region,
            mapping,
        });
    }
    return Ok(copied);
}

/// Write copied-out regions into the client's shm buffer.
///
/// This is the part that has to wait for the GPU, so it is only called once
/// [`PendingReadback::is_ready`] says the copies landed.
pub fn write_regions(
    renderer: &mut GlesRenderer,
    readback: PendingReadback,
    buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    size: smithay::utils::Size<i32, Buffer>,
    shm_format: smithay::reexports::wayland_server::protocol::wl_shm::Format,
) -> anyhow::Result<()> {
    let row_len = usize::try_from(size.w)? * 4;
    let height = usize::try_from(size.h)?;
    // One `with_buffer_contents_mut` for all regions: it locks the pool, so
    // calling it once per region would be needlessly slow.
    smithay::wayland::shm::with_buffer_contents_mut(buffer, |ptr, pool_len, data| {
        anyhow::ensure!(
            data.format == shm_format && data.width == size.w && data.height == size.h,
            "capture buffer does not match the announced constraints"
        );
        let offset = usize::try_from(data.offset)?;
        let stride = usize::try_from(data.stride)?;
        anyhow::ensure!(
            stride >= row_len && offset + stride * (height - 1) + row_len <= pool_len,
            "capture buffer does not fit in its shm pool"
        );
        // SAFETY: `ptr` is valid for `pool_len` bytes (the client's pool) and the
        // bounds checks below keep every write inside it.
        let dst = unsafe { ptr.add(offset) };
        for region in &readback.regions {
            let x = usize::try_from(region.dst.loc.x)?;
            let y0 = usize::try_from(region.dst.loc.y)?;
            let region_row = usize::try_from(region.dst.size.w)? * 4;
            let region_height = usize::try_from(region.dst.size.h)?;
            // `x` is a pixel and the offsets below are bytes, so scale it here
            // once: getting this wrong shifts the whole region horizontally.
            let byte_x = x * 4;
            anyhow::ensure!(
                byte_x + region_row <= row_len && y0 + region_height <= height,
                "capture damage lies outside the buffer"
            );
            let pixels = renderer.map_texture(&region.mapping)?;
            anyhow::ensure!(
                pixels.len() >= region_row * region_height,
                "short framebuffer readback for the capture"
            );
            for y in 0..region_height {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(y * region_row),
                        dst.add(super::pixel_offset(stride, x, y0 + y)),
                        region_row,
                    );
                }
            }
        }
        return Ok::<(), anyhow::Error>(());
    })
    .map_err(|err| anyhow::anyhow!("could not access the capture buffer: {err}"))
    .context("writing a capture frame")??;
    return Ok(());
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Point, Size};

    /// The bounds check [`copy_regions`] performs, isolated so it can be tested
    /// without a GL context: a region that lies outside the framebuffer must be
    /// rejected before `glReadPixels` is asked for pixels that are not there.
    fn source_fits(
        region: smithay::utils::Rectangle<i32, smithay::utils::Buffer>,
        origin: Point<i32, smithay::utils::Buffer>,
        frame: Size<i32, smithay::utils::Buffer>,
    ) -> bool {
        let src = super::super::source_rect(region, origin);
        return src.loc.x >= 0
            && src.loc.y >= 0
            && src.loc.x + src.size.w <= frame.w
            && src.loc.y + src.size.h <= frame.h;
    }

    fn rect(
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> smithay::utils::Rectangle<i32, smithay::utils::Buffer> {
        return smithay::utils::Rectangle::new(Point::from((x, y)), Size::from((w, h)));
    }

    #[test]
    fn a_region_inside_the_frame_is_copied() {
        let frame = Size::from((1920, 1080));
        assert!(source_fits(
            rect(0, 0, 1920, 1080),
            Point::from((0, 0)),
            frame
        ));
        assert!(source_fits(
            rect(1900, 1000, 20, 80),
            Point::from((0, 0)),
            frame
        ));
    }

    #[test]
    fn a_region_past_the_frame_is_rejected() {
        let frame = Size::from((1920, 1080));
        assert!(!source_fits(
            rect(1900, 0, 40, 10),
            Point::from((0, 0)),
            frame
        ));
        assert!(!source_fits(
            rect(0, 1070, 10, 20),
            Point::from((0, 0)),
            frame
        ));
    }

    #[test]
    fn the_origin_shifts_the_source_into_the_frame() {
        // A region capture's buffer is only as large as the region, so its own
        // coordinates say nothing; the origin is what places it in the output.
        let frame = Size::from((1920, 1080));
        assert!(source_fits(
            rect(0, 0, 100, 100),
            Point::from((100, 50)),
            frame
        ));
        assert!(!source_fits(
            rect(1900, 0, 100, 100),
            Point::from((100, 50)),
            frame
        ));
    }
}
