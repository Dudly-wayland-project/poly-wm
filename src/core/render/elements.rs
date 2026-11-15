// Ported from niri (GPL-3.0-or-later): src/render_helpers/clipped_surface.rs,
// src/render_helpers/damage.rs, src/render_helpers/render_elements.rs

//! Render elements and the renderer traits they need.
//!
//! Two things live here. [`PolyRenderer`] / [`AsGlesRenderer`] expose the single
//! GLES renderer behind both backends, so that elements written against
//! `GlesRenderer` (blur, borders, shadows) work on udev too, where Smithay wraps
//! it in a multi-GPU `MultiRenderer`. And
//! [`OutputElements`] is the hand-written element enum: the `render_elements!`
//! macro cannot express "draw this variant through the inner GLES frame".
//!
//! [`WindowElement`] is the per-surface wrapper that clips to the window's
//! rounded rectangle and runs the window shader.

use glam::{Mat3, Vec2};
use smithay::backend::renderer::buffer_y_inverted;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::utils::{CropRenderElement, RescaleRenderElement};
use smithay::backend::renderer::element::{
    Element, Id, Kind, NamespacedElement, RenderElement, UnderlyingStorage,
};
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{
    GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{
    ImportAll, ImportDmaWl, ImportEgl, ImportMem, ImportMemWl, Renderer,
};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::core::cursor::CursorRenderElement;

use super::blur::{BlurElement, XrayCaptureElement};
use super::shaders::mat3_uniform;
use super::visual::CornerRadius;

/// A renderer that can draw everything Poly draws.
///
/// Client content can arrive as shared memory, dmabufs or EGL images, the
/// cursor and the Toasts are uploaded from memory, and elements clone their
/// textures. `AsGlesRenderer` is what lets us run GLSL 100 programs on both
/// backends.
pub trait PolyRenderer:
    Renderer<TextureId: Clone + Send + 'static>
    + ImportAll
    + ImportMem
    + ImportMemWl
    + ImportEgl
    + ImportDmaWl
    + AsGlesRenderer
{
    /// The `GlesFrame` underneath one of this renderer's frames.
    ///
    /// Elements that talk GL directly (blur, pixel shaders) draw through this.
    /// The udev `MultiFrame` forwards to its inner `GlesFrame`, which costs the
    /// multi-GPU damage accumulation but is correct on a single GPU.
    ///
    /// This lives on the renderer (not on the frame) because a
    /// `for<'a, 'b> R::Frame<'a, 'b>: Trait<'a, 'b>` bound trips over a
    /// compiler limitation around GAT where-clauses
    /// (<https://github.com/rust-lang/rust/issues/100013>).
    fn as_gles_frame<'frame, 'buffer, 'a>(
        frame: &'a mut Self::Frame<'frame, 'buffer>,
    ) -> &'a mut GlesFrame<'frame, 'buffer>
    where
        'buffer: 'frame;

    /// Wrap a GL error into the renderer's error type.
    fn gles_error(err: GlesError) -> Self::Error;
}

/// The `GlesRenderer` underneath a [`PolyRenderer`].
///
/// The winit/headless backend *is* a `GlesRenderer`; the udev backend wraps one
/// in a `MultiRenderer` and implements this in `backend::udev`.
pub trait AsGlesRenderer {
    /// The GLES renderer used for shader programs and offscreen textures.
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer;
}

impl AsGlesRenderer for GlesRenderer {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer {
        return self;
    }
}

impl PolyRenderer for GlesRenderer {
    fn as_gles_frame<'frame, 'buffer, 'a>(
        frame: &'a mut Self::Frame<'frame, 'buffer>,
    ) -> &'a mut GlesFrame<'frame, 'buffer>
    where
        'buffer: 'frame,
    {
        return frame;
    }

    fn gles_error(err: GlesError) -> GlesError {
        return err;
    }
}

/// The `u_*` uniforms a window shader receives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowUniforms {
    /// Seconds since the compositor started.
    pub time: f32,
    /// Opacity times the animation alpha.
    pub alpha: f32,
    /// Border width in logical pixels.
    pub border_width: f32,
    /// Straight border color; alpha 0 when there is no border.
    pub border_color: [f32; 4],
    /// Whether the window holds keyboard focus.
    pub focused: bool,
    /// The window's stable id.
    pub window_id: u64,
    /// Pointer position in window coordinates; negative when outside.
    pub cursor: [f32; 2],
    /// Whether the program reads `u_time` (disables damage optimization).
    pub uses_time: bool,
    /// Whether the program reads `u_cursor` (damage when the pointer moves).
    pub uses_cursor: bool,
}

impl Default for WindowUniforms {
    fn default() -> Self {
        return Self {
            time: 0.0,
            alpha: 1.0,
            border_width: 0.0,
            border_color: [0.0; 4],
            focused: false,
            window_id: 0,
            cursor: [-1.0, -1.0],
            uses_time: false,
            uses_cursor: false,
        };
    }
}

/// Everything needed to clip a window surface to its rounded rectangle and run
/// the window shader over it.
#[derive(Debug, Clone)]
pub struct WindowClip {
    /// The window program: built-in or the user's `window_shade()`.
    pub program: GlesTexProgram,
    /// The drawn window rectangle in output-local logical coordinates.
    pub geometry: Rectangle<f64, Logical>,
    /// Corner radii in the same units as `geometry`.
    pub corner_radius: CornerRadius,
    /// Scale applied to the window surfaces (the animation/overview rescale,
    /// `1.0` when the window is drawn at its natural size).
    ///
    /// The surface elements are placed at their unscaled size, so the shader's
    /// texture-to-window mapping has to apply the same factor; without it a
    /// zoomed-out thumbnail would clip itself to its top-left corner.
    pub rescale: Scale<f64>,
    /// Output scale, physical pixels per logical pixel.
    pub scale: f32,
    /// The workspace card in window-geometry space (`x0, y0, x1, y1`,
    /// normalized to `geometry`), outside of which fragments are discarded.
    /// `[0, 0, 1, 1]` disables the clip.
    pub card_clip: [f32; 4],
    /// Uniform values.
    pub uniforms: WindowUniforms,
}

impl WindowClip {
    fn compute_uniforms<R: PolyRenderer>(
        &self,
        inner: &WaylandSurfaceRenderElement<R>,
    ) -> Vec<Uniform<'static>> {
        let scale = Scale::from(f64::from(self.scale));
        let elem_geo = inner.geometry(scale);

        let elem_geo_loc = Vec2::new(elem_geo.loc.x as f32, elem_geo.loc.y as f32);
        let elem_geo_size = Vec2::new(elem_geo.size.w as f32, elem_geo.size.h as f32);
        // Size of the quad actually drawn on screen: the surface element is
        // rescaled around `origin` by `WindowElement`.
        let quad_size = Vec2::new(
            elem_geo_size.x * self.rescale.x as f32,
            elem_geo_size.y * self.rescale.y as f32,
        );

        let geo: Rectangle<i32, Physical> = self.geometry.to_physical_precise_round(scale);
        let geo_loc = Vec2::new(geo.loc.x as f32, geo.loc.y as f32);
        let geo_size = Vec2::new(geo.size.w as f32, geo.size.h as f32);

        let buf_size = inner.buffer_size();
        let buf_size = Vec2::new(buf_size.w as f32, buf_size.h as f32);

        let view = inner.view();
        let src_loc = Vec2::new(view.src.loc.x as f32, view.src.loc.y as f32);
        let src_size = Vec2::new(view.src.size.w as f32, view.src.size.h as f32);

        // The buffer's own transform is applied to the texture, so the shader
        // has to undo it to get geometry-oriented coordinates. Flipped
        // transforms are already handled by the texture matrix.
        let transform = inner.transform();
        let transform = match transform {
            Transform::_90 => Transform::_270,
            Transform::_270 => Transform::_90,
            other => other,
        };
        let transform_matrix = Mat3::from_translation(Vec2::new(0.5, 0.5))
            * transform.matrix()
            * Mat3::from_translation(-Vec2::new(0.5, 0.5));

        let y_invert = if buffer_y_inverted(inner.buffer()).unwrap_or(false) {
            Mat3::from_scale(Vec2::new(1.0, -1.0))
        } else {
            Mat3::IDENTITY
        };

        // Texture coords -> window geometry coords, normalized to `[0, 1]`.
        // The buffer transform is undone last, in the window's normalized
        // space (same order as niri's `clipped_surface`).
        let input_to_geo = transform_matrix
            * Mat3::from_scale(quad_size / geo_size)
            * Mat3::from_translation((elem_geo_loc - geo_loc) / quad_size)
            * Mat3::from_scale(buf_size / src_size)
            * Mat3::from_translation(-src_loc / buf_size)
            * y_invert;

        let uniforms = &self.uniforms;
        let logical_size = (self.geometry.size.w as f32, self.geometry.size.h as f32);

        return vec![
            Uniform::new("poly_scale", self.scale),
            Uniform::new("geo_size", logical_size),
            Uniform::new("corner_radius", <[f32; 4]>::from(self.corner_radius)),
            Uniform::new("poly_card_clip", self.card_clip),
            mat3_uniform("input_to_geo", input_to_geo),
            Uniform::new("u_time", uniforms.time),
            Uniform::new("u_size", logical_size),
            Uniform::new("u_scale", self.scale),
            Uniform::new("u_alpha", uniforms.alpha),
            Uniform::new("u_radius", self.corner_radius.max()),
            Uniform::new("u_border_width", uniforms.border_width),
            Uniform::new("u_border_color", uniforms.border_color),
            Uniform::new("u_focused", if uniforms.focused { 1.0f32 } else { 0.0 }),
            Uniform::new("u_window_id", uniforms.window_id as f32),
            Uniform::new("u_cursor", uniforms.cursor),
            Uniform::new("noise", 0.0f32),
        ];
    }

    /// The four corner squares in output-local logical coordinates.
    fn rounded_corners(&self) -> [Rectangle<f64, Logical>; 4] {
        let geo = self.geometry;
        let radius = &self.corner_radius;
        let tl = f64::from(radius.top_left);
        let tr = f64::from(radius.top_right);
        let br = f64::from(radius.bottom_right);
        let bl = f64::from(radius.bottom_left);

        return [
            Rectangle::new(geo.loc, Size::from((tl, tl))),
            Rectangle::new(
                Point::from((geo.loc.x + geo.size.w - tr, geo.loc.y)),
                Size::from((tr, tr)),
            ),
            Rectangle::new(
                Point::from((geo.loc.x + geo.size.w - br, geo.loc.y + geo.size.h - br)),
                Size::from((br, br)),
            ),
            Rectangle::new(
                Point::from((geo.loc.x, geo.loc.y + geo.size.h - bl)),
                Size::from((bl, bl)),
            ),
        ];
    }
}

/// One surface of a window: clipped to the window's rounded rectangle and
/// shaded with the window program.
#[derive(Debug)]
pub struct WindowElement<R: PolyRenderer> {
    /// The element id, namespaced with the window's visual epoch so that any
    /// visual change damages the whole window.
    id: Id,
    inner: WaylandSurfaceRenderElement<R>,
    /// Animation rescale origin, in output-local physical coordinates.
    origin: Point<i32, Physical>,
    /// Animation rescale factor.
    rescale: Scale<f64>,
    /// Clipping and shading; `None` for the plain path.
    clip: Option<WindowClip>,
}

impl<R: PolyRenderer> WindowElement<R> {
    /// Wrap `inner` (already placed at `origin` for the unscaled geometry).
    pub fn new(
        inner: WaylandSurfaceRenderElement<R>,
        origin: Point<i32, Physical>,
        rescale: Scale<f64>,
        clip: Option<WindowClip>,
        epoch: usize,
    ) -> Self {
        let id = inner.id().namespaced(epoch);
        return Self {
            id,
            inner,
            origin,
            rescale,
            clip,
        };
    }
}

impl<R: PolyRenderer> Element for WindowElement<R> {
    fn id(&self) -> &Id {
        return &self.id;
    }

    fn current_commit(&self) -> CommitCounter {
        // Keep the inner commit counter so incremental client damage survives.
        return self.inner.current_commit();
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        return self.inner.src();
    }

    fn transform(&self) -> Transform {
        return self.inner.transform();
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        let mut geometry = self.inner.geometry(scale);
        geometry.loc -= self.origin;
        geometry = geometry.to_f64().upscale(self.rescale).to_i32_round();
        geometry.loc += self.origin;
        return geometry;
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        let geometry = self.geometry(scale);
        let Some(clip) = &self.clip else {
            // Same as `RescaleRenderElement`: just upscale the client damage.
            return self
                .inner
                .damage_since(scale, commit)
                .into_iter()
                .map(|rect| return rect.to_f64().upscale(self.rescale).to_i32_up())
                .collect::<DamageSet<_, _>>();
        };

        // A shader-driven window has to repaint completely: `u_time` changes
        // every frame, and so may any uniform derived from the window state.
        if clip.uniforms.uses_time {
            return DamageSet::from_slice(&[Rectangle::from_size(geometry.size)]);
        }

        // Clipped to the window rectangle, and in element-local coordinates.
        let mut window = clip.geometry.to_physical_precise_round(scale);
        window.loc -= geometry.loc;

        return self
            .inner
            .damage_since(scale, commit)
            .into_iter()
            .map(|rect| return rect.to_f64().upscale(self.rescale).to_i32_up())
            .filter_map(|rect| return rect.intersection(window))
            .collect::<DamageSet<_, _>>();
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let regions: OpaqueRegions<i32, Physical> = self
            .inner
            .opaque_regions(scale)
            .into_iter()
            .map(|rect| return rect.to_f64().upscale(self.rescale).to_i32_round())
            .collect();

        let Some(clip) = &self.clip else {
            return regions;
        };

        let geometry = self.geometry(scale);
        let mut window = clip.geometry.to_physical_precise_round(scale);
        window.loc -= geometry.loc;
        let regions = regions
            .into_iter()
            .filter_map(|rect| return rect.intersection(window));

        if clip.corner_radius == CornerRadius::default() {
            return regions.collect();
        }

        // Conservative: cut the corner squares out of the opaque area.
        let corners = clip.rounded_corners().into_iter().map(|rect| {
            let mut rect = rect.to_physical_precise_up(scale);
            rect.loc -= geometry.loc;
            return rect;
        });
        return OpaqueRegions::from_slice(&Rectangle::subtract_rects_many(regions, corners));
    }

    fn alpha(&self) -> f32 {
        return self.inner.alpha();
    }

    fn kind(&self) -> Kind {
        return self.inner.kind();
    }
}

impl<R> RenderElement<R> for WindowElement<R>
where
    R: PolyRenderer,
    R::TextureId: Clone + 'static,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        let Some(clip) = &self.clip else {
            return RenderElement::<R>::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            );
        };

        let uniforms = clip.compute_uniforms(&self.inner);
        R::as_gles_frame(frame).override_default_tex_program(clip.program.clone(), uniforms);
        let result =
            RenderElement::<R>::draw(&self.inner, frame, src, dst, damage, opaque_regions, cache);
        R::as_gles_frame(frame).clear_tex_program_override();
        return result;
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        // Drawing through a shader program means the buffer cannot be scanned
        // out directly.
        if self.clip.is_some() {
            return None;
        }
        return Some(UnderlyingStorage::Wayland(self.inner.buffer()));
    }
}

/// A pixel shader (border, shadow) that both backends can draw.
#[derive(Debug, Clone)]
pub struct ShaderElement {
    inner: PixelShaderElement,
}

impl ShaderElement {
    /// Wrap a `PixelShaderElement`.
    pub fn new(inner: PixelShaderElement) -> Self {
        return Self { inner };
    }
}

impl Element for ShaderElement {
    fn id(&self) -> &Id {
        return Element::id(&self.inner);
    }

    fn current_commit(&self) -> CommitCounter {
        return Element::current_commit(&self.inner);
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        return Element::src(&self.inner);
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        return Element::geometry(&self.inner, scale);
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        return Element::opaque_regions(&self.inner, scale);
    }

    fn alpha(&self) -> f32 {
        return Element::alpha(&self.inner);
    }

    fn kind(&self) -> Kind {
        return Element::kind(&self.inner);
    }
}

impl RenderElement<GlesRenderer> for ShaderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        return RenderElement::<GlesRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        );
    }
}

/// A Bottom/Background layer surface scaled into one workspace card and
/// cropped to it, with the card index as the element-id namespace so several
/// copies of the same surface can live on one frame.
pub type CardLayerElement<R> =
    NamespacedElement<CropRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<R>>>>;

/// Everything Poly draws onto an output, front to back.
///
/// Written by hand because the `render_elements!` macro cannot route a variant
/// through [`PolyRenderer::as_gles_frame`] (blur and pixel shaders are
/// `GlesRenderer`-only).
/// A baked window fading out after its client was destroyed.
pub type ClosingRenderElement = CropRenderElement<TextureRenderElement<GlesTexture>>;

#[allow(clippy::large_enum_variant)]
pub enum OutputElements<R: PolyRenderer> {
    /// A plain surface: popups, layer surfaces, the lock screen.
    Surface(WaylandSurfaceRenderElement<R>),
    /// A Bottom/Background layer surface scaled and cropped into one
    /// workspace card of the overview.
    Card(CardLayerElement<R>),
    /// A window surface, clipped and shaded.
    Window(WindowElement<R>),
    /// The software cursor.
    Cursor(CursorRenderElement<R>),
    /// A solid color (background, drop indicator).
    Solid(SolidColorRenderElement),
    /// The blur backdrop of a window.
    Blur(BlurElement),
    /// The capture that fills the shared xray backdrop of an output. It also
    /// owns the background elements it repaints before capturing.
    XrayCapture(XrayCaptureElement<R>),
    /// A border or a drop shadow.
    Shader(ShaderElement),
    /// A Toast.
    Toast(MemoryRenderBufferRenderElement<R>),
    /// A baked snapshot of a closed window, fading out.
    Closing(ClosingRenderElement),
}

impl<R: PolyRenderer> std::fmt::Debug for OutputElements<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Surface(_) => "Surface",
            Self::Card(_) => "Card",
            Self::Window(_) => "Window",
            Self::Cursor(_) => "Cursor",
            Self::Solid(_) => "Solid",
            Self::Blur(_) => "Blur",
            Self::XrayCapture(_) => "XrayCapture",
            Self::Shader(_) => "Shader",
            Self::Toast(_) => "Toast",
            Self::Closing(_) => "Closing",
        };
        return f.debug_tuple("OutputElements").field(&name).finish();
    }
}

impl<R: PolyRenderer> Element for OutputElements<R> {
    fn id(&self) -> &Id {
        return match self {
            Self::Surface(element) => Element::id(element),
            Self::Card(element) => Element::id(element),
            Self::Window(element) => Element::id(element),
            Self::Cursor(element) => Element::id(element),
            Self::Solid(element) => Element::id(element),
            Self::Blur(element) => Element::id(element),
            Self::XrayCapture(element) => Element::id(element),
            Self::Shader(element) => Element::id(element),
            Self::Toast(element) => Element::id(element),
            Self::Closing(element) => Element::id(element),
        };
    }

    fn current_commit(&self) -> CommitCounter {
        return match self {
            Self::Surface(element) => Element::current_commit(element),
            Self::Card(element) => Element::current_commit(element),
            Self::Window(element) => Element::current_commit(element),
            Self::Cursor(element) => Element::current_commit(element),
            Self::Solid(element) => Element::current_commit(element),
            Self::Blur(element) => Element::current_commit(element),
            Self::XrayCapture(element) => Element::current_commit(element),
            Self::Shader(element) => Element::current_commit(element),
            Self::Toast(element) => Element::current_commit(element),
            Self::Closing(element) => Element::current_commit(element),
        };
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        return match self {
            Self::Surface(element) => Element::src(element),
            Self::Card(element) => Element::src(element),
            Self::Window(element) => Element::src(element),
            Self::Cursor(element) => Element::src(element),
            Self::Solid(element) => Element::src(element),
            Self::Blur(element) => Element::src(element),
            Self::XrayCapture(element) => Element::src(element),
            Self::Shader(element) => Element::src(element),
            Self::Toast(element) => Element::src(element),
            Self::Closing(element) => Element::src(element),
        };
    }

    fn transform(&self) -> Transform {
        return match self {
            Self::Surface(element) => Element::transform(element),
            Self::Card(element) => Element::transform(element),
            Self::Window(element) => Element::transform(element),
            Self::Cursor(element) => Element::transform(element),
            Self::Solid(element) => Element::transform(element),
            Self::Blur(element) => Element::transform(element),
            Self::XrayCapture(element) => Element::transform(element),
            Self::Shader(element) => Element::transform(element),
            Self::Toast(element) => Element::transform(element),
            Self::Closing(element) => Element::transform(element),
        };
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        return match self {
            Self::Surface(element) => Element::geometry(element, scale),
            Self::Card(element) => Element::geometry(element, scale),
            Self::Window(element) => Element::geometry(element, scale),
            Self::Cursor(element) => Element::geometry(element, scale),
            Self::Solid(element) => Element::geometry(element, scale),
            Self::Blur(element) => Element::geometry(element, scale),
            Self::XrayCapture(element) => Element::geometry(element, scale),
            Self::Shader(element) => Element::geometry(element, scale),
            Self::Toast(element) => Element::geometry(element, scale),
            Self::Closing(element) => Element::geometry(element, scale),
        };
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        return match self {
            Self::Surface(element) => element.damage_since(scale, commit),
            Self::Card(element) => element.damage_since(scale, commit),
            Self::Window(element) => element.damage_since(scale, commit),
            Self::Cursor(element) => element.damage_since(scale, commit),
            Self::Solid(element) => element.damage_since(scale, commit),
            Self::Blur(element) => element.damage_since(scale, commit),
            Self::XrayCapture(element) => element.damage_since(scale, commit),
            Self::Shader(element) => element.damage_since(scale, commit),
            Self::Toast(element) => element.damage_since(scale, commit),
            Self::Closing(element) => element.damage_since(scale, commit),
        };
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        return match self {
            Self::Surface(element) => Element::opaque_regions(element, scale),
            Self::Card(element) => Element::opaque_regions(element, scale),
            Self::Window(element) => Element::opaque_regions(element, scale),
            Self::Cursor(element) => Element::opaque_regions(element, scale),
            Self::Solid(element) => Element::opaque_regions(element, scale),
            Self::Blur(element) => Element::opaque_regions(element, scale),
            Self::XrayCapture(element) => Element::opaque_regions(element, scale),
            Self::Shader(element) => Element::opaque_regions(element, scale),
            Self::Toast(element) => Element::opaque_regions(element, scale),
            Self::Closing(element) => Element::opaque_regions(element, scale),
        };
    }

    fn alpha(&self) -> f32 {
        return match self {
            Self::Surface(element) => Element::alpha(element),
            Self::Card(element) => Element::alpha(element),
            Self::Window(element) => Element::alpha(element),
            Self::Cursor(element) => Element::alpha(element),
            Self::Solid(element) => Element::alpha(element),
            Self::Blur(element) => Element::alpha(element),
            Self::XrayCapture(element) => Element::alpha(element),
            Self::Shader(element) => Element::alpha(element),
            Self::Toast(element) => Element::alpha(element),
            Self::Closing(element) => Element::alpha(element),
        };
    }

    fn kind(&self) -> Kind {
        return match self {
            Self::Surface(element) => Element::kind(element),
            Self::Card(element) => Element::kind(element),
            Self::Window(element) => Element::kind(element),
            Self::Cursor(element) => Element::kind(element),
            Self::Solid(element) => Element::kind(element),
            Self::Blur(element) => Element::kind(element),
            Self::XrayCapture(element) => Element::kind(element),
            Self::Shader(element) => Element::kind(element),
            Self::Toast(element) => Element::kind(element),
            Self::Closing(element) => Element::kind(element),
        };
    }

    fn is_framebuffer_effect(&self) -> bool {
        return match self {
            Self::Blur(element) => Element::is_framebuffer_effect(element),
            _ => false,
        };
    }
}

impl<R> RenderElement<R> for OutputElements<R>
where
    R: PolyRenderer,
    R::TextureId: Clone + 'static,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        return match self {
            Self::Surface(element) => {
                RenderElement::<R>::draw(element, frame, src, dst, damage, opaque_regions, cache)
            }
            Self::Card(element) => {
                RenderElement::<R>::draw(element, frame, src, dst, damage, opaque_regions, cache)
            }
            Self::Window(element) => {
                RenderElement::<R>::draw(element, frame, src, dst, damage, opaque_regions, cache)
            }
            Self::Cursor(element) => {
                RenderElement::<R>::draw(element, frame, src, dst, damage, opaque_regions, cache)
            }
            Self::Solid(element) => {
                RenderElement::<R>::draw(element, frame, src, dst, damage, opaque_regions, cache)
            }
            Self::Toast(element) => {
                RenderElement::<R>::draw(element, frame, src, dst, damage, opaque_regions, cache)
            }
            // GL-only elements: go through the inner `GlesFrame`.
            Self::Blur(element) => RenderElement::<GlesRenderer>::draw(
                element,
                R::as_gles_frame(frame),
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            )
            .map_err(R::gles_error),
            Self::XrayCapture(element) => {
                RenderElement::<R>::draw(element, frame, src, dst, damage, opaque_regions, cache)
            }
            Self::Shader(element) => RenderElement::<GlesRenderer>::draw(
                element,
                R::as_gles_frame(frame),
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            )
            .map_err(R::gles_error),
            Self::Closing(element) => RenderElement::<GlesRenderer>::draw(
                element,
                R::as_gles_frame(frame),
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            )
            .map_err(R::gles_error),
        };
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        return match self {
            Self::Surface(element) => element.underlying_storage(renderer),
            Self::Card(element) => element.underlying_storage(renderer),
            Self::Window(element) => element.underlying_storage(renderer),
            Self::Cursor(element) => element.underlying_storage(renderer),
            Self::Toast(element) => element.underlying_storage(renderer),
            _ => None,
        };
    }

    fn capture_framebuffer(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), R::Error> {
        let Self::Blur(element) = self else {
            return Ok(());
        };
        return RenderElement::<GlesRenderer>::capture_framebuffer(
            element,
            R::as_gles_frame(frame),
            src,
            dst,
            cache,
        )
        .map_err(R::gles_error);
    }
}
