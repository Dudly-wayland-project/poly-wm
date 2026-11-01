// Ported from niri (GPL-3.0-or-later): src/render_helpers/blur.rs,
// src/render_helpers/framebuffer_effect.rs

//! Background blur: a framebuffer effect that captures what is behind a
//! window, runs a dual Kawase blur (or a user replacement) and composites the
//! result into the window.
//!
//! Correctness rule: the offscreen backdrop contains only
//! the elements *below* the blurred window. The blur element is therefore
//! pushed after the window in the front-to-back list, so it is drawn before it.

use std::cell::RefCell;
use std::cmp::max;
use std::rc::Rc;

use glam::{Mat3, Vec2};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Id, RenderElement};
use smithay::backend::renderer::gles::{
    GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, ffi, link_program,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet};
use smithay::backend::renderer::{
    ContextId, Frame as _, FrameContext as _, Offscreen, Renderer as _, Texture as _,
};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use tracing::warn;

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

#[derive(Debug)]
struct BlurCache {
    framebuffer: Option<GlesTexture>,
    chain: Option<BlurChain>,
    intermediate: Option<GlesTexture>,
}

/// A blurred backdrop behind one window.
pub struct BlurElement {
    id: Id,
    commit: CommitCounter,
    program: BlurProgram,
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

    fn compute_uniforms(&self, dst: Rectangle<i32, Physical>) -> [Uniform<'static>; 8] {
        let geo_size = Vec2::new(self.geometry.size.w as f32, self.geometry.size.h as f32);
        let dst_size = Vec2::new(dst.size.w as f32, dst.size.h as f32);
        let geo_loc = Vec2::new(self.geometry.loc.x as f32, self.geometry.loc.y as f32);
        let dst_loc = Vec2::new(dst.loc.x as f32, dst.loc.y as f32);

        // Drawn texture coords are `[0, 1]` over `dst`; convert them to `[0, 1]`
        // over the window geometry so the rounding matches the window.
        let dst_to_geo = Mat3::from_scale(geo_size / dst_size)
            * Mat3::from_translation((dst_loc - geo_loc) / dst_size);

        return [
            Uniform::new("poly_scale", self.scale as f32),
            Uniform::new("geo_size", (geo_size.x, geo_size.y)),
            Uniform::new("corner_radius", <[f32; 4]>::from(self.corner_radius)),
            mat3_uniform("input_to_geo", dst_to_geo),
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
        let output_rect = Rectangle::from_size(frame.output_size());
        let Some(clamped_dst) = dst.intersection(output_rect) else {
            return Ok(());
        };
        let transform = frame.transformation();

        let inner = cache.get_or_insert::<RefCell<BlurCache>, _>(|| {
            return RefCell::new(BlurCache {
                framebuffer: None,
                chain: None,
                intermediate: None,
            });
        });
        let mut inner = inner.borrow_mut();
        let inner = &mut *inner;
        inner.intermediate = None;

        let size = clamped_dst
            .size
            .to_logical(1)
            .to_buffer(1, Transform::Normal);
        if inner
            .framebuffer
            .as_ref()
            .is_some_and(|buffer| return buffer.size() != size)
        {
            inner.framebuffer = None;
        }
        let framebuffer = match inner.framebuffer.clone() {
            Some(buffer) => buffer,
            None => {
                let buffer = {
                    let mut guard = frame.renderer();
                    let renderer: &mut GlesRenderer = guard.as_mut();
                    Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, size)?
                };
                inner.framebuffer = Some(buffer.clone());
                buffer
            }
        };

        // Blit the current framebuffer content (everything below the window,
        // which has already been drawn) into our texture.
        let blit_dst = transform.transform_rect_in(clamped_dst, &output_rect.size);
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
        let passes = self.options.passes.clamp(1, 16) as usize;
        let chain = match &mut inner.chain {
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
                    &self.program,
                    &mut chain.textures,
                    &framebuffer,
                    self.options,
                );
            });
        drop(guard);

        match result {
            Ok(texture) => inner.intermediate = Some(texture),
            Err(err) => warn!("error rendering blur: {err:?}"),
        }
        return Ok(());
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
            let physical = Rectangle::new(
                Point::from((
                    (f64::from(region.loc.x) * sx).round() as i32,
                    (f64::from(region.loc.y) * sy).round() as i32,
                )),
                Size::from((
                    (f64::from(region.size.w) * sx).round() as i32,
                    (f64::from(region.size.h) * sy).round() as i32,
                )),
            );
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

        let output_rect = Rectangle::from_size(frame.output_size());
        let Some(clamped_dst) = dst.intersection(output_rect) else {
            return Ok(());
        };
        // `damage` is element-local (Smithay subtracts the element origin before
        // calling us), so it must be clamped against the visible part of the
        // element in local coordinates, not against `clamped_dst` whose
        // location is in output coordinates.
        let local_area = Rectangle::new(clamped_dst.loc - dst.loc, clamped_dst.size);
        // A client blur region narrows what is painted: `render_texture_from_to`
        // draws one instance per damage rectangle, so intersecting the damage
        // with the region is enough to leave the rest of the window untouched —
        // no scissor state and no second shader path.
        let damage = self.clip_to_regions(damage, dst);
        let damage: Vec<_> = damage
            .iter()
            .filter_map(|rect| return rect.intersection(local_area))
            .collect();
        if damage.is_empty() {
            return Ok(());
        }

        let uniforms = self.compute_uniforms(clamped_dst);
        return frame.render_texture_from_to(
            texture,
            Rectangle::from_size(texture.size().to_f64()),
            clamped_dst,
            &damage,
            &[],
            frame.transformation().invert(),
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
        if self.uses_time || commit != Some(self.commit) {
            return DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)]);
        }
        return DamageSet::default();
    }

    fn is_framebuffer_effect(&self) -> bool {
        return true;
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
