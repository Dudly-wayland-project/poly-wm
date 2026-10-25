// Ported from niri (GPL-3.0-or-later): src/render_helpers/border.rs,
// src/render_helpers/shadow.rs, src/render_helpers/blur.rs

//! Per-window decoration caching: borders, drop shadows and the blur backdrop.
//!
//! Decorations are expensive to rebuild (a fresh `PixelShaderElement` gets a
//! fresh [`Id`], which damages the whole window), so they are cached per window
//! and only rebuilt when their parameters actually change.
//!
//! Every visual change also bumps the window's *visual epoch*. The epoch
//! namespaces the id of the window's surface element, so the damage tracker
//! treats it as a new element and repaints the whole window exactly once when
//! the radius, the focus state or the cursor position (for `u_cursor`) changes
//! (`docs/plan.md` §10.5). Shader reloads bump every epoch through
//! [`Decorations::begin_frame`].

use std::collections::{HashMap, HashSet};

use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::Uniform;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::utils::{Logical, Rectangle};
use tracing::warn;

use super::blur::{BlurElement, BlurOptions, BlurProgram};
use super::elements::ShaderElement;
use super::shaders::Shaders;
use super::visual::{CornerRadius, WindowVisuals};

/// Everything the decoration cache needs to know about one window.
pub struct DecorationInputs<'a> {
    /// The window's stable id.
    pub id: u64,
    /// The drawn window rectangle, output-local logical.
    pub rect: Rectangle<i32, Logical>,
    /// Opacity times the animation alpha.
    pub alpha: f32,
    /// Whether the window holds keyboard focus.
    pub focused: bool,
    /// The resolved visuals.
    pub visuals: &'a WindowVisuals,
    /// Whether this is a compositor-managed fullscreen window (no border,
    /// shadow or blur).
    pub fullscreen: bool,
    /// Output scale.
    pub scale: f64,
    /// Seconds since startup, for `u_time`.
    pub time: f32,
    /// Pointer position in window coordinates.
    pub cursor: [f32; 2],
    /// Clip the decorations to this rectangle (output-local logical), so a
    /// window's shadow cannot paint over a neighbouring overview card.
    pub clip: Option<Rectangle<i32, Logical>>,
}

/// Normalize `clip` (output-local logical) into the window's geometry space,
/// which is what the shaders expect. Falls back to the whole window.
fn card_clip(rect: Rectangle<i32, Logical>, clip: Option<Rectangle<i32, Logical>>) -> [f32; 4] {
    let Some(clip) = clip else {
        return [0.0, 0.0, 1.0, 1.0];
    };
    if rect.size.w <= 0 || rect.size.h <= 0 {
        return [0.0, 0.0, 1.0, 1.0];
    }
    return [
        (clip.loc.x - rect.loc.x) as f32 / rect.size.w as f32,
        (clip.loc.y - rect.loc.y) as f32 / rect.size.h as f32,
        (clip.loc.x + clip.size.w - rect.loc.x) as f32 / rect.size.w as f32,
        (clip.loc.y + clip.size.h - rect.loc.y) as f32 / rect.size.h as f32,
    ];
}

/// The decorations of one window, front to back relative to each other.
pub struct WindowDecorations {
    /// Namespace for the window's surface element id.
    pub epoch: usize,
    /// Corner radii, fitted to the drawn rectangle.
    pub radius: CornerRadius,
    /// The border, if the window has one.
    pub border: Option<ShaderElement>,
    /// The drop shadow, if the window has one.
    pub shadow: Option<ShaderElement>,
    /// The blur backdrop, if the window has one.
    pub blur: Option<BlurElement>,
}

/// Border parameters, used to detect changes.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BorderKey {
    area: Rectangle<i32, Logical>,
    alpha: f32,
    color: [f32; 4],
    width: f32,
    radius: [f32; 4],
    /// Workspace card in window geometry space; see `WindowClip::card_clip`.
    card_clip: [f32; 4],
}

/// Shadow parameters, used to detect changes.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ShadowKey {
    area: Rectangle<i32, Logical>,
    alpha: f32,
    color: [f32; 4],
    sigma: f32,
    spread: f32,
    window_offset: [f32; 2],
    window_size: [f32; 2],
    window_radius: [f32; 4],
    shadow_offset: [f32; 2],
}

/// Blur parameters, used to detect changes. The program identity is compared
/// separately, because a reload can keep every parameter and still need a new
/// element.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BlurKey {
    passes: u32,
    offset_bits: u64,
    noise: f32,
    radius: [f32; 4],
    alpha: f32,
}

struct WindowDecor {
    epoch: usize,
    border: Option<(BorderKey, ShaderElement)>,
    shadow: Option<(ShadowKey, ShaderElement)>,
    blur: Option<(BlurKey, BlurProgram, Id, CommitCounter)>,
    radius: CornerRadius,
    focused: bool,
    cursor: [f32; 2],
}

impl WindowDecor {
    fn new() -> Self {
        return Self {
            epoch: 1,
            border: None,
            shadow: None,
            blur: None,
            radius: CornerRadius::default(),
            focused: false,
            cursor: [-1.0, -1.0],
        };
    }
}

/// The decoration cache of the compositor.
pub struct Decorations {
    windows: HashMap<u64, WindowDecor>,
    /// Shader generation the epochs were last bumped for.
    shader_epoch: u64,
    /// Windows already warned about, so a per-frame failure does not spam.
    warned: HashSet<u64>,
}

impl Default for Decorations {
    fn default() -> Self {
        return Self::new();
    }
}

impl Decorations {
    /// An empty cache.
    pub fn new() -> Self {
        return Self {
            windows: HashMap::new(),
            shader_epoch: 0,
            warned: HashSet::new(),
        };
    }

    /// Start a frame. When the shaders were recompiled, every window's element
    /// ids change, so the next frame repaints them all.
    pub fn begin_frame(&mut self, shader_epoch: u64) {
        if shader_epoch != self.shader_epoch {
            self.shader_epoch = shader_epoch;
            self.bump_all();
        }
    }

    /// Damage every window on the next frame.
    pub fn bump_all(&mut self) {
        for decor in self.windows.values_mut() {
            decor.epoch = decor.epoch.wrapping_add(1);
        }
    }

    /// Forget windows that no longer exist.
    pub fn prune(&mut self, alive: &HashSet<u64>) {
        self.windows.retain(|id, _| return alive.contains(id));
        self.warned.retain(|id| return alive.contains(id));
    }

    /// Build (or reuse) the decorations of one window.
    pub fn prepare(
        &mut self,
        inputs: &DecorationInputs<'_>,
        shaders: &Shaders,
    ) -> WindowDecorations {
        let entry = self
            .windows
            .entry(inputs.id)
            .or_insert_with(WindowDecor::new);

        let rect = inputs.rect;
        let visuals = inputs.visuals;
        // Corner radii are defined in the window's logical units; fitting them
        // to the drawn rectangle keeps a shrinking animation sane.
        let radius = if rect.size.w > 0 && rect.size.h > 0 {
            visuals
                .radius
                .fit_to(rect.size.w as f32, rect.size.h as f32)
        } else {
            CornerRadius::default()
        };

        if entry.radius != radius {
            entry.radius = radius;
            entry.epoch = entry.epoch.wrapping_add(1);
        }
        if entry.focused != inputs.focused {
            entry.focused = inputs.focused;
            entry.epoch = entry.epoch.wrapping_add(1);
        }

        let path = visuals.shader_path.as_deref();
        // `u_cursor` only changes when the pointer moves; bump the epoch so the
        // window is repainted exactly then.
        if shaders.window_uses_cursor(path) && entry.cursor != inputs.cursor {
            entry.cursor = inputs.cursor;
            entry.epoch = entry.epoch.wrapping_add(1);
        }

        let mut border = None;
        let mut shadow = None;
        let mut blur = None;

        if !inputs.fullscreen {
            border = Self::prepare_border(entry, inputs, shaders, radius);
            shadow = Self::prepare_shadow(entry, inputs, shaders, radius);
            blur = Self::prepare_blur(entry, inputs, shaders, radius);
        }

        return WindowDecorations {
            epoch: entry.epoch,
            radius,
            border,
            shadow,
            blur,
        };
    }

    fn prepare_border(
        entry: &mut WindowDecor,
        inputs: &DecorationInputs<'_>,
        shaders: &Shaders,
        radius: CornerRadius,
    ) -> Option<ShaderElement> {
        let program = shaders.border()?;
        let width = inputs.visuals.border_width;
        if width <= 0.0 {
            return None;
        }
        let width = width.round().max(1.0);
        let area = border_geometry(inputs.rect, width);
        let outer_radius = CornerRadius {
            top_left: radius.top_left + width,
            top_right: radius.top_right + width,
            bottom_right: radius.bottom_right + width,
            bottom_left: radius.bottom_left + width,
        }
        .fit_to(area.size.w as f32, area.size.h as f32);
        let key = BorderKey {
            area,
            alpha: inputs.alpha,
            color: inputs.visuals.border_color(inputs.focused),
            width,
            radius: <[f32; 4]>::from(outer_radius),
            card_clip: card_clip(inputs.rect, inputs.clip),
        };
        if let Some((cached, element)) = &entry.border {
            if *cached == key {
                return Some(element.clone());
            }
        }

        let uniforms = vec![
            Uniform::new("poly_scale", inputs.scale as f32),
            Uniform::new("border_color", key.color),
            Uniform::new("border_width", width),
            Uniform::new("corner_radius", key.radius),
            Uniform::new("poly_card_clip", key.card_clip),
        ];
        let element = PixelShaderElement::new(
            program.clone(),
            area,
            None,
            inputs.alpha,
            uniforms,
            smithay::backend::renderer::element::Kind::Unspecified,
        );
        entry.border = Some((key, ShaderElement::new(element.clone())));
        return Some(ShaderElement::new(element));
    }

    fn prepare_shadow(
        entry: &mut WindowDecor,
        inputs: &DecorationInputs<'_>,
        shaders: &Shaders,
        radius: CornerRadius,
    ) -> Option<ShaderElement> {
        let program = shaders.shadow()?;
        let params = inputs.visuals.shadow?;
        if params.color[3] <= 0.0 {
            return None;
        }
        let rect = inputs.rect;
        let area = inputs.visuals.shadow_rect(rect)?;
        // The shadow is clipped to the workspace card like the window itself,
        // otherwise it would bleed over the gap and the adjacent card.
        let area = match inputs.clip {
            Some(clip) => area.intersection(clip)?,
            None => area,
        };

        let key = ShadowKey {
            area,
            alpha: inputs.alpha,
            color: params.color,
            sigma: params.sigma,
            spread: params.spread,
            window_offset: [
                (rect.loc.x - area.loc.x) as f32,
                (rect.loc.y - area.loc.y) as f32,
            ],
            window_size: [rect.size.w as f32, rect.size.h as f32],
            window_radius: <[f32; 4]>::from(radius),
            shadow_offset: [params.offset_x, params.offset_y],
        };
        if let Some((cached, element)) = &entry.shadow {
            if *cached == key {
                return Some(element.clone());
            }
        }

        let uniforms = vec![
            Uniform::new("shadow_color", key.color),
            Uniform::new("sigma", key.sigma),
            Uniform::new("spread", key.spread),
            Uniform::new("poly_scale", inputs.scale as f32),
            Uniform::new("window_offset", key.window_offset),
            Uniform::new("window_size", key.window_size),
            Uniform::new("window_corner_radius", key.window_radius),
            Uniform::new("shadow_offset", key.shadow_offset),
        ];
        let element = PixelShaderElement::new(
            program.clone(),
            area,
            None,
            inputs.alpha,
            uniforms,
            smithay::backend::renderer::element::Kind::Unspecified,
        );
        entry.shadow = Some((key, ShaderElement::new(element.clone())));
        return Some(ShaderElement::new(element));
    }

    fn prepare_blur(
        entry: &mut WindowDecor,
        inputs: &DecorationInputs<'_>,
        shaders: &Shaders,
        radius: CornerRadius,
    ) -> Option<BlurElement> {
        let program = shaders.blur()?;
        let params = inputs.visuals.blur?;
        if inputs.rect.size.w <= 0 || inputs.rect.size.h <= 0 {
            return None;
        }

        let key = BlurKey {
            passes: params.passes,
            offset_bits: params.offset.to_bits(),
            noise: params.noise,
            radius: <[f32; 4]>::from(radius),
            alpha: inputs.alpha,
        };
        let unchanged = match &entry.blur {
            Some((cached, cached_program, _, _)) => {
                *cached == key && cached_program.ptr_eq(program)
            }
            None => false,
        };
        if !unchanged {
            // Parameters changed: bump the commit counter so the damage
            // tracker repaints the blurred area. A new element id is only
            // needed the first time; afterwards the (stable) id plus the new
            // commit is enough.
            let (id, commit) = match &entry.blur {
                Some((_, _, id, commit)) => {
                    let mut commit = *commit;
                    commit.increment();
                    (id.clone(), commit)
                }
                None => (Id::new(), CommitCounter::default()),
            };
            entry.blur = Some((key, program.clone(), id, commit));
        }

        let (_, _, id, commit) = entry.blur.as_ref()?;
        return Some(BlurElement::new(
            id.clone(),
            *commit,
            program.clone(),
            inputs.rect.to_f64(),
            radius,
            inputs.scale,
            BlurOptions {
                passes: params.passes,
                offset: params.offset,
            },
            params.noise,
            inputs.alpha,
            program.uses_time(),
            inputs.time,
        ));
    }

    /// Warn once per window about a visual feature that could not be applied.
    pub fn warn_once(&mut self, id: u64, message: &str) {
        if self.warned.insert(id) {
            warn!("{message}");
        }
    }
}

/// The window rectangle expanded by its border, as the border element sees it.
pub fn border_geometry(rect: Rectangle<i32, Logical>, width: f32) -> Rectangle<i32, Logical> {
    let width = width.round().max(0.0) as i32;
    return Rectangle::new(
        (rect.loc.x - width, rect.loc.y - width).into(),
        (rect.size.w + 2 * width, rect.size.h + 2 * width).into(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_geometry_grows_on_every_side() {
        let rect = Rectangle::new((10, 20).into(), (100, 50).into());
        assert_eq!(
            border_geometry(rect, 2.0),
            Rectangle::new((8, 18).into(), (104, 54).into())
        );
        assert_eq!(border_geometry(rect, 0.0), rect);
    }
}
