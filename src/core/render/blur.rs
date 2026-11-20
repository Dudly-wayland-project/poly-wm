// Ported from niri (GPL-3.0-or-later): src/render_helpers/blur.rs,
// src/render_helpers/framebuffer_effect.rs

//! Background blur: a framebuffer effect that captures what is behind a
//! window, runs a dual Kawase blur (or a user replacement) and composites the
//! result into the window.
//!
//! Correctness rule: the offscreen backdrop contains only
//! the elements *below* the blurred window. The blur element is therefore
//! pushed after the window in the front-to-back list, so it is drawn before it.
//!
//! With `general.blur.xray` the backdrop is shared instead: [`XrayBuffer`]
//! keeps one blurred copy of the wallpaper per output and every blur element
//! samples it. [`XrayCaptureElement`] *is* the wallpaper in the draw order: it
//! repaints the background (the Bottom/Background layers) and then captures
//! it, so the copy never contains windows and only a
//! wallpaper change makes it run the blur again.

use std::cell::{Cell, RefCell};
use std::cmp::max;
use std::rc::Rc;

use glam::{Mat3, Vec2};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Id, RenderElement};
use smithay::backend::renderer::gles::{
    GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, ffi, link_program,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{
    Color32F, ContextId, Frame as _, FrameContext as _, Offscreen, Renderer as _, Texture as _,
};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use tracing::warn;

use poly_config::Color;

use super::elements::{OutputElements, PolyRenderer};
use super::shaders::mat3_uniform;
use super::visual::CornerRadius;

const BLUR_VERT: &str = include_str!("shaders/blur.vert");

/// A single raw blur pass (downsample or upsample).
#[derive(Debug)]
struct BlurPass {
    program: ffi::types::GLuint,
    uniform_tex: ffi::types::GLint,
    uniform_half_pixel: ffi::types::GLint,
    uniform_offset: ffi::types::GLint,
    attrib_vert: ffi::types::GLint,
}

/// The compiled blur pass pair plus the composite program.
#[derive(Debug, Clone)]
pub struct BlurProgram(Rc<BlurProgramInner>);

#[derive(Debug)]
struct BlurProgramInner {
    down: BlurPass,
    up: BlurPass,
    finish: GlesTexProgram,
    /// Whether a pass (or the finish pass) reads `u_time`.
    pub uses_time: bool,
}

fn compile_pass(gl: &ffi::Gles2, src: &str) -> Result<BlurPass, GlesError> {
    let program = unsafe { link_program(gl, BLUR_VERT, src)? };
    let vert = c"vert";
    let tex = c"tex";
    let half_pixel = c"half_pixel";
    let offset = c"offset";
    unsafe {
        return Ok(BlurPass {
            program,
            uniform_tex: gl.GetUniformLocation(program, tex.as_ptr()),
            uniform_half_pixel: gl.GetUniformLocation(program, half_pixel.as_ptr()),
            uniform_offset: gl.GetUniformLocation(program, offset.as_ptr()),
            attrib_vert: gl.GetAttribLocation(program, vert.as_ptr()),
        });
    }
}

impl BlurProgram {
    /// Compile both passes and store the already compiled composite program.
    ///
    /// The raw GL programs are intentionally leaked on reload: the renderer
    /// does not expose a public way to schedule their deletion, and shader
    /// reloads are rare.
    pub fn compile(
        renderer: &mut GlesRenderer,
        down_src: &str,
        up_src: &str,
        finish: GlesTexProgram,
        uses_time: bool,
    ) -> Result<BlurProgram, String> {
        let inner = renderer
            .with_context(|gl| {
                let down = compile_pass(gl, down_src)?;
                let up = compile_pass(gl, up_src)?;
                return Ok::<_, GlesError>(BlurProgramInner {
                    down,
                    up,
                    finish,
                    uses_time,
                });
            })
            .map_err(|err| return err.to_string())?
            .map_err(|err| return err.to_string())?;
        return Ok(BlurProgram(Rc::new(inner)));
    }

    /// The composite program.
    pub fn finish(&self) -> &GlesTexProgram {
        return &self.0.finish;
    }

    /// Whether this program set reads `u_time`.
    pub fn uses_time(&self) -> bool {
        return self.0.uses_time;
    }

    /// Identity check, for decoration cache keys.
    pub fn ptr_eq(&self, other: &BlurProgram) -> bool {
        return Rc::ptr_eq(&self.0, &other.0);
    }
}

/// Blur options for one window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlurOptions {
    /// Number of down/up pass pairs.
    pub passes: u32,
    /// Sampling offset in pixels.
    pub offset: f64,
}

/// The intermediate texture chain of one blurred window.
#[derive(Debug)]
struct BlurChain {
    context_id: ContextId<GlesTexture>,
    /// Output texture followed by the intermediate textures, large to small.
    textures: Vec<GlesTexture>,
}

impl BlurChain {
    fn ensure(
        &mut self,
        create: impl FnMut(Size<i32, Buffer>) -> Result<GlesTexture, GlesError>,
        source: &GlesTexture,
        passes: usize,
    ) -> Result<(), GlesError> {
        if self
            .textures
            .first()
            .is_some_and(|output| return output.size() != source.size())
        {
            self.textures.clear();
        }
        // Create any missing textures (largest to smallest). The first texture
        // has the source size, every following one half of its predecessor.
        let mut w = source.size().w;
        let mut h = source.size().h;
        let mut create = create;
        for index in 0..=passes {
            if self.textures.len() <= index {
                self.textures.push(create(Size::new(w, h))?);
            }
            w = max(1, w / 2);
            h = max(1, h / 2);
        }
        self.textures.truncate(passes + 1);
        return Ok(());
    }
}

#[derive(Debug, Default)]
struct BlurCache {
    framebuffer: Option<GlesTexture>,
    chain: Option<BlurChain>,
    intermediate: Option<GlesTexture>,
}

/// Blit `region` of the current framebuffer into `cache` and blur it.
///
/// The caller must have drawn everything that belongs into the backdrop
/// already: the blit reads the frame's framebuffer as it is at this point.
/// `region` is in element/output coordinates; the texture is allocated in
/// buffer space so a rotated output keeps its orientation.
fn capture_region(
    frame: &mut GlesFrame<'_, '_>,
    region: Rectangle<i32, Physical>,
    program: &BlurProgram,
    options: BlurOptions,
    cache: &mut BlurCache,
) -> Result<(), GlesError> {
    let output_rect = Rectangle::from_size(frame.output_size());
    let Some(clamped) = region.intersection(output_rect) else {
        return Ok(());
    };
    let transform = frame.transformation();
    cache.intermediate = None;

    let size = transform
        .transform_size(clamped.size)
        .to_logical(1)
        .to_buffer(1, Transform::Normal);
    if cache
        .framebuffer
        .as_ref()
        .is_some_and(|buffer| return buffer.size() != size)
    {
        cache.framebuffer = None;
    }
    let framebuffer = match cache.framebuffer.clone() {
        Some(buffer) => buffer,
        None => {
            let buffer = {
                let mut guard = frame.renderer();
                let renderer: &mut GlesRenderer = guard.as_mut();
                Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, size)?
            };
            cache.framebuffer = Some(buffer.clone());
            buffer
        }
    };

    // Blit the current framebuffer content (everything below the element, which
    // has already been drawn) into our texture.
    let blit_dst = transform.transform_rect_in(clamped, &output_rect.size);
    frame.with_context(|gl| unsafe {
        let mut previous_fbo = 0i32;
        gl.GetIntegerv(ffi::DRAW_FRAMEBUFFER_BINDING, &mut previous_fbo as *mut _);
        let scissor_was_enabled = gl.IsEnabled(ffi::SCISSOR_TEST) == ffi::TRUE;
        gl.Disable(ffi::SCISSOR_TEST);

        let mut fbo = 0;
        gl.GenFramebuffers(1, &mut fbo as *mut _);
        gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, fbo);
        gl.FramebufferTexture2D(
            ffi::DRAW_FRAMEBUFFER,
            ffi::COLOR_ATTACHMENT0,
            ffi::TEXTURE_2D,
            framebuffer.tex_id(),
            0,
        );
        gl.BlitFramebuffer(
            blit_dst.loc.x,
            blit_dst.loc.y,
            blit_dst.loc.x + blit_dst.size.w,
            blit_dst.loc.y + blit_dst.size.h,
            0,
            0,
            size.w,
            size.h,
            ffi::COLOR_BUFFER_BIT,
            ffi::LINEAR,
        );

        gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, previous_fbo as u32);
        if scissor_was_enabled {
            gl.Enable(ffi::SCISSOR_TEST);
        }
        gl.DeleteFramebuffers(1, &fbo as *const _);
    })?;

    // Run the Kawase passes.
    let mut guard = frame.renderer();
    let renderer: &mut GlesRenderer = guard.as_mut();
    let passes = options.passes.clamp(1, 16) as usize;
    let chain = match &mut cache.chain {
        Some(chain) if chain.context_id == renderer.context_id() => chain,
        slot => slot.insert(BlurChain {
            context_id: renderer.context_id(),
            textures: Vec::new(),
        }),
    };
    let result = chain
        .ensure(
            |size| return renderer.create_buffer(Fourcc::Abgr8888, size),
            &framebuffer,
            passes,
        )
        .and_then(|()| {
            return render_passes(
                renderer,
                program,
                &mut chain.textures,
                &framebuffer,
                options,
            );
        });
    drop(guard);

    match result {
        Ok(texture) => cache.intermediate = Some(texture),
        Err(err) => warn!("error rendering blur: {err:?}"),
    }
    return Ok(());
}

/// The matrix Smithay uses to map destination-local pixels to texture
/// coordinates, see `build_texture_mat` in the GLES renderer.
///
/// It is reproduced here because the blur shader has to undo it: the shader
/// only sees the interpolated texture coordinates.
fn texture_coords_to_dst(
    src: Rectangle<f64, Buffer>,
    dst: Rectangle<i32, Physical>,
    texture_size: Size<i32, Buffer>,
    transform: Transform,
) -> Mat3 {
    let dst_src_size = transform.transform_size(src.size);
    let scale = Vec2::new(
        (dst_src_size.w / f64::from(dst.size.w.max(1))) as f32,
        (dst_src_size.h / f64::from(dst.size.h.max(1))) as f32,
    );

    let mut tex_mat = Mat3::from_scale(scale);
    // Compensate for the texture transform, with the same translation table as
    // Smithay.
    let translation = match transform {
        Transform::Normal | Transform::Flipped90 => Vec2::ZERO,
        Transform::_90 => Vec2::new(0.0, dst_src_size.h as f32),
        Transform::_180 => Vec2::new(dst_src_size.w as f32, dst_src_size.h as f32),
        Transform::_270 => Vec2::new(dst_src_size.h as f32, 0.0),
        Transform::Flipped => Vec2::new(dst_src_size.w as f32, 0.0),
        Transform::Flipped180 => Vec2::new(0.0, dst_src_size.h as f32),
        Transform::Flipped270 => Vec2::new(dst_src_size.h as f32, dst_src_size.w as f32),
    };
    tex_mat = Mat3::from(transform.matrix()) * tex_mat;
    tex_mat = Mat3::from_translation(translation) * tex_mat;
    tex_mat = Mat3::from_translation(Vec2::new(src.loc.x as f32, src.loc.y as f32)) * tex_mat;
    tex_mat = Mat3::from_scale(Vec2::new(
        1.0 / texture_size.w as f32,
        1.0 / texture_size.h as f32,
    )) * tex_mat;
    return tex_mat;
}

/// Re-tag a physical rectangle as buffer coordinates.
///
/// [`Transform::transform_rect_in`] keeps the numeric type it was given, but its
/// values are already in buffer space: the transform swaps width and height and
/// mirrors the location into the rotated area.
fn as_buffer_rect(rect: Rectangle<i32, Physical>) -> Rectangle<f64, Buffer> {
    return Rectangle::new(
        Point::from((f64::from(rect.loc.x), f64::from(rect.loc.y))),
        Size::from((f64::from(rect.size.w), f64::from(rect.size.h))),
    );
}

/// Clip element-local `damage` to the visible part of `dst` and rebase it onto
/// `dest`.
///
/// Smithay hands us damage relative to `dst.loc`, while
/// `render_texture_from_to` expects it relative to the rectangle it draws.
/// The two only differ when the element is partially outside the output.
fn clip_damage(
    damage: &[Rectangle<i32, Physical>],
    dst: Rectangle<i32, Physical>,
    dest: Rectangle<i32, Physical>,
) -> Vec<Rectangle<i32, Physical>> {
    let visible = Rectangle::new(dest.loc - dst.loc, dest.size);
    let offset = dest.loc - dst.loc;
    return damage
        .iter()
        .filter_map(|rect| return rect.intersection(visible))
        .map(|rect| return Rectangle::new(rect.loc - offset, rect.size))
        .collect();
}

/// Where a [`BlurElement`] takes its backdrop from.
enum BlurSource {
    /// The framebuffer in front of the element, captured per element.
    Framebuffer,
    /// The shared per-output xray copy.
    Xray(Rc<RefCell<XrayBuffer>>),
}

/// A blurred backdrop behind one window.
pub struct BlurElement {
    id: Id,
    commit: CommitCounter,
    program: BlurProgram,
    source: BlurSource,
    /// Window geometry in output-local logical coordinates.
    geometry: Rectangle<f64, Logical>,
    /// Element-local logical rectangles to paint, when a client asked for a
    /// partial backdrop (`ext-background-effect-v1`). `None` paints the whole
    /// element, which is the configured window blur.
    regions: Option<Rc<[Rectangle<i32, Logical>]>>,
    corner_radius: CornerRadius,
    scale: f64,
    options: BlurOptions,
    noise: f32,
    alpha: f32,
    uses_time: bool,
    /// Seconds since startup, for `u_time` in a custom `finish()`.
    time: f32,
}

impl std::fmt::Debug for BlurElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        return f
            .debug_struct("BlurElement")
            .field("id", &self.id)
            .field("geometry", &self.geometry)
            .field("regions", &self.regions.as_ref().map(|r| return r.len()))
            .finish();
    }
}

impl BlurElement {
    /// Build a blur element for a window.
    ///
    /// `regions` are element-local logical rectangles, relative to `geometry`.
    /// `None` blurs `geometry` as a whole; `Some` blurs only those parts, which
    /// is what a client asked for through `ext-background-effect-v1`. An empty
    /// slice is treated as `None`, so a client that committed an empty region
    /// cannot turn the backdrop into nothing by accident.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Id,
        commit: CommitCounter,
        program: BlurProgram,
        geometry: Rectangle<f64, Logical>,
        regions: Option<Rc<[Rectangle<i32, Logical>]>>,
        corner_radius: CornerRadius,
        scale: f64,
        options: BlurOptions,
        noise: f32,
        alpha: f32,
        uses_time: bool,
        time: f32,
    ) -> BlurElement {
        return BlurElement {
            id,
            commit,
            program,
            source: BlurSource::Framebuffer,
            geometry,
            regions: regions.filter(|regions| return !regions.is_empty()),
            corner_radius,
            scale,
            options,
            noise,
            alpha,
            uses_time,
            time,
        };
    }

    /// Take the backdrop from the shared per-output xray buffer instead of
    /// capturing the framebuffer below this element.
    pub fn with_xray(mut self, buffer: Rc<RefCell<XrayBuffer>>) -> BlurElement {
        buffer.borrow_mut().mark_used();
        self.source = BlurSource::Xray(buffer);
        return self;
    }

    /// The uniforms of the composite shader.
    ///
    /// `src`, `texture_size` and `transform` must be exactly what is passed to
    /// `render_texture_from_to`: the shader only sees the interpolated texture
    /// coordinates, so `input_to_geo` has to undo the texture matrix Smithay
    /// builds from them.
    fn compute_uniforms(
        &self,
        dst: Rectangle<i32, Physical>,
        src: Rectangle<f64, Buffer>,
        texture_size: Size<i32, Buffer>,
        transform: Transform,
    ) -> [Uniform<'static>; 8] {
        // `geo_size` and `corner_radius` are logical, which is what the
        // rounding shader expects.
        let geo_size = Vec2::new(self.geometry.size.w as f32, self.geometry.size.h as f32);
        let geo_phys: Rectangle<i32, Physical> = self
            .geometry
            .to_physical_precise_round(Scale::from(self.scale));
        let geo_loc = Vec2::new(geo_phys.loc.x as f32, geo_phys.loc.y as f32);
        let dst_loc = Vec2::new(dst.loc.x as f32, dst.loc.y as f32);

        // Texture coords -> destination pixels -> geometry-relative -> [0, 1].
        let tex_mat = texture_coords_to_dst(src, dst, texture_size, transform);
        let input_to_geo = Mat3::from_scale(Vec2::new(
            1.0 / geo_phys.size.w.max(1) as f32,
            1.0 / geo_phys.size.h.max(1) as f32,
        )) * Mat3::from_translation(dst_loc - geo_loc)
            * tex_mat.inverse();

        return [
            Uniform::new("poly_scale", self.scale as f32),
            Uniform::new("geo_size", (geo_size.x, geo_size.y)),
            Uniform::new("corner_radius", <[f32; 4]>::from(self.corner_radius)),
            mat3_uniform("input_to_geo", input_to_geo),
            Uniform::new("noise", self.noise),
            Uniform::new("u_scale", self.scale as f32),
            Uniform::new("u_size", (geo_size.x, geo_size.y)),
            Uniform::new("u_time", self.time),
        ];
    }

    fn capture(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        let inner = cache.get_or_insert::<RefCell<BlurCache>, _>(|| {
            return RefCell::new(BlurCache::default());
        });
        let mut inner = inner.borrow_mut();
        return capture_region(frame, dst, &self.program, self.options, &mut inner);
    }

    /// Intersect the frame's element-local damage with the client's blur region.
    ///
    /// Damage arrives in physical pixels while the region is expressed in
    /// logical surface pixels, so it is scaled by the factor the element is
    /// currently drawn at: both the output scale and the opening animation are
    /// folded into `dst`.
    fn clip_to_regions(
        &self,
        damage: &[Rectangle<i32, Physical>],
        dst: Rectangle<i32, Physical>,
    ) -> Vec<Rectangle<i32, Physical>> {
        let Some(regions) = &self.regions else {
            return damage.to_vec();
        };
        if self.geometry.size.w <= 0.0 || self.geometry.size.h <= 0.0 {
            return Vec::new();
        }
        let sx = f64::from(dst.size.w) / self.geometry.size.w.max(1.0);
        let sy = f64::from(dst.size.h) / self.geometry.size.h.max(1.0);
        let mut clipped = Vec::with_capacity(damage.len());
        for region in regions.iter() {
            // Round the extremities, not location and size: strips that touch
            // in logical space then still touch in physical space, with no
            // seam and no doubly painted row at fractional scales.
            let a = Point::<i32, Physical>::from((
                (f64::from(region.loc.x) * sx).round() as i32,
                (f64::from(region.loc.y) * sy).round() as i32,
            ));
            let b = Point::<i32, Physical>::from((
                (f64::from(region.loc.x + region.size.w) * sx).round() as i32,
                (f64::from(region.loc.y + region.size.h) * sy).round() as i32,
            ));
            if b.x <= a.x || b.y <= a.y {
                continue;
            }
            let physical = Rectangle::from_extremities(a, b);
            clipped.extend(
                damage
                    .iter()
                    .filter_map(|rect| return rect.intersection(physical)),
            );
        }
        return clipped;
    }

    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let output_rect = Rectangle::from_size(frame.output_size());
        let Some(clamped_dst) = dst.intersection(output_rect) else {
            return Ok(());
        };
        let transform = frame.transformation();
        // `texture` and `src` describe the blurred backdrop in buffer space.
        let (texture, src) = match &self.source {
            BlurSource::Framebuffer => {
                let Some(cache) = cache else {
                    return Ok(());
                };
                let Some(inner) = cache.get::<RefCell<BlurCache>>() else {
                    return Ok(());
                };
                let inner = inner.borrow();
                let Some(texture) = &inner.intermediate else {
                    return Ok(());
                };
                // The capture is exactly the drawn rectangle, so the whole
                // texture maps to it.
                (
                    texture.clone(),
                    Rectangle::from_size(texture.size().to_f64()),
                )
            }
            BlurSource::Xray(buffer) => {
                let Some(texture) = buffer.borrow().texture() else {
                    return Ok(());
                };
                // The shared texture covers the whole output, so the drawn
                // rectangle is a crop of it.
                let src = transform.transform_rect_in(clamped_dst, &output_rect.size);
                (texture, as_buffer_rect(src))
            }
        };

        // `damage` is element-local (Smithay subtracts the element origin before
        // calling us), so it must be clamped against the visible part of the
        // element in local coordinates, not against `clamped_dst` whose
        // location is in output coordinates.
        // A client blur region narrows what is painted: `render_texture_from_to`
        // draws one instance per damage rectangle, so intersecting the damage
        // with the region is enough to leave the rest of the window untouched —
        // no scissor state and no second shader path.
        let damage = clip_damage(&self.clip_to_regions(damage, dst), dst, clamped_dst);
        if damage.is_empty() {
            return Ok(());
        }

        let uniforms = self.compute_uniforms(clamped_dst, src, texture.size(), transform);
        return frame.render_texture_from_to(
            &texture,
            src,
            clamped_dst,
            &damage,
            &[],
            transform,
            self.alpha,
            Some(self.program.finish()),
            &uniforms,
        );
    }
}

/// Run the dual Kawase down/up passes into `textures` and return the biggest
/// one.
fn render_passes(
    renderer: &mut GlesRenderer,
    program: &BlurProgram,
    textures: &mut [GlesTexture],
    source: &GlesTexture,
    options: BlurOptions,
) -> Result<GlesTexture, GlesError> {
    let offset = options.offset as f32;
    let program = program.0.clone();
    renderer.with_context(|gl| unsafe {
        while gl.GetError() != ffi::NO_ERROR {}

        // Save the GL state the frame expects us to restore.
        let mut previous_fbo = 0i32;
        gl.GetIntegerv(ffi::DRAW_FRAMEBUFFER_BINDING, &mut previous_fbo as *mut _);
        let mut previous_viewport = [0i32; 4];
        gl.GetIntegerv(ffi::VIEWPORT, previous_viewport.as_mut_ptr());
        let blend_was_enabled = gl.IsEnabled(ffi::BLEND) == ffi::TRUE;
        let scissor_was_enabled = gl.IsEnabled(ffi::SCISSOR_TEST) == ffi::TRUE;

        gl.Disable(ffi::BLEND);
        gl.Disable(ffi::SCISSOR_TEST);
        gl.ActiveTexture(ffi::TEXTURE0);

        let mut fbo = 0;
        gl.GenFramebuffers(1, &mut fbo as *mut _);
        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);

        let vertices: [f32; 12] = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0];

        // Downsample: source -> textures[0] -> textures[1] -> ...
        let pass = &program.down;
        gl.UseProgram(pass.program);
        gl.Uniform1i(pass.uniform_tex, 0);
        gl.Uniform1f(pass.uniform_offset, offset);
        gl.EnableVertexAttribArray(pass.attrib_vert as u32);
        gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
        gl.VertexAttribPointer(
            pass.attrib_vert as u32,
            2,
            ffi::FLOAT,
            ffi::FALSE,
            0,
            vertices.as_ptr().cast(),
        );
        for index in 1..textures.len() {
            let source = if index == 1 {
                source
            } else {
                &textures[index - 1]
            };
            let target = &textures[index];
            let size = target.size();
            gl.Viewport(0, 0, size.w, size.h);
            // During downsampling, half_pixel is half of the destination pixel.
            gl.Uniform2f(
                pass.uniform_half_pixel,
                0.5 / size.w as f32,
                0.5 / size.h as f32,
            );
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                target.tex_id(),
                0,
            );
            bind_linear(gl, source.tex_id());
            gl.DrawArrays(ffi::TRIANGLES, 0, 6);
        }
        gl.DisableVertexAttribArray(pass.attrib_vert as u32);

        // Upsample: smallest -> ... -> textures[0].
        let pass = &program.up;
        gl.UseProgram(pass.program);
        gl.Uniform1i(pass.uniform_tex, 0);
        gl.Uniform1f(pass.uniform_offset, offset);
        gl.EnableVertexAttribArray(pass.attrib_vert as u32);
        gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
        gl.VertexAttribPointer(
            pass.attrib_vert as u32,
            2,
            ffi::FLOAT,
            ffi::FALSE,
            0,
            vertices.as_ptr().cast(),
        );
        for index in (0..textures.len() - 1).rev() {
            let source = &textures[index + 1];
            let target = &textures[index];
            let target_size = target.size();
            gl.Viewport(0, 0, target_size.w, target_size.h);
            // During upsampling, half_pixel is half of the source pixel.
            let source_size = source.size();
            gl.Uniform2f(
                pass.uniform_half_pixel,
                0.5 / source_size.w as f32,
                0.5 / source_size.h as f32,
            );
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                target.tex_id(),
                0,
            );
            bind_linear(gl, source.tex_id());
            gl.DrawArrays(ffi::TRIANGLES, 0, 6);
        }
        gl.DisableVertexAttribArray(pass.attrib_vert as u32);

        // Restore the GL state.
        gl.DeleteFramebuffers(1, &fbo as *const _);
        gl.BindFramebuffer(ffi::FRAMEBUFFER, previous_fbo as u32);
        gl.Viewport(
            previous_viewport[0],
            previous_viewport[1],
            previous_viewport[2],
            previous_viewport[3],
        );
        if blend_was_enabled {
            gl.Enable(ffi::BLEND);
        }
        if scissor_was_enabled {
            gl.Enable(ffi::SCISSOR_TEST);
        }

        if gl.GetError() != ffi::NO_ERROR {
            warn!("GL error while rendering blur");
        }
    })?;

    return Ok(textures[0].clone());
}

unsafe fn bind_linear(gl: &ffi::Gles2, texture: ffi::types::GLuint) {
    unsafe {
        gl.BindTexture(ffi::TEXTURE_2D, texture);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
        gl.TexParameteri(
            ffi::TEXTURE_2D,
            ffi::TEXTURE_WRAP_S,
            ffi::CLAMP_TO_EDGE as i32,
        );
        gl.TexParameteri(
            ffi::TEXTURE_2D,
            ffi::TEXTURE_WRAP_T,
            ffi::CLAMP_TO_EDGE as i32,
        );
    }
}

impl Element for BlurElement {
    fn id(&self) -> &Id {
        return &self.id;
    }

    fn current_commit(&self) -> CommitCounter {
        return self.commit;
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        return Rectangle::from_size(self.geometry.size.to_buffer(1.0, Transform::Normal));
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        return self.geometry.to_physical_precise_round(scale);
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // With xray the texture comes from the shared buffer; a repaint of that
        // buffer is requested by `XrayCaptureElement`, and this element is drawn
        // in the same frames because the request damages the whole output.
        let own_texture = match self.source {
            BlurSource::Framebuffer => commit != Some(self.commit),
            BlurSource::Xray(_) => false,
        };
        if self.uses_time || own_texture {
            return DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)]);
        }
        return DamageSet::default();
    }

    fn is_framebuffer_effect(&self) -> bool {
        return matches!(self.source, BlurSource::Framebuffer);
    }
}

impl RenderElement<GlesRenderer> for BlurElement {
    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        if !matches!(self.source, BlurSource::Framebuffer) {
            return Ok(());
        }
        return self.capture(frame, dst, cache);
    }

    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        return self.draw(frame, dst, damage, cache);
    }
}

/// Everything that determines the content of one xray capture.
///
/// The damage tracker only reports *that* an element changed, not what it
/// contains, so [`XrayBuffer`] compares this snapshot instead. Any difference
/// bumps the generation, which makes the next frame capture again.
#[derive(Debug, Clone, PartialEq)]
pub struct XrayState {
    /// The renderer context the cached textures belong to.
    pub context: ContextId<GlesTexture>,
    /// Configuration generation: blur parameters and the output background.
    pub config_generation: u64,
    /// Shader generation, so a reload re-captures.
    pub shader_epoch: u64,
    /// Number of Kawase pass pairs.
    pub passes: u32,
    /// Sampling offset in pixels, compared as bits.
    pub offset_bits: u64,
    /// The clear colour below the wallpaper.
    pub background: Option<Color>,
    /// Output size, scale and transform.
    pub size: Size<i32, Physical>,
    /// Fractional scale, compared as bits.
    pub scale_bits: u64,
    pub transform: Transform,
    /// Frame time when the blur program reads `u_time` (zero otherwise), so a
    /// time-varying `finish()` keeps the shared texture up to date.
    pub time_bits: u64,
    /// Bottom/Background layers in draw order: protocol id, commit, geometry.
    pub layers: Vec<(u32, CommitCounter, Rectangle<i32, Logical>)>,
}

/// One blurred copy of the wallpaper, shared by every blur element of an output.
///
/// The copy is only re-rendered when [`XrayState`] changes, which is what makes
/// xray cheaper than a per-window backdrop: a static wallpaper costs one blur
/// per change instead of one per blurred window and frame.
#[derive(Debug)]
pub struct XrayBuffer {
    id: Id,
    /// Bumped whenever `state` changes.
    generation: u64,
    /// The state the current capture was made from.
    state: Option<XrayState>,
    /// The generation the cached texture holds.
    captured: Option<u64>,
    /// The generation `damage_since` already asked to be repainted.
    ///
    /// A fully covered capture element is skipped by the damage tracker before
    /// `damage_since` runs, so this keeps it from requesting a full-output
    /// repaint on every frame while nothing can be captured.
    requested: Cell<Option<u64>>,
    /// Changes whenever the generation does, for the damage tracker.
    commit: CommitCounter,
    cache: BlurCache,
    /// Whether a blur element used the buffer while building this frame.
    used: bool,
}

impl XrayBuffer {
    pub fn new() -> XrayBuffer {
        return XrayBuffer {
            id: Id::new(),
            generation: 0,
            state: None,
            captured: None,
            requested: Cell::new(None),
            commit: CommitCounter::default(),
            cache: BlurCache::default(),
            used: false,
        };
    }

    /// Start a frame: remember `state` and bump the generation if it changed.
    pub fn begin_frame(&mut self, state: XrayState) {
        self.used = false;
        if self.state.as_ref() == Some(&state) {
            return;
        }
        if self
            .state
            .as_ref()
            .is_some_and(|old| return old.context != state.context)
        {
            // The cached textures belong to another renderer context.
            self.cache = BlurCache::default();
        }
        self.state = Some(state);
        self.generation = self.generation.wrapping_add(1);
        self.requested.set(None);
        self.commit.increment();
    }

    /// Record that a blur element samples this buffer.
    pub fn mark_used(&mut self) {
        self.used = true;
    }

    /// Whether a blur element sampled the buffer while building this frame.
    pub fn is_used(&self) -> bool {
        return self.used;
    }

    /// The element id of the capture element, stable across frames.
    pub fn id(&self) -> &Id {
        return &self.id;
    }

    /// The commit counter the capture element reports.
    pub fn commit(&self) -> CommitCounter {
        return self.commit;
    }

    /// Whether the cached texture matches the current state.
    pub fn is_current(&self) -> bool {
        return self.captured == Some(self.generation);
    }

    /// Ask for one full-output repaint so the next draw can capture.
    ///
    /// Returns `false` when this generation was already requested.
    pub fn request_capture(&self) -> bool {
        if self.requested.get() == Some(self.generation) {
            return false;
        }
        self.requested.set(Some(self.generation));
        return true;
    }

    /// The blurred wallpaper, if it was captured at least once.
    pub fn texture(&self) -> Option<GlesTexture> {
        return self.cache.intermediate.clone();
    }

    /// Blit the current framebuffer into the shared texture and blur it.
    pub fn capture(
        &mut self,
        frame: &mut GlesFrame<'_, '_>,
        program: &BlurProgram,
        options: BlurOptions,
    ) {
        let region = Rectangle::from_size(frame.output_size());
        match capture_region(frame, region, program, options, &mut self.cache) {
            Ok(()) => {
                self.captured = Some(self.generation);
            }
            Err(err) => warn!("error capturing the xray background: {err:?}"),
        }
    }
}

/// The element that owns the background and captures the shared xray backdrop.
///
/// It is the backmost element of the frame and holds the elements that paint the
/// wallpaper: the Bottom/Background layers. Instead of blitting whatever the framebuffer happens to
/// contain, it repaints them itself: an opaque window may have covered this area
/// in an earlier frame and the damage tracker then never repainted the wallpaper
/// below it, so a plain blit would capture window pixels.
///
/// Its geometry is the whole output: a capture is only ever a full frame.
pub struct XrayCaptureElement<R: PolyRenderer> {
    id: Id,
    buffer: Rc<RefCell<XrayBuffer>>,
    geometry: Rectangle<i32, Physical>,
    program: BlurProgram,
    options: BlurOptions,
    /// The background, front to back, exactly as the output would draw it.
    backdrop: Vec<OutputElements<R>>,
    /// The colour below the backdrop (the output background).
    background: Color32F,
    /// Output scale, for the backdrop geometries.
    scale: Scale<f64>,
}

impl<R: PolyRenderer> std::fmt::Debug for XrayCaptureElement<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        return f
            .debug_struct("XrayCaptureElement")
            .field("id", &self.id)
            .field("geometry", &self.geometry)
            .field("backdrop", &self.backdrop.len())
            .finish();
    }
}

impl<R: PolyRenderer> XrayCaptureElement<R> {
    pub fn new(
        buffer: Rc<RefCell<XrayBuffer>>,
        geometry: Rectangle<i32, Physical>,
        program: BlurProgram,
        options: BlurOptions,
        backdrop: Vec<OutputElements<R>>,
        background: [f32; 4],
        scale: Scale<f64>,
    ) -> XrayCaptureElement<R> {
        let id = buffer.borrow().id().clone();
        return XrayCaptureElement {
            id,
            buffer,
            geometry,
            program,
            options,
            backdrop,
            background: background.into(),
            scale,
        };
    }
}

impl<R: PolyRenderer> Element for XrayCaptureElement<R> {
    fn id(&self) -> &Id {
        return &self.id;
    }

    fn current_commit(&self) -> CommitCounter {
        return self.buffer.borrow().commit();
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        let size: Size<f64, Buffer> = Size::from((
            f64::from(self.geometry.size.w),
            f64::from(self.geometry.size.h),
        ));
        return Rectangle::from_size(size);
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        return self.geometry;
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        _commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        let buffer = self.buffer.borrow();
        if buffer.is_current() {
            return DamageSet::default();
        }
        if !buffer.request_capture() {
            return DamageSet::default();
        }
        return DamageSet::from_slice(&[Rectangle::from_size(self.geometry.size)]);
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        return OpaqueRegions::default();
    }
}

impl<R: PolyRenderer> RenderElement<R> for XrayCaptureElement<R> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        _dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        let mut buffer = self.buffer.borrow_mut();
        // A pending capture needs the complete background, not just the damage
        // the tracker let through: it is a full-frame texture.
        let capture = !buffer.is_current();
        let full = Rectangle::from_size(self.geometry.size);
        let region: &[Rectangle<i32, Physical>] = if capture {
            std::slice::from_ref(&full)
        } else {
            damage
        };
        if region.is_empty() {
            return Ok(());
        }

        // Rebuild the background from scratch instead of reusing the
        // framebuffer, which may still hold a window that has since moved away.
        R::as_gles_frame(frame)
            .clear(self.background, region)
            .map_err(R::gles_error)?;
        for element in &self.backdrop {
            let dst = element.geometry(self.scale);
            let mut local: Vec<Rectangle<i32, Physical>> = Vec::with_capacity(region.len());
            for rect in region {
                if let Some(hit) = rect.intersection(dst) {
                    local.push(Rectangle::new(hit.loc - dst.loc, hit.size));
                }
            }
            if local.is_empty() {
                continue;
            }
            // The tracker subtracts the element origin from the damage before
            // calling `draw`; do the same so the elements see element-local
            // rectangles. The effects cache belongs to the capture element, not
            // to them.
            RenderElement::<R>::draw(element, frame, element.src(), dst, &local, &[], None)?;
        }

        if capture {
            buffer.capture(R::as_gles_frame(frame), &self.program, self.options);
        }
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(context: ContextId<GlesTexture>) -> XrayState {
        return XrayState {
            context,
            config_generation: 0,
            shader_epoch: 0,
            passes: 3,
            offset_bits: 3.0f64.to_bits(),
            background: None,
            size: (1920, 1080).into(),
            scale_bits: 1.0f64.to_bits(),
            transform: Transform::Normal,
            time_bits: 0,
            layers: Vec::new(),
        };
    }

    #[test]
    fn xray_requests_one_capture_per_generation() {
        let mut buffer = XrayBuffer::new();
        let first = state(ContextId::new());
        buffer.begin_frame(first.clone());
        let commit = buffer.commit();
        assert!(!buffer.is_current());
        // The first frame asks for a repaint so the capture can run...
        assert!(buffer.request_capture());
        // ...and does not ask again for the same generation.
        assert!(!buffer.request_capture());

        // The same state keeps both the commit and the generation.
        buffer.begin_frame(first);
        assert_eq!(buffer.commit(), commit);
        assert!(!buffer.request_capture());

        // A new wallpaper layer changes the content: new commit, new capture.
        let mut second = state(ContextId::new());
        second.layers.push((
            7,
            CommitCounter::default(),
            Rectangle::new((0, 0).into(), (1920, 1080).into()),
        ));
        buffer.begin_frame(second);
        assert_ne!(buffer.commit(), commit);
        assert!(buffer.request_capture());
    }

    #[test]
    fn xray_marks_the_buffer_used_once_per_frame() {
        let mut buffer = XrayBuffer::new();
        buffer.begin_frame(state(ContextId::new()));
        assert!(!buffer.is_used());
        buffer.mark_used();
        assert!(buffer.is_used());
        buffer.begin_frame(state(ContextId::new()));
        assert!(!buffer.is_used());
    }

    #[test]
    fn texture_coords_map_the_destination_onto_the_source() {
        let dst = Rectangle::new((100, 50).into(), (200, 100).into());
        let src = Rectangle::new((10.0, 20.0).into(), (200.0, 100.0).into());
        let texture_size = Size::from((400, 300));
        let mat = texture_coords_to_dst(src, dst, texture_size, Transform::Normal);

        let top_left = mat.transform_point2(Vec2::new(0.0, 0.0));
        assert!((top_left.x - 10.0 / 400.0).abs() < 1e-6);
        assert!((top_left.y - 20.0 / 300.0).abs() < 1e-6);

        let bottom_right = mat.transform_point2(Vec2::new(200.0, 100.0));
        assert!((bottom_right.x - 210.0 / 400.0).abs() < 1e-6);
        assert!((bottom_right.y - 120.0 / 300.0).abs() < 1e-6);
    }

    #[test]
    fn texture_coords_scale_a_smaller_source_onto_the_destination() {
        // A source half the size of the destination: every destination pixel
        // moves half a source pixel.
        let dst = Rectangle::new((0, 0).into(), (200, 100).into());
        let src = Rectangle::new((0.0, 0.0).into(), (100.0, 50.0).into());
        let texture_size = Size::from((100, 50));
        let mat = texture_coords_to_dst(src, dst, texture_size, Transform::Normal);

        let middle = mat.transform_point2(Vec2::new(100.0, 50.0));
        assert!((middle.x - 0.5).abs() < 1e-6);
        assert!((middle.y - 0.5).abs() < 1e-6);
    }

    #[test]
    fn buffer_rects_keep_the_transformed_values() {
        let rect = Rectangle::new((5, 7).into(), (30, 40).into());
        let buffer = as_buffer_rect(rect);
        assert_eq!(buffer.loc, Point::from((5.0, 7.0)));
        assert_eq!(buffer.size, Size::from((30.0, 40.0)));
    }

    #[test]
    fn damage_is_rebased_onto_the_visible_part() {
        // The element starts 20 pixels to the right of where it is drawn and
        // sticks 10 pixels out of the output at the top.
        let dst = Rectangle::new((100, 100).into(), (50, 50).into());
        let dest = Rectangle::new((120, 90).into(), (50, 50).into());
        let damage = [Rectangle::new((0, 0).into(), (50, 50).into())];
        let clipped = clip_damage(&damage, dst, dest);
        assert_eq!(
            clipped,
            vec![Rectangle::new((0, 10).into(), (30, 40).into())]
        );
    }

    #[test]
    fn damage_outside_the_visible_part_is_dropped() {
        let dst = Rectangle::new((0, 0).into(), (50, 50).into());
        let dest = Rectangle::new((200, 0).into(), (50, 50).into());
        let damage = [Rectangle::new((0, 0).into(), (10, 10).into())];
        assert!(clip_damage(&damage, dst, dest).is_empty());
    }
}
