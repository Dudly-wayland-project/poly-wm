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
use std::rc::Rc;

use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::gles::Uniform;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::utils::{Logical, Point, Rectangle, Size};
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
    /// Blur region the client asked for (`ext-background-effect-v1`), in
    /// output-local logical coordinates.
    ///
    /// `None` means the client never set a region, so the configured
    /// whole-window backdrop is drawn. `Some` means it did — including the
    /// empty slice a client uses to switch the blur off, which must not be
    /// confused with "no opinion".
    pub blur_region: Option<&'a [Rectangle<i32, Logical>]>,
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

/// Past this many rectangles a client blur region is drawn as its bounding box.
///
/// Every rectangle becomes a draw instance in the blur element, and normalising
/// overlapping rectangles into disjoint ones can split them further. A region a
/// client fragmented into hundreds of pieces is not worth that.
const MAX_CLIENT_BLUR_REGIONS: usize = 16;

/// What a window's backdrop has to cover.
///
/// The three cases are distinct on purpose: a client that never touched
/// `ext-background-effect-v1` keeps the configured whole-window blur, while one
/// that committed an empty region asked for *no* blur — a panel that hides its
/// blur by clearing the region, not by destroying the object.
#[derive(Debug, Clone, PartialEq)]
enum BlurArea {
    /// No client region: the configured whole-window backdrop.
    Whole,
    /// The client asked for nothing visible.
    Nothing,
    /// Disjoint element-local rectangles.
    Regions(Rc<[Rectangle<i32, Logical>]>),
}

/// Turn the client's blur region into element-local rectangles.
///
/// The region arrives in output-local logical coordinates (the renderer mapped
/// it out of surface space), so it is clipped to the drawn window and shifted
/// by the window origin.
///
/// The result is disjoint: the blur element draws one instance per damage
/// rectangle, so two overlapping rectangles would composite the backdrop twice
/// and darken the overlap wherever the window is translucent.
fn client_regions(inputs: &DecorationInputs<'_>) -> BlurArea {
    let Some(blur_region) = inputs.blur_region else {
        return BlurArea::Whole;
    };
    if blur_region.is_empty() || inputs.rect.size.w <= 0 || inputs.rect.size.h <= 0 {
        return BlurArea::Nothing;
    }
    let window = inputs.rect;
    let mut clipped: Vec<Rectangle<i32, Logical>> = Vec::with_capacity(blur_region.len());
    for region in blur_region {
        let Some(inside) = region.intersection(window) else {
            continue;
        };
        clipped.push(Rectangle::new(inside.loc - window.loc, inside.size));
    }
    if clipped.is_empty() {
        return BlurArea::Nothing;
    }
    if clipped.len() > MAX_CLIENT_BLUR_REGIONS {
        clipped = bounding_box(&clipped).into_iter().collect();
    }

    let disjoint = disjoint_pieces(clipped);
    if disjoint.is_empty() {
        return BlurArea::Nothing;
    }
    return BlurArea::Regions(Rc::from(disjoint));
}

/// Express a list of rectangles as disjoint pieces.
///
/// The blur element draws one instance per damage rectangle, so two
/// overlapping rectangles would composite the backdrop twice and darken the
/// overlap wherever the surface is translucent. Past
/// [`MAX_CLIENT_BLUR_REGIONS`] pieces the union is drawn as its bounding box.
pub fn disjoint_pieces(rects: Vec<Rectangle<i32, Logical>>) -> Vec<Rectangle<i32, Logical>> {
    // Peel every rectangle against the ones already accepted, so the union is
    // expressed as disjoint pieces.
    let mut disjoint: Vec<Rectangle<i32, Logical>> = Vec::with_capacity(rects.len());
    for rect in rects {
        let pieces = Rectangle::subtract_rects_many_in_place(vec![rect], disjoint.iter().copied());
        disjoint.extend(pieces);
    }
    if disjoint.len() > MAX_CLIENT_BLUR_REGIONS {
        return bounding_box(&disjoint).into_iter().collect();
    }
    return disjoint;
}

/// The smallest rectangle containing all of `rects`.
fn bounding_box(rects: &[Rectangle<i32, Logical>]) -> Option<Rectangle<i32, Logical>> {
    let first = *rects.first()?;
    let mut min_x = first.loc.x;
    let mut min_y = first.loc.y;
    let mut max_x = first.loc.x + first.size.w;
    let mut max_y = first.loc.y + first.size.h;
    for rect in &rects[1..] {
        min_x = min_x.min(rect.loc.x);
        min_y = min_y.min(rect.loc.y);
        max_x = max_x.max(rect.loc.x + rect.size.w);
        max_y = max_y.max(rect.loc.y + rect.size.h);
    }
    return Some(Rectangle::new(
        Point::from((min_x, min_y)),
        Size::from((max_x - min_x, max_y - min_y)),
    ));
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
#[derive(Debug, Clone, PartialEq)]
struct BlurKey {
    passes: u32,
    offset_bits: u64,
    noise: f32,
    radius: [f32; 4],
    alpha: f32,
    /// Element-local rectangles, or `None` for the whole window. Compared by
    /// content, so a region that did not change keeps the same commit counter
    /// and does not damage the window.
    regions: Option<Rc<[Rectangle<i32, Logical>]>>,
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

        let regions = match client_regions(inputs) {
            BlurArea::Nothing => {
                // The client switched the backdrop off. Drop the cached element
                // so a later region starts from a fresh id instead of resuming
                // a commit counter the damage tracker last saw at zero area.
                entry.blur = None;
                return None;
            }
            BlurArea::Whole => None,
            BlurArea::Regions(regions) => Some(regions),
        };

        let key = BlurKey {
            passes: params.passes,
            offset_bits: params.offset.to_bits(),
            noise: params.noise,
            radius: <[f32; 4]>::from(radius),
            alpha: inputs.alpha,
            regions,
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

        let (cached, _, id, commit) = entry.blur.as_ref()?;
        return Some(BlurElement::new(
            id.clone(),
            *commit,
            program.clone(),
            inputs.rect.to_f64(),
            cached.regions.clone(),
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

    fn inputs<'a>(
        rect: Rectangle<i32, Logical>,
        visuals: &'a WindowVisuals,
        blur_region: Option<&'a [Rectangle<i32, Logical>]>,
    ) -> DecorationInputs<'a> {
        return DecorationInputs {
            id: 1,
            rect,
            alpha: 1.0,
            focused: false,
            visuals,
            fullscreen: false,
            scale: 1.0,
            time: 0.0,
            cursor: [-1.0, -1.0],
            clip: None,
            blur_region,
        };
    }

    #[test]
    fn client_regions_are_clipped_to_the_window_and_made_element_local() {
        let visuals = WindowVisuals::default();
        let region = [Rectangle::new((90, 30).into(), (100, 100).into())];
        let inputs = inputs(
            Rectangle::new((100, 50).into(), (400, 300).into()),
            &visuals,
            Some(&region),
        );

        let regions = match client_regions(&inputs) {
            BlurArea::Regions(regions) => regions,
            other => panic!("the region lies inside the window, got {other:?}"),
        };
        assert_eq!(&*regions, &[Rectangle::new((0, 0).into(), (90, 80).into())]);
    }

    #[test]
    fn no_client_region_means_the_whole_window() {
        let visuals = WindowVisuals::default();
        let inputs = inputs(
            Rectangle::new((100, 50).into(), (400, 300).into()),
            &visuals,
            None,
        );
        assert_eq!(client_regions(&inputs), BlurArea::Whole);
    }

    #[test]
    fn an_empty_client_region_means_no_blur_at_all() {
        let visuals = WindowVisuals::default();
        let inputs = inputs(
            Rectangle::new((100, 50).into(), (400, 300).into()),
            &visuals,
            Some(&[]),
        );
        assert_eq!(client_regions(&inputs), BlurArea::Nothing);
    }

    #[test]
    fn a_client_region_outside_the_window_blurs_nothing() {
        let visuals = WindowVisuals::default();
        // Entirely to the right of the window: the client asked for a backdrop
        // it does not cover, which must not fall back to the whole window.
        let region = [Rectangle::new((600, 50).into(), (100, 100).into())];
        let inputs = inputs(
            Rectangle::new((100, 50).into(), (400, 300).into()),
            &visuals,
            Some(&region),
        );
        assert_eq!(client_regions(&inputs), BlurArea::Nothing);
    }

    fn area(rects: &[Rectangle<i32, Logical>]) -> i64 {
        return rects
            .iter()
            .map(|rect| return i64::from(rect.size.w) * i64::from(rect.size.h))
            .sum();
    }

    #[test]
    fn overlapping_client_regions_are_split_so_nothing_is_painted_twice() {
        let visuals = WindowVisuals::default();
        // Two 100x100 squares overlapping in a 50x50 corner.
        let region = [
            Rectangle::new((100, 50).into(), (100, 100).into()),
            Rectangle::new((150, 100).into(), (100, 100).into()),
        ];
        let inputs = inputs(
            Rectangle::new((100, 50).into(), (400, 300).into()),
            &visuals,
            Some(&region),
        );

        let regions = match client_regions(&inputs) {
            BlurArea::Regions(regions) => regions,
            other => panic!("the union is not empty, got {other:?}"),
        };
        // Counting the union once is exactly what keeping the pieces disjoint
        // means: 100*100 + 100*100 - 50*50.
        assert_eq!(area(&regions), 17_500);
        assert!(
            regions.len() > 1,
            "the overlap has to be split off, got {regions:?}"
        );
    }
}
