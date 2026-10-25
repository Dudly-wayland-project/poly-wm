// Ported from anvil (GPL-3.0-or-later): anvil/src/cursor.rs, anvil/src/drawing.rs

//! Software cursor: xcursor theme loading plus the render element that draws
//! either a themed icon or a client-provided cursor surface.

use std::io::Read;
use std::time::Duration;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Kind, render_elements};
use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus};
use smithay::utils::{IsAlive, Logical, Physical, Point, Scale, Transform};
use smithay::wayland::compositor::with_states;
use tracing::warn;
use xcursor::CursorTheme;
use xcursor::parser::{Image, parse_xcursor};

static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../../resources/cursor.rgba");

/// A loaded xcursor theme.
pub struct Cursor {
    icons: Vec<Image>,
    size: u32,
}

impl Cursor {
    /// Load `$XCURSOR_THEME` (falling back to the built-in cursor).
    pub fn load() -> Cursor {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| return "default".to_owned());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|size| return size.parse().ok())
            .unwrap_or(24);

        let theme = CursorTheme::load(&name);
        let icons = load_icon(&theme).unwrap_or_else(|err| {
            warn!("unable to load xcursor theme: {err:?}, using fallback cursor");
            return vec![fallback_image()];
        });

        return Cursor { icons, size };
    }

    /// Grab the frame of the cursor animation to draw at `time`.
    pub fn get_image(&self, scale: u32, time: Duration) -> Image {
        let size = self.size * scale;
        return frame(time.as_millis() as u32, size, &self.icons);
    }
}

fn fallback_image() -> Image {
    return Image {
        size: 32,
        width: 64,
        height: 64,
        xhot: 1,
        yhot: 1,
        delay: 1,
        pixels_rgba: Vec::from(FALLBACK_CURSOR_DATA),
        pixels_argb: Vec::new(),
    };
}

fn nearest_images(size: u32, images: &[Image]) -> impl Iterator<Item = &Image> {
    // Follow the nominal size of the cursor to choose the nearest.
    let nearest_image = images
        .iter()
        .min_by_key(|image| return (size as i32 - image.size as i32).abs())
        .unwrap_or_else(|| return &images[0]);

    return images
        .iter()
        .filter(move |image| return image.width == nearest_image.width)
        .filter(move |image| return image.height == nearest_image.height);
}

fn frame(mut millis: u32, size: u32, images: &[Image]) -> Image {
    let total = nearest_images(size, images).fold(0, |acc, image| return acc + image.delay);
    if total == 0 {
        return nearest_images(size, images)
            .next()
            .cloned()
            .unwrap_or_else(fallback_image);
    }
    millis %= total;

    for img in nearest_images(size, images) {
        if millis < img.delay {
            return img.clone();
        }
        millis -= img.delay;
    }

    return nearest_images(size, images)
        .next()
        .cloned()
        .unwrap_or_else(fallback_image);
}

#[derive(Debug, thiserror::Error)]
enum LoadError {
    #[error("theme has no default cursor")]
    NoDefaultCursor,
    #[error("error reading cursor file: {0}")]
    File(#[from] std::io::Error),
    #[error("failed to parse xcursor file")]
    Parse,
}

fn load_icon(theme: &CursorTheme) -> Result<Vec<Image>, LoadError> {
    let icon_path = theme
        .load_icon("default")
        .ok_or(LoadError::NoDefaultCursor)?;
    let mut cursor_file = std::fs::File::open(icon_path)?;
    let mut cursor_data = Vec::new();
    cursor_file.read_to_end(&mut cursor_data)?;
    return parse_xcursor(&cursor_data).ok_or(LoadError::Parse);
}

use crate::core::render::PolyRenderer;
use smithay::backend::renderer::RendererSuper;

render_elements! {
    /// One piece of the software cursor.
    pub CursorRenderElement<R>
    where
        R: PolyRenderer,
        <R as RendererSuper>::TextureId: Clone + 'static;
    Surface=WaylandSurfaceRenderElement<R>,
    Memory=MemoryRenderBufferRenderElement<R>,
}

/// The cursor as the seat wants it drawn.
#[derive(Debug)]
pub struct CursorElement {
    buffer: Option<MemoryRenderBuffer>,
    status: CursorImageStatus,
}

impl Default for CursorElement {
    fn default() -> Self {
        return Self {
            buffer: None,
            status: CursorImageStatus::default_named(),
        };
    }
}

impl CursorElement {
    /// Use this buffer for named cursors.
    pub fn set_buffer(&mut self, buffer: MemoryRenderBuffer) {
        self.buffer = Some(buffer);
    }

    /// Update which cursor the client (or we) want drawn.
    pub fn set_status(&mut self, status: CursorImageStatus) {
        self.status = status;
    }

    /// Build the render elements for this cursor at `location`.
    pub fn elements<R>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
    ) -> Vec<CursorRenderElement<R>>
    where
        R: PolyRenderer,
        <R as RendererSuper>::TextureId: Clone + Send + 'static,
    {
        let mut elements = Vec::new();
        match &self.status {
            CursorImageStatus::Hidden => (),
            // Named cursors are always drawn from our own theme image.
            CursorImageStatus::Named(_) => {
                let Some(buffer) = self.buffer.as_ref() else {
                    return elements;
                };
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location.to_f64(),
                    buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(element) => elements.push(CursorRenderElement::Memory(element)),
                    Err(err) => warn!("error building cursor element: {err:?}"),
                }
            }
            CursorImageStatus::Surface(surface) => {
                let surface_elements: Vec<CursorRenderElement<R>> =
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        renderer,
                        surface,
                        location,
                        scale,
                        1.0,
                        Kind::Cursor,
                    );
                elements.extend(surface_elements);
            }
        }
        return elements;
    }
}

/// The hotspot of a client-provided cursor surface.
pub fn cursor_hotspot(status: &CursorImageStatus) -> Point<i32, Logical> {
    let CursorImageStatus::Surface(surface) = status else {
        return Point::from((0, 0));
    };
    return with_states(surface, |states| {
        let attributes = states
            .data_map
            .get::<std::sync::Mutex<CursorImageAttributes>>();
        let Some(attributes) = attributes else {
            return Point::from((0, 0));
        };
        return attributes
            .lock()
            .unwrap_or_else(|err| return err.into_inner())
            .hotspot;
    });
}

/// Whether a client-provided cursor surface went away.
pub fn is_dead_cursor_surface(status: &CursorImageStatus) -> bool {
    return match status {
        CursorImageStatus::Surface(surface) => return !surface.alive(),
        _ => false,
    };
}

/// Build the render buffer for a cursor image.
pub fn buffer_from_image(image: &Image, scale: i32) -> MemoryRenderBuffer {
    return MemoryRenderBuffer::from_slice(
        &image.pixels_rgba,
        Fourcc::Argb8888,
        (image.width as i32, image.height as i32),
        scale,
        Transform::Normal,
        None,
    );
}
