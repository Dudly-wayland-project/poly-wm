// Ported from anvil (GPL-3.0-or-later): anvil/src/cursor.rs, anvil/src/drawing.rs

//! Software cursor: xcursor theme loading plus the render element that draws
//! either a themed icon or a client-provided cursor surface.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::rc::Rc;
use std::time::Duration;

use poly_config::Cursor as CursorSettings;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Kind, render_elements};
use smithay::input::pointer::{CursorIcon, CursorImageAttributes, CursorImageStatus};
use smithay::utils::{IsAlive, Logical, Physical, Point, Scale, Transform};
use smithay::wayland::compositor::with_states;
use tracing::{debug, warn};
use xcursor::CursorTheme;
use xcursor::parser::{Image, parse_xcursor};

static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../../resources/cursor.rgba");

/// A loaded xcursor theme.
///
/// Every shape a client or a grab names (`pointer` over a link, `text` over
/// an entry, the resize arrows) is a separate xcursor file. They are loaded
/// the first time they are asked for and kept, so switching shapes while the
/// pointer moves never touches the filesystem twice.
pub struct Cursor {
    theme_files: CursorTheme,
    /// Loaded shapes; `None` remembers a shape the theme does not have.
    icons: RefCell<HashMap<CursorIcon, Option<Rc<[Image]>>>>,
    /// The `default` shape, which every missing one falls back to.
    default: Rc<[Image]>,
    /// Theme name the icons came from, for change detection on reload.
    theme: String,
    /// Nominal size in logical pixels, before the output scale.
    size: u32,
}

impl Cursor {
    /// Load the theme named by `general.cursor`.
    ///
    /// The configuration wins over the environment, and falls back to it when
    /// it says nothing: `$XCURSOR_THEME` (`"default"` when unset) and
    /// `$XCURSOR_SIZE` (`24`).
    pub fn load(settings: &CursorSettings) -> Cursor {
        let name = settings.theme_name();
        let size = settings.size_px();

        let theme_files = CursorTheme::load(&name);
        let default = load_icon(&theme_files, CursorIcon::Default)
            .map(Rc::from)
            .unwrap_or_else(|err| {
                warn!("unable to load xcursor theme {name:?}: {err:?}, using fallback cursor");
                return Rc::from(vec![fallback_image()]);
            });

        return Cursor {
            theme_files,
            icons: RefCell::new(HashMap::new()),
            default,
            theme: name,
            size,
        };
    }

    /// Whether this cursor is already the one `settings` asks for.
    ///
    /// A reload that does not change the cursor keeps the loaded icons, which
    /// also keeps the animation frame it is on.
    pub fn matches(&self, settings: &CursorSettings) -> bool {
        return self.theme == settings.theme_name() && self.size == settings.size_px();
    }

    /// Grab the frame of `icon`'s animation to draw at `time`.
    pub fn get_image(&self, icon: CursorIcon, scale: u32, time: Duration) -> Image {
        let size = self.size * scale;
        let images = self.images(icon);
        return frame(time.as_millis() as u32, size, &images);
    }

    /// The frames of `icon`, or of the default arrow when the theme lacks it.
    fn images(&self, icon: CursorIcon) -> Rc<[Image]> {
        if icon == CursorIcon::Default {
            return self.default.clone();
        }
        let mut icons = self.icons.borrow_mut();
        let loaded = icons.entry(icon).or_insert_with(|| {
            return match load_icon(&self.theme_files, icon) {
                Ok(images) => Some(Rc::from(images)),
                Err(err) => {
                    debug!(
                        "cursor theme {:?} has no {:?} icon: {err}",
                        self.theme,
                        icon.name()
                    );
                    None
                }
            };
        });
        return loaded
            .clone()
            .unwrap_or_else(|| return self.default.clone());
    }
}

/// The shape a cursor status names; surfaces and hidden cursors have none.
pub fn named_icon(status: &CursorImageStatus) -> CursorIcon {
    return match status {
        CursorImageStatus::Named(icon) => *icon,
        _ => CursorIcon::Default,
    };
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
    #[error("theme has no such cursor")]
    NoCursor,
    #[error("error reading cursor file: {0}")]
    File(#[from] std::io::Error),
    #[error("failed to parse xcursor file")]
    Parse,
}

/// Load `icon` under its CSS name or, failing that, one of the legacy X11
/// names themes still ship it under (`pointer` is often `hand2`).
fn load_icon(theme: &CursorTheme, icon: CursorIcon) -> Result<Vec<Image>, LoadError> {
    let icon_path = std::iter::once(icon.name())
        .chain(icon.alt_names().iter().copied())
        .find_map(|name| return theme.load_icon(name))
        .ok_or(LoadError::NoCursor)?;
    let mut cursor_file = std::fs::File::open(icon_path)?;
    let mut cursor_data = Vec::new();
    cursor_file.read_to_end(&mut cursor_data)?;
    let images = parse_xcursor(&cursor_data).ok_or(LoadError::Parse)?;
    if images.is_empty() {
        return Err(LoadError::Parse);
    }
    return Ok(images);
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
            // Named cursors are drawn from the theme image `push_cursor` set.
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
