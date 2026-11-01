// Ported from niri (GPL-3.0-or-later): src/ui/config_error_notification.rs

//! Compositor-drawn Toasts.
//!
//! Configuration, layout and shader errors must be visible without a second
//! monitor showing a log, so they are rasterized into a memory buffer and drawn
//! on top of everything, centered at the top of each output. Identical messages
//! are deduplicated and expire after a few seconds.
//!
//! Text is laid out in logical pixels and rasterized once per output scale
//! (`ab_glyph` at `FONT_SIZE × scale`), so a HiDPI output gets crisp glyphs
//! rather than an upscaled image. A Toast slides in from the top edge when it
//! appears and slides out again before it expires; both animations are driven
//! by the layout tick (`LayoutManager::tick`), which is what keeps frames
//! coming while they run.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ab_glyph::{Font, FontArc, FontVec, PxScale, ScaleFont};
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::solid::SolidColorBuffer;
use smithay::reexports::gbm::Format as Fourcc;
use smithay::utils::{Buffer, Logical, Size, Transform};
use tracing::warn;

/// Font size in logical pixels.
const FONT_SIZE: f32 = 14.0;
/// Padding around the text.
const PADDING: f32 = 10.0;
/// Maximum text width before wrapping.
const MAX_WIDTH: f32 = 560.0;
/// Distance from the top of the output.
const MARGIN: i32 = 16;
/// Gap between two Toasts.
const SPACING: i32 = 6;
/// Toasts kept on screen at once; older ones are dropped.
const MAX_TOASTS: usize = 5;
/// How long an error stays on screen.
const DURATION: Duration = Duration::from_secs(8);
/// How long the slide in and the slide out take.
const SLIDE: Duration = Duration::from_millis(180);

/// Font candidates, in preference order. The first one that loads wins;
/// `POLY_TOAST_FONT` overrides the list.
const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
];

/// One Toast: rasterized text plus its expiry.
struct Toast {
    /// The message (used for deduplication).
    text: String,
    /// When the slide-in started; also the start of the lifetime.
    started: Instant,
    /// When the Toast disappears.
    deadline: Instant,
    /// The text laid out in logical pixels, shared by every rasterization.
    layout: Layout,
    /// The dark plate behind the text.
    background: SolidColorBuffer,
    /// Logical size of the buffer.
    size: Size<i32, Logical>,
    /// Rasterizations, one per output scale (`scale` → buffer). Filled on
    /// demand: the scales of the outputs that are actually attached.
    rasters: RefCell<HashMap<i32, MemoryRenderBuffer>>,
}

impl Toast {
    /// The buffer for this Toast at `scale`, rasterizing it on first use.
    fn buffer(&self, font: &FontArc, scale: i32) -> Option<MemoryRenderBuffer> {
        if let Some(buffer) = self.rasters.borrow().get(&scale) {
            return Some(buffer.clone());
        }
        let buffer = rasterize(font, &self.layout, scale)?;
        self.rasters.borrow_mut().insert(scale, buffer.clone());
        return Some(buffer);
    }

    /// How far above its resting place the Toast is right now, in logical
    /// pixels (0 when it sits still).
    fn slide_offset(&self, now: Instant) -> i32 {
        // A Toast that has slid fully out is off the top edge.
        let distance = (self.size.h + MARGIN) as f32;
        let entering = now.saturating_duration_since(self.started);
        if entering < SLIDE {
            let progress = ease_out(entering.as_secs_f32() / SLIDE.as_secs_f32());
            return -(distance * (1.0 - progress)).round() as i32;
        }
        let remaining = self.deadline.saturating_duration_since(now);
        if remaining < SLIDE {
            let progress = ease_out(1.0 - remaining.as_secs_f32() / SLIDE.as_secs_f32());
            return -(distance * progress).round() as i32;
        }
        return 0;
    }

    /// Whether this Toast is sliding, i.e. whether it needs frames.
    fn is_sliding(&self, now: Instant) -> bool {
        return now.saturating_duration_since(self.started) < SLIDE
            || self.deadline.saturating_duration_since(now) < SLIDE;
    }
}

/// Cubic ease-out: fast at the start, gentle at the end.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let inverse = 1.0 - t;
    return 1.0 - inverse * inverse * inverse;
}

/// The wrapped text of one Toast, laid out at scale 1 (logical pixels).
///
/// Laying out once and rasterizing per scale keeps the line breaks identical
/// on a HiDPI output and on a normal one.
struct Layout {
    /// The wrapped lines.
    lines: Vec<String>,
    /// Distance between two baselines.
    line_height: f32,
    /// Font ascent: how far the baseline sits below the top of a line box.
    ascent: f32,
    /// Logical size of the plate, padding included.
    size: Size<i32, Logical>,
}

/// The background color of a Toast (straight alpha; premultiplied when used).
const BACKGROUND: [f32; 4] = [0.11, 0.11, 0.13, 0.95];

/// The Toast stack of the compositor.
pub struct Toasts {
    items: Vec<Toast>,
    font: Option<FontArc>,
}

impl Default for Toasts {
    fn default() -> Self {
        return Self::new();
    }
}

impl Toasts {
    /// Load the font once; a missing font only loses the text, not the
    /// compositor.
    pub fn new() -> Self {
        let font = load_font();
        if font.is_none() {
            warn!(
                "no usable font found for Toasts; set POLY_TOAST_FONT to a .ttf/.otf file \
                 to see configuration errors on screen"
            );
        }
        return Self {
            items: Vec::new(),
            font,
        };
    }

    /// Whether there is anything to draw.
    pub fn is_empty(&self) -> bool {
        return self.items.is_empty();
    }

    /// Show `text`, deduplicating against the Toasts already on screen.
    ///
    /// A duplicate refreshes the existing Toast instead of stacking another
    /// copy, so a shader that fails to compile on every frame cannot flood the
    /// screen.
    pub fn push(&mut self, text: &str, now: Instant) {
        if let Some(existing) = self.items.iter_mut().find(|item| return item.text == text) {
            existing.deadline = now + DURATION;
            return;
        }

        let Some(font) = self.font.clone() else {
            return;
        };
        let Some(layout) = layout_text(&font, text) else {
            return;
        };

        if self.items.len() >= MAX_TOASTS {
            self.items.remove(0);
        }
        self.items.push(Toast {
            text: text.to_owned(),
            started: now,
            deadline: now + DURATION,
            background: SolidColorBuffer::new(layout.size, BACKGROUND),
            size: layout.size,
            layout,
            rasters: RefCell::new(HashMap::new()),
        });
    }

    /// Drop expired Toasts; returns whether anything was removed.
    pub fn remove_expired(&mut self, now: Instant) -> bool {
        let before = self.items.len();
        self.items.retain(|item| return item.deadline > now);
        return self.items.len() != before;
    }

    /// Whether any Toast is sliding, so the caller keeps frames coming.
    pub fn animating(&self, now: Instant) -> bool {
        return self.items.iter().any(|item| return item.is_sliding(now));
    }

    /// Where to draw every Toast on an output of `output_size` logical pixels.
    ///
    /// The list is top to bottom; the caller draws the background first and the
    /// text on top of it. `scale` is the output scale: the text is rasterized
    /// at that integer scale, so it never has to be upscaled by the renderer.
    pub fn placements(&self, output_size: Size<i32, Logical>, scale: f64) -> Vec<ToastPlacement> {
        let Some(font) = self.font.as_ref() else {
            return Vec::new();
        };
        let raster_scale = (scale.round() as i32).max(1);
        let now = Instant::now();
        let mut result = Vec::with_capacity(self.items.len());
        let mut y = MARGIN;
        for item in &self.items {
            let width = item.size.w;
            let height = item.size.h;
            if width <= 0 || height <= 0 {
                continue;
            }
            let Some(buffer) = item.buffer(font, raster_scale) else {
                continue;
            };
            let x = (output_size.w - width) / 2;
            // The slide only moves the Toast being drawn; the ones below keep
            // their resting place.
            let rect = smithay::utils::Rectangle::<i32, Logical>::new(
                (x, y + item.slide_offset(now)).into(),
                (width, height).into(),
            );
            result.push(ToastPlacement {
                buffer,
                background: item.background.clone(),
                rect: rect.to_physical_precise_round(scale),
                size: item.size,
            });
            y += height + SPACING;
        }
        return result;
    }
}

/// One Toast ready to be turned into render elements.
///
/// The buffers are owned (they are reference-counted inside smithay) because
/// the rasterization cache lives behind a `RefCell`.
pub struct ToastPlacement {
    /// The text buffer.
    pub buffer: MemoryRenderBuffer,
    /// The plate behind the text, drawn first (i.e. below it).
    pub background: SolidColorBuffer,
    /// Output-local physical rectangle.
    pub rect: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
    /// Logical size of the buffer, for the render element.
    pub size: Size<i32, Logical>,
}

fn load_font() -> Option<FontArc> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(path) = std::env::var("POLY_TOAST_FONT") {
        candidates.push(PathBuf::from(path));
    }
    candidates.extend(
        FONT_CANDIDATES
            .iter()
            .map(|path| return PathBuf::from(path)),
    );

    for path in candidates {
        if let Some(font) = load_font_file(&path) {
            return Some(font);
        }
    }
    return None;
}

fn load_font_file(path: &Path) -> Option<FontArc> {
    let data = std::fs::read(path).ok()?;
    // `.ttc` files are collections; the first face is the regular one.
    let font = if path
        .extension()
        .is_some_and(|extension| return extension.eq_ignore_ascii_case("ttc"))
    {
        FontVec::try_from_vec_and_index(data, 0)
            .ok()
            .map(FontArc::new)
    } else {
        FontArc::try_from_vec(data).ok()
    };
    return font;
}

/// Wrap and measure `text` in logical pixels.
fn layout_text(font: &FontArc, text: &str) -> Option<Layout> {
    let scale = PxScale::from(FONT_SIZE);
    let scaled = font.as_scaled(scale);
    let lines = wrap(font, scale, text, MAX_WIDTH - 2.0 * PADDING);
    if lines.is_empty() {
        return None;
    }

    let mut text_width: f32 = 0.0;
    for line in &lines {
        text_width = text_width.max(line_width(&scaled, line));
    }

    let line_height = scaled.height() + scaled.line_gap();
    let width = (text_width + 2.0 * PADDING).ceil().max(1.0) as i32;
    let height = (line_height * lines.len() as f32 + 2.0 * PADDING)
        .ceil()
        .max(1.0) as i32;
    return Some(Layout {
        lines,
        line_height,
        ascent: scaled.ascent(),
        size: Size::from((width, height)),
    });
}

/// Rasterize `layout` at `scale`, as white premultiplied glyphs on a
/// transparent background.
///
/// The layout is computed once in logical pixels and shared by every scale;
/// only the glyph rasterization runs at `scale × FONT_SIZE`.
fn rasterize(font: &FontArc, layout: &Layout, scale: i32) -> Option<MemoryRenderBuffer> {
    let scale = scale.max(1);
    let factor = scale as f32;
    let px_scale = PxScale::from(FONT_SIZE * factor);
    let scaled = font.as_scaled(px_scale);

    // Logical size × scale: the glyph metrics scale linearly, so a line that
    // fits at scale 1 fits at every scale and nothing is clipped.
    let width = layout.size.w * scale;
    let height = layout.size.h * scale;
    let mut buffer = MemoryRenderBuffer::new(
        Fourcc::Abgr8888,
        (width, height),
        scale,
        Transform::Normal,
        None,
    );
    let stride = width as usize * 4;
    let mut pixels = vec![0u8; stride * height as usize];

    for (index, line) in layout.lines.iter().enumerate() {
        let baseline = (PADDING + layout.ascent + index as f32 * layout.line_height) * factor;
        let mut caret = PADDING * factor;
        let mut previous = None;
        for ch in line.chars() {
            let id = scaled.glyph_id(ch);
            if let Some(previous) = previous {
                caret += scaled.kern(previous, id);
            }
            previous = Some(id);

            let glyph = id.with_scale_and_position(px_scale, ab_glyph::point(caret, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let x = bounds.min.x as i32 + gx as i32;
                    let y = bounds.min.y as i32 + gy as i32;
                    if x < 0 || y < 0 || x >= width || y >= height {
                        return;
                    }
                    let offset = y as usize * stride + x as usize * 4;
                    let src = coverage.clamp(0.0, 1.0);
                    // White premultiplied over white premultiplied: every
                    // channel holds the accumulated alpha.
                    let dst = f32::from(pixels[offset]) / 255.0;
                    let alpha = ((src + dst * (1.0 - src)) * 255.0) as u8;
                    pixels[offset] = alpha;
                    pixels[offset + 1] = alpha;
                    pixels[offset + 2] = alpha;
                    pixels[offset + 3] = alpha;
                });
            }
            caret += scaled.h_advance(id);
        }
    }

    {
        let mut context = buffer.render();
        context
            .draw(
                |memory| -> Result<Vec<smithay::utils::Rectangle<i32, Buffer>>, ()> {
                    let len = pixels.len().min(memory.len());
                    memory[..len].copy_from_slice(&pixels[..len]);
                    return Ok(vec![smithay::utils::Rectangle::<i32, Buffer>::from_size(
                        (width, height).into(),
                    )]);
                },
            )
            .ok()?;
    }

    return Some(buffer);
}

fn line_width<F: Font>(scaled: &ab_glyph::PxScaleFont<&F>, line: &str) -> f32 {
    let mut width = 0.0;
    let mut previous = None;
    for ch in line.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(previous) = previous {
            width += scaled.kern(previous, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    return width;
}

/// Greedy word wrap; a word that does not fit on its own is broken by
/// character (CJK text has no spaces to break at).
fn wrap<F: Font>(font: &F, scale: PxScale, text: &str, max_width: f32) -> Vec<String> {
    let scaled = font.as_scaled(scale);
    let mut lines = Vec::new();

    for paragraph in text.split('\n') {
        let paragraph = paragraph.trim_end();
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }

        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if line_width(&scaled, &candidate) <= max_width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            // The word alone may still be too long: break it by character.
            let mut current = String::new();
            for ch in word.chars() {
                let mut candidate = current.clone();
                candidate.push(ch);
                if !current.is_empty() && line_width(&scaled, &candidate) > max_width {
                    lines.push(std::mem::take(&mut current));
                }
                current.push(ch);
            }
            line = current;
        }
        lines.push(line);
    }

    return lines;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_long_lines() {
        let font = match load_font() {
            Some(font) => font,
            None => return,
        };
        let lines = wrap(
            &font,
            PxScale::from(FONT_SIZE),
            "the quick brown fox jumps over the lazy dog and keeps running far away",
            80.0,
        );
        assert!(lines.len() > 1);
        for line in &lines {
            let scaled = font.as_scaled(PxScale::from(FONT_SIZE));
            assert!(line_width(&scaled, line) <= 80.5);
        }
    }

    #[test]
    fn duplicate_toasts_are_merged() {
        let mut toasts = Toasts::new();
        if toasts.font.is_none() {
            return;
        }
        let now = Instant::now();
        toasts.push("same error", now);
        toasts.push("same error", now);
        assert_eq!(toasts.items.len(), 1);
    }

    #[test]
    fn expired_toasts_are_removed() {
        let mut toasts = Toasts::new();
        if toasts.font.is_none() {
            return;
        }
        let now = Instant::now();
        toasts.push("error", now);
        assert!(!toasts.remove_expired(now));
        assert!(toasts.remove_expired(now + DURATION + Duration::from_secs(1)));
        assert!(toasts.is_empty());
    }

    #[test]
    fn one_toast_per_output_is_listed() {
        let mut toasts = Toasts::new();
        if toasts.font.is_none() {
            return;
        }
        let now = Instant::now();
        toasts.push("first", now);
        toasts.push("second", now);
        let placements = toasts.placements(Size::from((800, 600)), 1.0);
        assert_eq!(placements.len(), 2);
        assert!(
            placements[0].rect.loc.y < placements[1].rect.loc.y,
            "toasts stack downwards"
        );
        for placement in placements {
            assert!(placement.rect.loc.x >= 0);
            assert!(placement.rect.loc.x + placement.rect.size.w <= 800);
        }
    }
}
