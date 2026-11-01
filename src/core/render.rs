// Ported from niri (GPL-3.0-or-later): src/backend/mod.rs, src/render_helpers/mod.rs,
// src/render_helpers/render_elements.rs

//! The shared rendering pipeline: what gets drawn on an output.
//!
//! The element list is ordered front to back (Smithay's damage tracker draws it
//! in reverse). Per output that is: cursor, Overlay layers, Toasts, Top layers,
//! fullscreen windows, floating windows, the dragged window, the tiled windows,
//! Bottom/Background layers. A window itself contributes its popups, its
//! surfaces, its border, its shadow and its blur backdrop, in that order.
//!
//! The workspace overview follows niri: the cards are
//! opaque plates of the zoomed workspace, the active one centred with its
//! neighbours peeking in above and below, a soft shadow behind the stack and an
//! opaque backdrop around it. Windows keep their rounding, border and shadow,
//! scaled with the card; blur and popups are dropped there. Everything lands
//! behind the windows but in front of the Bottom/Background layers, so the
//! wallpaper is covered while the overview is open (niri crops it into the
//! cards instead).
//!
//! Every visual feature is optional and off by default: with the default
//! configuration a window is a plain, unclipped surface.

mod blur;
mod decorations;
mod elements;
mod shaders;
pub mod visual;

pub use decorations::Decorations;
pub use elements::{
    AsGlesRenderer, OutputElements, PolyRenderer, ShaderElement, WindowClip, WindowElement,
    WindowUniforms,
};
pub use shaders::Shaders;

use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::NamespacedElement;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture, Uniform};
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::backend::renderer::{Bind, Color32F, Frame, Offscreen, Renderer};
use smithay::desktop::utils::{
    OutputPresentationFeedback, send_frames_surface_tree,
    surface_presentation_feedback_flags_from_states, take_presentation_feedback_surface_tree,
};
use smithay::desktop::{PopupManager, layer_map_for_output};
use smithay::input::pointer::CursorImageStatus;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::background_effect::BackgroundEffectSurfaceCachedState;
use smithay::wayland::compositor::{RectangleKind, with_states};
use smithay::wayland::shell::wlr_layer::Layer;
use tracing::{debug, warn};

use crate::core::Core;
use crate::core::DragHintBuffers;
use crate::core::OverviewBuffers;
use crate::core::capture::{CaptureKey, CaptureOutcome, CaptureState};
use crate::core::cursor::{buffer_from_image, cursor_hotspot, is_dead_cursor_surface};
use crate::core::layout::FullscreenMode;
use crate::core::layout::LayoutManager;
use crate::core::layout::WorkspaceView;

use self::blur::{BlurElement, BlurOptions};
use self::decorations::DecorationInputs;
use self::visual::{CornerRadius, WindowVisuals};

/// Clients get at most one frame callback per this interval, even if we render
/// faster than the output refreshes.
const FRAME_CALLBACK_THROTTLE: Option<Duration> = Some(Duration::from_millis(995));

/// Build the elements for one frame of `output`: space contents plus cursor.
///
/// This is also where the shader programs are (re)compiled and where compile
/// errors are turned into Toasts: the GL context is only valid here.
pub fn output_elements<R>(
    core: &mut Core,
    renderer: &mut R,
    output: &Output,
    include_cursor: bool,
) -> Vec<OutputElements<R>>
where
    R: PolyRenderer,
    R::TextureId: Clone + Send + 'static,
{
    // Cursor capture sessions are pull-shaped: the pointer position, the
    // hotspot and the sprite size are only read when the compositor says so.
    crate::core::handlers::refresh_cursor_sessions(core);

    // Shaders are compiled lazily, on the first frame and after a reload or an
    // edit of a shader file. A failing file keeps
    // the previous program and reports through a Toast.
    core.shaders.prepare(
        &core.config,
        core.config_generation,
        renderer.as_gles_renderer(),
    );
    let errors = core.shaders.take_errors();
    if !errors.is_empty() {
        let now = Instant::now();
        for error in errors {
            core.toasts.push(&error, now);
        }
    }
    // A shader reading `u_time` changes on every frame, so the damage
    // optimization is off and the event loop keeps queueing redraws.
    core.continuous_redraw = core.shaders.uses_time();

    // A recompile changes the element ids of every decoration, so announce the
    // new shader epoch before the caches are used, and drop the state of
    // windows that no longer exist.
    let shader_epoch = core.shaders.epoch();
    core.decorations.begin_frame(shader_epoch);
    let alive = core.layout.window_ids();
    core.decorations.prune(&alive);

    let mut elements: Vec<OutputElements<R>> = Vec::new();

    // Render elements are ordered front to back. The cursor belongs on top of
    // everything, so it is pushed first and the space (windows) below it;
    // drawing them the other way around would hide the cursor behind windows.
    if include_cursor {
        push_cursor(&mut elements, core, renderer, output);
    }

    // While locked, nothing but the lock surface is visible. The lock client
    // draws its own background, so there is nothing else to render.
    if core.is_locked() {
        if let Some(lock_surface) = core.lock_surface_for_output(output) {
            let lock_surface = lock_surface.wl_surface().clone();
            let scale = output.current_scale().fractional_scale();
            let lock_elements =
                render_elements_from_surface_tree::<R, WaylandSurfaceRenderElement<R>>(
                    renderer,
                    &lock_surface,
                    (0, 0),
                    scale,
                    1.0,
                    Kind::Unspecified,
                );
            elements.extend(lock_elements.into_iter().map(OutputElements::Surface));
        }

        // A solid background below the lock surface (niri does the same). It
        // guarantees that the locked frame replaces the session content even
        // before the client attaches its first buffer, and gives the redraw
        // loop a frame to present before the lock is confirmed.
        let size = core
            .space
            .output_geometry(output)
            .map(|geometry| return geometry.size)
            .unwrap_or_default();
        let color = [0.0f32, 0.0, 0.0, 1.0];
        let background = core
            .lock_backgrounds
            .entry(output.clone())
            .or_insert_with(|| return SolidColorBuffer::new(size, color));
        background.update(size, color);
        elements.push(OutputElements::Solid(SolidColorRenderElement::from_buffer(
            background,
            (0, 0),
            output.current_scale().fractional_scale(),
            1.0,
            Kind::Unspecified,
        )));

        return elements;
    }

    // Build the element list ourselves instead of using
    // `space_render_elements`: fullscreen windows must be able to cover Top
    // layers, and a stack with two outputs needs a stable per-output order.
    let Some(output_geometry) = core.space.output_geometry(output) else {
        return elements;
    };
    let scale = output.current_scale().fractional_scale();
    let now = Instant::now();
    let time = core.start_time.elapsed().as_secs_f32();

    // Snapshot the windows so rendering does not hold a borrow of the layout
    // while the fullscreen backgrounds and the decoration cache are updated.
    let view = core.layout.view(output, output_geometry, now);
    let overview_zoom = (view.overview > 0.0).then_some(view.zoom);
    let overview = overview_zoom.is_some();
    let mut fullscreen = Vec::new();
    let mut floating = Vec::new();
    let mut dragging = Vec::new();
    let mut tiled = Vec::new();
    for placed in core.layout.windows_on_output(output, output_geometry, now) {
        let managed = placed.managed;
        // Workspaces that are off screen (or cards that the current zoom keeps
        // outside the output) cost nothing. During a slide both cards overlap
        // the output and are drawn.
        if placed.rect.intersection(output_geometry).is_none() {
            continue;
        }
        let open_scale = managed.open_scale_at(now);
        // `placed.rect` is the workspace/overview projection of the layout
        // rectangle. Do not use `managed.open_rect_at` here: that is still in
        // layout coordinates, and would draw every workspace (and the overview)
        // at the active card's position. Scale the projected rectangle around
        // its own centre for the opening animation instead.
        let rect = scaled_rect(placed.rect, open_scale);
        let entry = RenderWindow {
            id: managed.id,
            window: managed.window.clone(),
            root: managed.root.clone(),
            rect,
            card: placed.card,
            // Window opacity multiplies the open/close animation alpha.
            alpha: managed.alpha_at(now) * managed.visuals.opacity,
            scale: open_scale,
            fullscreen: managed.fullscreen,
            reordering: managed.tiled && LayoutManager::is_reordering(managed),
            visuals: managed.visuals.clone(),
        };
        match managed.fullscreen {
            Some(FullscreenMode::Fullscreen) => fullscreen.push(entry),
            // A reordering tiled window follows the pointer and belongs on top
            // of the regular tiled stack, like a floating window.
            _ if entry.reordering => dragging.push(entry),
            _ if managed.tiled => tiled.push(entry),
            _ => floating.push(entry),
        }
    }

    // Front to back. The cursor is already first. Overlay layers always stay
    // above a fullscreen window; Top layers can be covered by it.
    push_layers(
        &mut elements,
        core,
        renderer,
        output,
        &[Layer::Overlay],
        scale,
        time,
    );
    push_toasts(&mut elements, core, renderer, output, scale);
    // In the overview a fullscreen window is just another card, so it must not
    // cover the Top layer (the bar stays visible above the cards).
    let cover_top = !overview && core.config.fullscreen.cover_top_layer && !fullscreen.is_empty();
    if !cover_top {
        push_layers(
            &mut elements,
            core,
            renderer,
            output,
            &[Layer::Top],
            scale,
            time,
        );
    }

    // In the overview a compositor-managed fullscreen window is just another
    // card-sized window, so the black frame behind it is not drawn (it would
    // cover the card background anyway).
    // Closing windows are drawn above their former stacking group for the
    // duration of the fade. The fullscreen backdrop is pushed after this, so it
    // stays behind the closing snapshot.
    push_closing(
        &mut elements,
        core,
        output,
        output_geometry,
        &view,
        scale,
        ClosingGroup::Fullscreen,
        now,
    );
    for managed in fullscreen.iter().rev() {
        push_window(
            &mut elements,
            core,
            renderer,
            managed,
            output_geometry,
            scale,
            time,
            overview_zoom,
            false,
        );
        if overview {
            continue;
        }
        let buffer = core
            .fullscreen_backgrounds
            .entry(output.clone())
            .or_insert_with(|| {
                return SolidColorBuffer::new(output_geometry.size, [0.0, 0.0, 0.0, 1.0]);
            });
        buffer.update(output_geometry.size, [0.0, 0.0, 0.0, 1.0]);
        elements.push(OutputElements::Solid(SolidColorRenderElement::from_buffer(
            buffer,
            (0, 0),
            scale,
            1.0,
            Kind::Unspecified,
        )));
    }

    if cover_top {
        push_layers(
            &mut elements,
            core,
            renderer,
            output,
            &[Layer::Top],
            scale,
            time,
        );
    }
    push_closing(
        &mut elements,
        core,
        output,
        output_geometry,
        &view,
        scale,
        ClosingGroup::Floating,
        now,
    );
    for managed in floating.iter().rev() {
        push_window(
            &mut elements,
            core,
            renderer,
            managed,
            output_geometry,
            scale,
            time,
            overview_zoom,
            false,
        );
    }
    for managed in dragging.iter().rev() {
        push_window(
            &mut elements,
            core,
            renderer,
            managed,
            output_geometry,
            scale,
            time,
            overview_zoom,
            false,
        );
    }
    // Niri-style drop indicator: the slot the dragged tiled window will land
    // in, drawn above the tiled windows but below the dragged one. It is not
    // meaningful while the overview is open.
    if !overview {
        push_drag_hint(&mut elements, core, output, output_geometry, scale);
    }
    push_closing(
        &mut elements,
        core,
        output,
        output_geometry,
        &view,
        scale,
        ClosingGroup::Tiled,
        now,
    );
    for managed in tiled.iter().rev() {
        push_window(
            &mut elements,
            core,
            renderer,
            managed,
            output_geometry,
            scale,
            time,
            overview_zoom,
            false,
        );
    }
    if overview {
        // Cards, card shadows and the backdrop go behind the windows. Each
        // card also gets a scaled copy of the Bottom/Background layers, so the
        // wallpaper is visible inside the thumbnails like it is in niri.
        push_overview(
            &mut elements,
            core,
            renderer,
            output,
            output_geometry,
            &view,
            scale,
        );
    }
    push_layers(
        &mut elements,
        core,
        renderer,
        output,
        &[Layer::Bottom, Layer::Background],
        scale,
        time,
    );

    return elements;
}

/// A cached card shadow element.
///
/// Rebuilding the `PixelShaderElement` every frame would give it a fresh `Id`,
/// so the damage tracker would consider the whole card stack new on every
/// frame — a pure cursor move would repaint every card. The key covers every
/// uniform and the shader generation.
#[derive(Debug)]
pub struct CardShadowCache {
    key: CardShadowKey,
    element: ShaderElement,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CardShadowKey {
    area: Rectangle<i32, Logical>,
    window_offset: [f32; 2],
    window_size: [f32; 2],
    sigma: f32,
    spread: f32,
    offset: [f32; 2],
    color: [f32; 4],
    alpha: f32,
    scale: f32,
    epoch: u64,
}

/// Draw the `layers` (already positioned in output space) a second time,
/// scaled around the card's top-left and cropped to the card.
///
/// This puts the wallpaper (and any Bottom layer) inside the overview cards,
/// which is what niri does: the workspace is a scaled copy of the output, so
/// everything that is not a window scales with it.
#[allow(clippy::too_many_arguments)]
fn push_card_layers<R>(
    elements: &mut Vec<OutputElements<R>>,
    renderer: &mut R,
    output: &Output,
    layers: &[Layer],
    scale: f64,
    card: Rectangle<i32, Logical>,
    namespace: usize,
    zoom: f64,
) where
    R: PolyRenderer,
    R::TextureId: Clone + Send + 'static,
{
    if card.size.w <= 0 || card.size.h <= 0 || zoom <= 0.0 {
        return;
    }
    let origin = card.loc.to_f64().to_physical_precise_round(scale);
    let crop = card.to_f64().to_physical_precise_round(scale);
    let map = layer_map_for_output(output);
    for layer in layers {
        for surface in map.layers_on(*layer).rev() {
            let Some(geometry) = map.layer_geometry(surface) else {
                continue;
            };
            let location = Point::<i32, Logical>::from((geometry.loc.x, geometry.loc.y))
                .to_physical_precise_round(scale);
            let rendered = render_elements_from_surface_tree::<R, WaylandSurfaceRenderElement<R>>(
                renderer,
                surface.wl_surface(),
                location,
                Scale::from(scale),
                1.0,
                Kind::Unspecified,
            );
            for element in rendered {
                let scaled = RescaleRenderElement::from_element(element, origin, zoom);
                // A layer surface may be larger than or offset from the
                // output; the card crop keeps the thumbnail clean.
                let Some(cropped) = CropRenderElement::from_element(scaled, scale, crop) else {
                    continue;
                };
                elements.push(OutputElements::Card(NamespacedElement::new(
                    cropped, namespace,
                )));
            }
        }
    }
}

/// Add the workspace cards, their shadows and the backdrop of the overview.
///
/// This mirrors niri's `Monitor::render_workspaces` / `render_workspace_shadows`
/// order: every card is an opaque plate the size of the
/// zoomed workspace, the whole stack casts a soft shadow, and an opaque
/// backdrop fills the desktop around it. Everything is pushed after the
/// windows, i.e. *behind* them, so the cards read as backgrounds. Inside every
/// card the Bottom/Background layer surfaces are drawn again, scaled and
/// cropped, so the wallpaper shows in the thumbnails (the originals stay
/// behind the backdrop, which fades in over them).
fn push_overview<R>(
    elements: &mut Vec<OutputElements<R>>,
    core: &mut Core,
    renderer: &mut R,
    output: &Output,
    output_geometry: Rectangle<i32, Logical>,
    view: &crate::core::layout::WorkspaceView,
    scale: f64,
) where
    R: PolyRenderer,
    R::TextureId: Clone + Send + 'static,
{
    // Fade the plates in with the zoom: at the first frame they still cover
    // the whole screen and would otherwise pop.
    let fade = view.overview.clamp(0.0, 1.0) as f32;
    if fade <= 0.001 {
        return;
    }
    let config = core.config.overview.clone();
    // niri forces the backdrop opaque so the desktop behind the cards never
    // bleeds through.
    let mut backdrop_color = config.backdrop_color.to_floats();
    backdrop_color[3] = 1.0;
    let card_color = config.card_color.to_floats();
    // Clone the program up front: the buffer borrow below is mutable.
    let shadow_program = core.shaders.shadow().cloned();
    let shader_epoch = core.shaders.epoch();

    let buffers = core
        .overview_buffers
        .entry(output.clone())
        .or_insert_with(OverviewBuffers::new);

    for card in view.cards.iter().rev() {
        let local = Rectangle::new(card.rect.loc - output_geometry.loc, card.rect.size);
        if local.size.w <= 0 || local.size.h <= 0 {
            continue;
        }
        // Cards pushed off the output by the zoom (or by a long workspace
        // stack) cost nothing: the damage tracker culls them anyway, and
        // allocating a buffer for each of them would waste memory.
        if local
            .intersection(Rectangle::from_size(output_geometry.size))
            .is_none()
        {
            continue;
        }
        // Bottom/Background layers belong to the output, not to a workspace;
        // repeat them scaled into every card so the wallpaper is visible in
        // the thumbnail (niri draws its per-workspace background there too).
        push_card_layers(
            elements,
            renderer,
            output,
            &[Layer::Bottom, Layer::Background],
            scale,
            local,
            card.index,
            view.zoom,
        );
        let buffer = buffers.card_mut(card.index);
        buffer.update(local.size, card_color);
        elements.push(OutputElements::Solid(SolidColorRenderElement::from_buffer(
            buffer,
            local.loc.to_physical_precise_round(scale),
            scale,
            fade,
            Kind::Unspecified,
        )));
    }

    // Card shadows come after (behind) the cards, like niri: the shadow is
    // drawn around the plate, never on top of it.
    if let Some(program) = shadow_program {
        let shadow_color = config.shadow.color.to_floats();
        if config.shadow.enable && shadow_color[3] > 0.0 {
            // niri normalizes the workspace shadow to 1080p and then scales it
            // with the card (`compute_workspace_shadow_config`).
            let norm = f64::from(output_geometry.size.h) / 1080.0 * view.zoom;
            let sigma = (config.shadow.blur * 0.5 * norm) as f32;
            let spread = (config.shadow.spread * norm) as f32;
            let offset_x = (f64::from(config.shadow.offset_x) * norm).round() as i32;
            let offset_y = (f64::from(config.shadow.offset_y) * norm).round() as i32;
            for card in view.cards.iter().rev() {
                let local = Rectangle::new(card.rect.loc - output_geometry.loc, card.rect.size);
                if local
                    .intersection(Rectangle::from_size(output_geometry.size))
                    .is_none()
                {
                    continue;
                }
                let Some(area) =
                    visual::shadow_rect(local, sigma, spread.round() as i32, offset_x, offset_y)
                else {
                    continue;
                };
                let key = CardShadowKey {
                    area,
                    window_offset: [
                        (local.loc.x - area.loc.x) as f32,
                        (local.loc.y - area.loc.y) as f32,
                    ],
                    window_size: [local.size.w as f32, local.size.h as f32],
                    sigma,
                    spread,
                    offset: [offset_x as f32, offset_y as f32],
                    color: shadow_color,
                    alpha: fade,
                    scale: scale as f32,
                    epoch: shader_epoch,
                };
                let build = || {
                    let uniforms = vec![
                        Uniform::new("shadow_color", shadow_color),
                        Uniform::new("sigma", sigma),
                        Uniform::new("spread", spread),
                        Uniform::new("poly_scale", scale as f32),
                        Uniform::new("window_offset", key.window_offset),
                        Uniform::new("window_size", key.window_size),
                        Uniform::new("window_corner_radius", [0.0_f32; 4]),
                        Uniform::new("shadow_offset", key.offset),
                    ];
                    return ShaderElement::new(PixelShaderElement::new(
                        program.clone(),
                        area,
                        None,
                        fade,
                        uniforms,
                        Kind::Unspecified,
                    ));
                };
                let slot = buffers.shadow_mut(card.index);
                let cached = slot
                    .as_ref()
                    .filter(|cached| return cached.key == key)
                    .map(|cached| return cached.element.clone());
                let element = match cached {
                    Some(element) => element,
                    None => {
                        let element = build();
                        *slot = Some(CardShadowCache {
                            key,
                            element: element.clone(),
                        });
                        element
                    }
                };
                elements.push(OutputElements::Shader(element));
            }
        }
    }

    let backdrop = buffers.backdrop_mut();
    backdrop.update(output_geometry.size, backdrop_color);
    elements.push(OutputElements::Solid(SolidColorRenderElement::from_buffer(
        backdrop,
        (0, 0),
        scale,
        fade,
        Kind::Unspecified,
    )));
}

/// Scale a rectangle around its centre, rounding to whole logical pixels.
fn scaled_rect(rect: Rectangle<i32, Logical>, scale: f64) -> Rectangle<i32, Logical> {
    if scale >= 0.9999 {
        return rect;
    }
    let size = rect.size.to_f64() * scale;
    let centre = rect.loc.to_f64() + rect.size.to_f64() / 2.0;
    let loc = Point::from((centre.x - size.w / 2.0, centre.y - size.h / 2.0));
    return Rectangle::new(loc.to_i32_round(), size.to_i32_round());
}

/// A window snapshot used while building one output's element list.
struct RenderWindow {
    id: u64,
    window: smithay::desktop::Window,
    root: WlSurface,
    /// The drawn rectangle, in compositor coordinates.
    rect: Rectangle<i32, Logical>,
    /// The workspace card the window belongs to, in compositor coordinates.
    card: Rectangle<i32, Logical>,
    /// Opacity times the animation alpha.
    alpha: f32,
    /// Content scale of the opening animation (`1.0` when settled).
    scale: f64,
    fullscreen: Option<FullscreenMode>,
    /// Tiled window currently following the pointer.
    reordering: bool,
    visuals: WindowVisuals,
}

/// Add the niri-style tiled drag drop indicator: a translucent slot without a
/// border. The dragged window is rendered above it.
fn push_drag_hint<R>(
    elements: &mut Vec<OutputElements<R>>,
    core: &mut Core,
    output: &Output,
    output_geometry: Rectangle<i32, Logical>,
    scale: f64,
) where
    R: PolyRenderer,
    R::TextureId: Clone + Send + 'static,
{
    let Some((workspace, rect)) = core.layout.drag_hint_for(output) else {
        return;
    };
    // `rect` is workspace-local; project it exactly like the windows.
    let now = Instant::now();
    let view = core.layout.view(output, output_geometry, now);
    let Some(rect) = view.project(workspace, rect) else {
        return;
    };
    let local = Rectangle::new(rect.loc - output_geometry.loc, rect.size);
    if local.size.w <= 0 || local.size.h <= 0 {
        return;
    }
    let buffers = core
        .drag_hint_buffers
        .get_or_insert_with(DragHintBuffers::new);
    let fill = [0.36f32, 0.58, 1.0, 0.22];
    let fill_buffer = buffers.fill_mut();
    fill_buffer.update(local.size, fill);

    let location: Point<i32, smithay::utils::Physical> = local.loc.to_physical_precise_round(scale);
    elements.push(OutputElements::Solid(SolidColorRenderElement::from_buffer(
        &*fill_buffer,
        location,
        scale,
        1.0,
        Kind::Unspecified,
    )));
}

/// The workspace card in window-geometry space (`x0, y0, x1, y1`), or `None`
/// when the card fully contains the window.
///
/// The shader discards fragments outside this rectangle, so a floating window
/// cannot paint over the gap or the next overview card.
fn card_clip(rect: Rectangle<i32, Logical>, card: Rectangle<i32, Logical>) -> Option<[f32; 4]> {
    if rect.size.w <= 0 || rect.size.h <= 0 {
        return None;
    }
    let x0 = (card.loc.x - rect.loc.x) as f32 / rect.size.w as f32;
    let y0 = (card.loc.y - rect.loc.y) as f32 / rect.size.h as f32;
    let x1 = (card.loc.x + card.size.w - rect.loc.x) as f32 / rect.size.w as f32;
    let y1 = (card.loc.y + card.size.h - rect.loc.y) as f32 / rect.size.h as f32;
    if x0 <= 0.0 && y0 <= 0.0 && x1 >= 1.0 && y1 >= 1.0 {
        return None;
    }
    return Some([x0, y0, x1, y1]);
}

/// Which stacking group a closing window belonged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosingGroup {
    Fullscreen,
    Floating,
    Tiled,
}

/// Push the baked snapshots of closed windows of one stacking group.
///
/// The snapshot is scaled around the window centre while it fades, exactly like
/// niri's closing animation, and cropped to its workspace card so it cannot
/// paint over a neighbouring card in the overview.
#[allow(clippy::too_many_arguments)]
fn push_closing<R>(
    elements: &mut Vec<OutputElements<R>>,
    core: &Core,
    output: &Output,
    output_geometry: Rectangle<i32, Logical>,
    view: &WorkspaceView,
    scale: f64,
    group: ClosingGroup,
    now: Instant,
) where
    R: PolyRenderer,
    R::TextureId: Clone + Send + 'static,
{
    for closing in core.layout.closing_windows() {
        if &closing.output != output {
            continue;
        }
        let belongs = match group {
            ClosingGroup::Fullscreen => closing.fullscreen,
            ClosingGroup::Floating => !closing.fullscreen && !closing.tiled,
            ClosingGroup::Tiled => !closing.fullscreen && closing.tiled,
        };
        if !belongs {
            continue;
        }
        let alpha = closing.alpha_at(now);
        if alpha <= 0.001 {
            continue;
        }
        let Some(window_rect) = view.project(closing.workspace, closing.window_rect) else {
            continue;
        };
        let Some(snapshot_rect) = view.project(closing.workspace, closing.snapshot_rect) else {
            continue;
        };
        let Some(card) = view.cards.get(closing.workspace) else {
            continue;
        };
        // Scale around the window centre, not the snapshot bounds: the baked
        // border/shadow make the texture larger than the window itself and
        // scaling that box would drift the content sideways.
        let factor = closing.scale_at(now);
        let centre = window_rect.loc.to_f64() + window_rect.size.to_f64() / 2.0;
        let scaled_size = snapshot_rect.size.to_f64() * factor;
        let offset = snapshot_rect.loc.to_f64() - centre;
        let scaled_loc = Point::from((centre.x + offset.x * factor, centre.y + offset.y * factor));
        let dst = Rectangle::new(scaled_loc.to_i32_round(), scaled_size.to_i32_round());
        if dst.size.w <= 0 || dst.size.h <= 0 || dst.intersection(output_geometry).is_none() {
            continue;
        }
        // Output-local logical rectangle; render elements use output-local
        // physical coordinates even though cards/windows are tracked in
        // compositor coordinates (multi-output offsets must not leak in here).
        let local_dst = Rectangle::new(dst.loc - output_geometry.loc, dst.size);
        let location = local_dst.loc.to_f64().to_physical_precise_round(scale);
        // Sample the whole baked texture while drawing it at the scaled
        // destination size. Without the explicit source rectangle Smithay
        // derives it from `size`, which samples only the top-left corner.
        let src = Rectangle::from_size(closing.source_size.to_f64());
        let element = TextureRenderElement::from_texture_buffer(
            location,
            &closing.buffer,
            Some(alpha),
            Some(src),
            Some(local_dst.size),
            Kind::Unspecified,
        );
        let local_card = Rectangle::new(card.rect.loc - output_geometry.loc, card.rect.size);
        let crop = local_card.to_f64().to_physical_precise_round(scale);
        let Some(cropped) = CropRenderElement::from_element(element, scale, crop) else {
            continue;
        };
        elements.push(OutputElements::Closing(cropped));
    }
}

/// Add one window: popups, surfaces, border, shadow and blur backdrop, front to
/// back.
///
/// The blur element is pushed last, so the damage tracker captures the
/// framebuffer before drawing the window itself: the backdrop only ever
/// contains what is *below* this window.
#[allow(clippy::too_many_arguments)]
fn push_window<R>(
    elements: &mut Vec<OutputElements<R>>,
    core: &mut Core,
    renderer: &mut R,
    managed: &RenderWindow,
    output_geometry: Rectangle<i32, Logical>,
    scale: f64,
    time: f32,
    overview: Option<f64>,
    snapshot: bool,
) where
    R: PolyRenderer,
    R::TextureId: Clone + Send + 'static,
{
    if managed.alpha <= 0.001 {
        return;
    }
    let geometry = managed.window.geometry();
    // Output-local logical rectangle of the drawn window.
    let local_rect = Rectangle::new(managed.rect.loc - output_geometry.loc, managed.rect.size);
    // Output-local card of this window's workspace. Windows and their shadows
    // are clipped to it: a floating window may stick out of the workspace and
    // would otherwise paint over the gap or the neighbouring card.
    let card_rect = Rectangle::new(managed.card.loc - output_geometry.loc, managed.card.size);
    let card_bounds = card_clip(local_rect, card_rect);

    // Animation/overview rescale factors (1, 1) when nothing is animating.
    let sx = managed.rect.size.w as f64 / f64::from(geometry.size.w.max(1));
    let sy = managed.rect.size.h as f64 / f64::from(geometry.size.h.max(1));
    let rescale = Scale::from((sx, sy));

    // Output-local logical location of the window's surface tree. The surface
    // geometry offset (CSD shadows and the like) belongs to the window content,
    // so it is scaled with it — otherwise a zoomed-out thumbnail would show its
    // client-side decorations shifted by `(1 - zoom) * offset`.
    let offset = geometry
        .loc
        .to_f64()
        .upscale(rescale)
        .to_physical_precise_round(scale);
    let location = (managed.rect.loc - output_geometry.loc)
        .to_f64()
        .to_physical_precise_round(scale)
        - offset;

    // In the overview a window is a shrunken copy of itself: radius, border
    // width and shadow scale with the card, the blur pass is dropped, and the
    // popups are not drawn (they would float outside the card). The opening
    // animation scales the same decorations but keeps the blur, so a blurred
    // window does not pop when the animation ends.
    let scaled = match overview {
        Some(zoom) => Some(managed.visuals.scaled(zoom * managed.scale)),
        None if managed.scale < 0.9999 => Some(managed.visuals.scaled_animation(managed.scale)),
        None => None,
    };
    let visuals = scaled.as_ref().unwrap_or(&managed.visuals);
    // A fullscreen window keeps its own output when the overview is closed; in
    // the overview it is a normal card-sized window with the usual border.
    let fullscreen = overview.is_none() && managed.fullscreen == Some(FullscreenMode::Fullscreen);
    let focused = core
        .last_window_focus
        .as_ref()
        .is_some_and(|root| return root == &managed.root);
    let cursor = window_cursor(core, managed.rect);

    // `ext-background-effect-v1`: the client may ask for only part of its window
    // to be blurred. `general.blur.enable` stays the master switch — it decides
    // whether the window has blur parameters at all — while
    // `accept_client_regions` decides whether the client's region replaces the
    // whole-window backdrop. The surface state is only read when this window
    // could blur at all: otherwise the common case pays a lock per frame for
    // nothing.
    let blur_region = if visuals.blur.is_none() {
        None
    } else {
        client_blur_region(
            core.config.general.blur.accept_client_regions,
            &managed.root,
            local_rect,
            geometry,
            rescale,
        )
    };

    let inputs = DecorationInputs {
        id: managed.id,
        rect: local_rect,
        alpha: managed.alpha,
        focused,
        visuals,
        fullscreen,
        scale,
        time,
        cursor,
        clip: Some(card_rect),
        blur_region: blur_region.as_deref(),
    };
    let decorations = core.decorations.prepare(&inputs, &core.shaders);

    let border_width = if fullscreen {
        0.0
    } else if visuals.border_width > 0.0 && core.shaders.border().is_some() {
        visuals.border_width.round().max(1.0)
    } else {
        0.0
    };

    // Clipping (rounded corners, the window shader and the workspace card) is
    // only needed when it would change something: a zero radius, the built-in
    // shader and a card that contains the window draw it as-is.
    let clip = if fullscreen
        || (card_bounds.is_none() && decorations.radius.is_zero() && visuals.shader_path.is_none())
    {
        None
    } else {
        let path = visuals.shader_path.as_deref();
        match core.shaders.window_program(path) {
            Some(program) => Some(WindowClip {
                program: program.clone(),
                geometry: local_rect.to_f64(),
                corner_radius: decorations.radius,
                rescale,
                scale: scale as f32,
                card_clip: card_bounds.unwrap_or([0.0, 0.0, 1.0, 1.0]),
                uniforms: WindowUniforms {
                    time,
                    alpha: managed.alpha,
                    border_width,
                    border_color: if border_width > 0.0 {
                        visuals.border_color(focused)
                    } else {
                        [0.0; 4]
                    },
                    focused,
                    window_id: managed.id,
                    cursor,
                    uses_time: core.shaders.window_uses_time(path),
                    uses_cursor: core.shaders.window_uses_cursor(path),
                },
            }),
            None => {
                core.decorations.warn_once(
                    managed.id,
                    "no window shader compiled; drawing the window unclipped",
                );
                None
            }
        }
    };

    // Popups belong to the window but are not clipped: they may extend past
    // the rounded corners and have their own buffers.
    // The overview leaves them out, and so does a window that is still scaling
    // in: their offsets are not rescaled with the surface. Close snapshots do
    // include them while the window is at its settled size.
    if overview.is_none() && managed.scale >= 0.9999 {
        for (popup, popup_offset) in PopupManager::popups_for_surface(&managed.root) {
            let offset = (geometry.loc + popup_offset - popup.geometry().loc)
                .to_physical_precise_round(scale);
            let rendered = render_elements_from_surface_tree::<R, WaylandSurfaceRenderElement<R>>(
                renderer,
                popup.wl_surface(),
                location + offset,
                scale,
                managed.alpha,
                Kind::Unspecified,
            );
            elements.extend(rendered.into_iter().map(OutputElements::Surface));
        }
    }

    let mut rendered = render_elements_from_surface_tree::<R, WaylandSurfaceRenderElement<R>>(
        renderer,
        &managed.root,
        location,
        scale,
        managed.alpha,
        Kind::Unspecified,
    );
    // A destruction hook runs after Smithay has already orphaned the surface
    // being destroyed: the root is no longer in its own child list, so tree
    // traversal yields nothing even though its buffer and texture are still
    // alive. Render that last root surface directly for the close snapshot.
    if snapshot && rendered.is_empty() {
        let root_location = with_renderer_surface_state(&managed.root, |state| {
            return state.view().map(|view| {
                return location.to_f64() + view.offset.to_f64().to_physical(scale);
            });
        })
        .flatten();
        if let Some(root_location) = root_location {
            let element = with_states(&managed.root, |states| {
                return WaylandSurfaceRenderElement::from_surface(
                    renderer,
                    &managed.root,
                    states,
                    root_location,
                    managed.alpha,
                    Kind::Unspecified,
                );
            });
            match element {
                Ok(Some(element)) => rendered.push(element),
                Ok(None) => {}
                Err(err) => {
                    warn!("failed to snapshot the destroyed root surface: {err}");
                }
            }
        }
    }
    for element in rendered {
        elements.push(OutputElements::Window(WindowElement::new(
            element,
            location,
            rescale,
            clip.clone(),
            decorations.epoch,
        )));
    }

    // Border and shadow are outside the window rectangle, the blur backdrop is
    // behind everything of this window.
    if let Some(border) = decorations.border {
        elements.push(OutputElements::Shader(border));
    }
    if let Some(shadow) = decorations.shadow {
        elements.push(OutputElements::Shader(shadow));
    }
    if let Some(blur) = decorations.blur {
        // A snapshot has no framebuffer behind it: drawing a blur element
        // there would capture the empty offscreen target.
        if !snapshot {
            elements.push(OutputElements::Blur(blur));
        }
    }
}

/// The blur region a client set on a window, in output-local logical
/// coordinates.
///
/// `ext-background-effect-v1` stores the region in *surface-local* logical
/// coordinates. The surface tree is drawn at `local_rect.loc` shifted by the
/// surface geometry offset and scaled by the opening animation, so a surface
/// point maps to `local_rect.loc + (point - geometry.loc) * rescale` — the same
/// transform `push_window` uses for the window's own geometry.
///
/// `None` means "the client never asked for a region" and keeps the configured
/// whole-window backdrop; `Some(&[])` means it asked for none, which disables
/// the blur. The rects are not clipped to the window here;
/// `decorations::client_regions` clips them once the drawn rectangle is known.
fn client_blur_region(
    accept: bool,
    root: &WlSurface,
    local_rect: Rectangle<i32, Logical>,
    geometry: Rectangle<i32, Logical>,
    rescale: Scale<f64>,
) -> Option<Vec<Rectangle<i32, Logical>>> {
    if !accept {
        return None;
    }
    // A client that never mentioned blur keeps the configured whole-window
    // backdrop. One that did gets exactly what it asked for — including the
    // null region Noctalia sends to switch blur off, which has to stay
    // distinguishable from silence (see `handlers/background_effect.rs`).
    if !crate::core::handlers::background_effect::blur_requested(root) {
        return None;
    }
    let region = with_states(root, |states| {
        return states
            .cached_state
            .get::<BackgroundEffectSurfaceCachedState>()
            .current()
            .blur_region
            .clone();
    });
    let Some(region) = region else {
        return Some(Vec::new());
    };

    // The protocol region is an ordered add/subtract list, so a client can cut a
    // hole out of its backdrop.
    let mut added: Vec<Rectangle<i32, Logical>> = Vec::new();
    let mut cut: Vec<Rectangle<i32, Logical>> = Vec::new();
    for (kind, rect) in region.rects {
        match kind {
            RectangleKind::Add => added.push(rect),
            RectangleKind::Subtract => cut.push(rect),
        }
    }
    if !cut.is_empty() {
        added = Rectangle::subtract_rects_many_in_place(added, cut);
    }

    let mut out = Vec::with_capacity(added.len());
    for rect in added {
        out.push(place_client_rect(rect, local_rect, geometry, rescale));
    }
    return Some(out);
}

/// Map one surface-local logical rectangle into output-local logical space.
fn place_client_rect(
    rect: Rectangle<i32, Logical>,
    local_rect: Rectangle<i32, Logical>,
    geometry: Rectangle<i32, Logical>,
    rescale: Scale<f64>,
) -> Rectangle<i32, Logical> {
    let loc = Point::from((
        (f64::from(rect.loc.x - geometry.loc.x) * rescale.x).round() as i32,
        (f64::from(rect.loc.y - geometry.loc.y) * rescale.y).round() as i32,
    ));
    let size = Size::from((
        (f64::from(rect.size.w) * rescale.x).round() as i32,
        (f64::from(rect.size.h) * rescale.y).round() as i32,
    ));
    return Rectangle::new(local_rect.loc + loc, size);
}

/// Pointer position in window coordinates, or a negative point when the
/// pointer is outside the window (`u_cursor`).
fn window_cursor(core: &Core, rect: Rectangle<i32, Logical>) -> [f32; 2] {
    let pointer = core.pointer_location();
    if !rect.to_f64().contains(pointer) {
        return [-1.0, -1.0];
    }
    let local = pointer - rect.loc.to_f64();
    return [local.x as f32, local.y as f32];
}

/// Add the topmost-to-bottommost surfaces of `layers` at their configured
/// geometry. `layer_geometry` is output-local, which is also the coordinate
/// space of the render list.
#[allow(clippy::too_many_arguments)]
fn push_layers<R>(
    elements: &mut Vec<OutputElements<R>>,
    core: &Core,
    renderer: &mut R,
    output: &Output,
    layers: &[Layer],
    scale: f64,
    time: f32,
) where
    R: PolyRenderer,
    R::TextureId: Clone + Send + 'static,
{
    let map = layer_map_for_output(output);
    for layer in layers {
        for surface in map.layers_on(*layer).rev() {
            let Some(geometry) = map.layer_geometry(surface) else {
                continue;
            };
            let location = Point::<i32, Logical>::from((geometry.loc.x, geometry.loc.y))
                .to_physical_precise_round(scale);
            // Popups belong to the layer surface and are positioned relative to
            // its geometry (layer surfaces have no `set_window_geometry`). Push
            // them before the layer contents so they stay on top: the element
            // list is front to back and the damage tracker draws it in reverse.
            for (popup, popup_offset) in PopupManager::popups_for_surface(surface.wl_surface()) {
                let offset = (popup_offset - popup.geometry().loc).to_physical_precise_round(scale);
                let rendered = render_elements_from_surface_tree::<R, WaylandSurfaceRenderElement<R>>(
                    renderer,
                    popup.wl_surface(),
                    location + offset,
                    Scale::from(scale),
                    1.0,
                    Kind::Unspecified,
                );
                elements.extend(rendered.into_iter().map(OutputElements::Surface));
            }
            let rendered = render_elements_from_surface_tree::<R, WaylandSurfaceRenderElement<R>>(
                renderer,
                surface.wl_surface(),
                location,
                Scale::from(scale),
                1.0,
                Kind::Unspecified,
            );
            elements.extend(rendered.into_iter().map(OutputElements::Surface));
            // The backdrop of a bar, panel or dock goes below everything of
            // this surface, popups included, so it only captures what is
            // behind the layer.
            if let Some(blur) = layer_blur(core, surface.wl_surface(), geometry, scale, time) {
                elements.push(OutputElements::Blur(blur));
            }
        }
    }
}

/// The backdrop a layer surface asked for, if any.
///
/// Layer surfaces get no configured whole-surface blur the way windows do —
/// `general.blur.enable` only unlocks the feature. A layer surface is blurred
/// exactly over the regions it commits through `ext-background-effect-v1`, so a
/// wallpaper or a bar that never calls `set_blur_region` stays sharp instead of
/// blurring a full-width strip of the output.
///
/// The element id comes from the surface, so the damage tracker keeps the
/// offscreen backdrop across frames, and the commit counter of the surface
/// drives repaints: a bar that repaints its own blur region does not reallocate
/// the blur textures every frame.
fn layer_blur(
    core: &Core,
    surface: &WlSurface,
    rect: Rectangle<i32, Logical>,
    scale: f64,
    time: f32,
) -> Option<BlurElement> {
    let config = &core.config.general.blur;
    if !config.enable || !config.layer_surfaces || !config.accept_client_regions {
        return None;
    }
    if rect.size.w <= 0 || rect.size.h <= 0 {
        return None;
    }
    let program = core.shaders.blur()?;
    let regions = layer_blur_regions(surface, rect)?;
    let commit = with_renderer_surface_state(surface, |state| {
        return state.current_commit();
    })
    .unwrap_or_default();
    let passes = config.passes.clamp(1, 16);
    return Some(BlurElement::new(
        // The surface's own render element already owns the plain wayland id.
        // A blur element is a framebuffer effect, and two elements sharing one
        // id inside a single damage-tracker pass are treated as the same
        // element — the backdrop then never reaches the framebuffer. The
        // namespace keeps the id stable per surface while distinguishing it
        // from the surface's own element.
        Id::from_wayland_resource(surface).namespaced(BLUR_ID_NAMESPACE),
        commit,
        program.clone(),
        rect.to_f64(),
        Some(Rc::from(regions)),
        CornerRadius::default(),
        scale,
        BlurOptions {
            passes,
            offset: config.offset,
        },
        config.noise.clamp(0.0, 1.0) as f32,
        1.0,
        program.uses_time(),
        time,
    ));
}

/// Namespace that separates a layer surface's blur backdrop from the render
/// element of the surface itself, which share one wayland resource id.
const BLUR_ID_NAMESPACE: usize = 0x626c_7572; // "blur"

/// Element-local rectangles a layer surface asked to blur.
///
/// `None` when the client never set a region, or when what it set covers
/// nothing on screen: unlike a window, a layer surface never falls back to a
/// whole-surface backdrop, because there is no configured one.
///
/// Layer-shell surfaces have no client-side shadow trick that would shift
/// their content, so the region is used in surface coordinates with the layer
/// rectangle as origin. A client that does call `set_window_geometry` would
/// only shift the backdrop by the same few pixels its own content moves.
fn layer_blur_regions(
    surface: &WlSurface,
    rect: Rectangle<i32, Logical>,
) -> Option<Vec<Rectangle<i32, Logical>>> {
    let region = with_states(surface, |states| {
        return states
            .cached_state
            .get::<BackgroundEffectSurfaceCachedState>()
            .current()
            .blur_region
            .clone();
    })?;

    // The protocol region is an ordered add/subtract list, so a client can cut
    // holes into its backdrop.
    let mut added: Vec<Rectangle<i32, Logical>> = Vec::new();
    let mut cut: Vec<Rectangle<i32, Logical>> = Vec::new();
    for (kind, region) in region.rects {
        match kind {
            RectangleKind::Add => added.push(region),
            RectangleKind::Subtract => cut.push(region),
        }
    }
    if !cut.is_empty() {
        added = Rectangle::subtract_rects_many_in_place(added, cut);
    }

    let mut placed = Vec::with_capacity(added.len());
    for region in added {
        let rect_on_output = Rectangle::new(rect.loc + region.loc, region.size);
        let Some(inside) = rect_on_output.intersection(rect) else {
            continue;
        };
        placed.push(Rectangle::new(inside.loc - rect.loc, inside.size));
    }
    let disjoint = decorations::disjoint_pieces(placed);
    return (!disjoint.is_empty()).then_some(disjoint);
}

/// Add the Toast stack of the compositor, centered at the top of the output.
///
/// The text is pushed before its dark plate, so the plate is drawn below it.
fn push_toasts<R>(
    elements: &mut Vec<OutputElements<R>>,
    core: &Core,
    renderer: &mut R,
    output: &Output,
    scale: f64,
) where
    R: PolyRenderer,
    R::TextureId: Clone + Send + 'static,
{
    if core.toasts.is_empty() {
        return;
    }
    let Some(geometry) = core.space.output_geometry(output) else {
        return;
    };
    for placement in core.toasts.placements(geometry.size, scale) {
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            placement.rect.loc.to_f64(),
            &placement.buffer,
            None,
            None,
            Some(placement.size),
            Kind::Unspecified,
        ) {
            Ok(element) => elements.push(OutputElements::Toast(element)),
            Err(err) => {
                warn!("error building a Toast element: {err:?}");
                continue;
            }
        }
        elements.push(OutputElements::Solid(SolidColorRenderElement::from_buffer(
            &placement.background,
            placement.rect.loc,
            scale,
            1.0,
            Kind::Unspecified,
        )));
    }
}

/// Append the software cursor to `elements`, if it belongs on this output.
pub fn push_cursor<R>(
    elements: &mut Vec<OutputElements<R>>,
    core: &mut Core,
    renderer: &mut R,
    output: &Output,
) where
    R: PolyRenderer,
    R::TextureId: Clone + Send + 'static,
{
    // A client-provided cursor surface can go away at any time.
    if is_dead_cursor_surface(&core.cursor_status) {
        core.cursor_status = CursorImageStatus::default_named();
    }
    if matches!(core.cursor_status, CursorImageStatus::Hidden) {
        return;
    }

    let Some(output_geometry) = core.space.output_geometry(output) else {
        return;
    };
    let pointer_location = core.pointer_location();
    if !output_geometry.to_f64().contains(pointer_location) {
        return;
    }

    let scale = output.current_scale();
    let image = core
        .cursor
        .get_image(scale.integer_scale() as u32, core.start_time.elapsed());

    let buffer = core
        .pointer_images
        .iter()
        .find_map(|(cached, buffer)| return (cached == &image).then(|| return buffer.clone()))
        .unwrap_or_else(|| {
            let buffer = buffer_from_image(&image, scale.integer_scale());
            core.pointer_images.push((image, buffer.clone()));
            return buffer;
        });

    core.cursor_element.set_buffer(buffer);
    core.cursor_element.set_status(core.cursor_status.clone());

    let hotspot = cursor_hotspot(&core.cursor_status);
    let location = (pointer_location - output_geometry.loc.to_f64() - hotspot.to_f64())
        .to_physical(scale.fractional_scale())
        .to_i32_round();
    let cursor_elements =
        core.cursor_element
            .elements(renderer, location, Scale::from(scale.fractional_scale()));

    elements.extend(cursor_elements.into_iter().map(OutputElements::Cursor));
}

/// Tell the clients whose content is on `output` that a frame was presented.
///
/// Windows on inactive workspaces are skipped: they are off screen (or a
/// scaled card in the overview), and keeping their frame callbacks flowing
/// would make them render frames nobody sees.
pub fn send_frame_callbacks(core: &mut Core, output: &Output, time: Duration) {
    let now = Instant::now();
    let overview = core.layout.overview_visible(now);
    let active = core.layout.active_workspace(output);
    for window in core.space.elements() {
        if !overview {
            let visible = core
                .layout
                .by_window(window)
                .and_then(|managed| return core.layout.workspace_of(managed.id))
                .is_none_or(|(_, workspace)| return workspace == active);
            if !visible {
                continue;
            }
        }
        window.send_frame(output, time, FRAME_CALLBACK_THROTTLE, |_, _| {
            return Some(output.clone());
        });
    }
    for layer in layer_map_for_output(output).layers() {
        layer.send_frame(output, time, FRAME_CALLBACK_THROTTLE, |_, _| {
            return Some(output.clone());
        });
    }
    if let Some(lock_surface) = core.lock_surface_for_output(output) {
        let lock_surface = lock_surface.wl_surface().clone();
        send_frames_surface_tree(
            &lock_surface,
            output,
            time,
            FRAME_CALLBACK_THROTTLE,
            |_, _| return Some(output.clone()),
        );
    }
    core.output_state(output).frame_callback_sequence = core
        .output_state(output)
        .frame_callback_sequence
        .wrapping_add(1);
}

/// Collect the presentation feedbacks of everything drawn on `output`.
pub fn take_presentation_feedbacks(
    core: &Core,
    output: &Output,
    states: &RenderElementStates,
) -> OutputPresentationFeedback {
    let mut feedback = OutputPresentationFeedback::new(output);
    for window in core.space.elements() {
        window.take_presentation_feedback(
            &mut feedback,
            |_, _| return Some(output.clone()),
            |surface, _| {
                return surface_presentation_feedback_flags_from_states(surface, None, states);
            },
        );
    }
    for layer in layer_map_for_output(output).layers() {
        layer.take_presentation_feedback(
            &mut feedback,
            |_, _| return Some(output.clone()),
            |surface, _| {
                return surface_presentation_feedback_flags_from_states(surface, None, states);
            },
        );
    }
    if let Some(lock_surface) = core.lock_surface_for_output(output) {
        take_presentation_feedback_surface_tree(
            lock_surface.wl_surface(),
            &mut feedback,
            |_, _| return Some(output.clone()),
            |surface, _| {
                return surface_presentation_feedback_flags_from_states(surface, None, states);
            },
        );
    }
    return feedback;
}

/// Everything needed to bake a window into a closing snapshot.
pub struct SnapshotWindow {
    /// Stable layout id, used by the decoration cache.
    pub id: u64,
    /// The Smithay window whose surfaces are rendered.
    pub window: smithay::desktop::Window,
    /// Root surface of the window.
    pub root: WlSurface,
    /// Window rectangle in workspace-local coordinates (including the open
    /// scale); `snapshot_window` shifts it to the output before baking.
    pub rect: Rectangle<i32, Logical>,
    /// Current opacity of the live window (window opacity times open alpha).
    pub alpha: f32,
    /// Current opening-animation scale (`1.0` when settled).
    pub scale: f64,
    /// Fullscreen state, so borders are omitted like in the live frame.
    pub fullscreen: Option<FullscreenMode>,
    /// Resolved decorations and shader selection.
    pub visuals: WindowVisuals,
}

/// A baked window and the workspace-local rectangle it covers.
pub struct WindowSnapshot {
    /// Texture holding content, border and shadow on a transparent backdrop.
    pub buffer: TextureBuffer<GlesTexture>,
    /// The covered rectangle relative to the workspace origin (logical).
    pub bounds: Rectangle<i32, Logical>,
    /// Full size of the baked texture, in the texture's own logical units.
    ///
    /// The texture is created with scale 1, so this equals the buffer size in
    /// pixels. The close animation needs it as the source rectangle: deriving
    /// the source from the scaled destination size would sample only the
    /// top-left corner and crop the window instead of shrinking it.
    pub source_size: Size<i32, Logical>,
}

/// Render a window into an offscreen texture for the close animation.
///
/// This runs before the toplevel is removed, while its last buffer is still
/// attached. Blur is skipped because the texture has no framebuffer behind it;
/// popups are included when the window is settled. Any failure returns `None`;
/// the window then closes without an animation, which is the safe fallback.
pub fn snapshot_window(
    core: &mut Core,
    renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
    window: &SnapshotWindow,
    output_geometry: Rectangle<i32, Logical>,
    output_scale: f64,
) -> Option<WindowSnapshot> {
    let time = core.start_time.elapsed().as_secs_f32();
    // Window rectangles are workspace-local, but `push_window` expects
    // compositor coordinates so that it can subtract `output_geometry.loc` and
    // produce output-local elements. Shift the snapshot rectangle to the
    // output's screen position; the baked texture stays workspace-local just
    // like `ClosingWindow.snapshot_rect`. This matters for outputs that do not
    // start at (0, 0): otherwise the card clip would reject the whole window.
    let screen_rect = Rectangle::new(
        Point::from((
            window.rect.loc.x + output_geometry.loc.x,
            window.rect.loc.y + output_geometry.loc.y,
        )),
        window.rect.size,
    );
    let entry = RenderWindow {
        id: window.id,
        window: window.window.clone(),
        root: window.root.clone(),
        rect: screen_rect,
        card: output_geometry,
        alpha: window.alpha,
        scale: window.scale,
        fullscreen: window.fullscreen,
        reordering: false,
        visuals: window.visuals.clone(),
    };
    let mut elements = Vec::new();
    push_window(
        &mut elements,
        core,
        renderer,
        &entry,
        output_geometry,
        output_scale,
        time,
        None,
        true,
    );
    let window_elements = elements
        .iter()
        .filter(|element| return matches!(element, OutputElements::Window(_)))
        .count();
    if elements.is_empty() || window_elements == 0 {
        // A border/shadow alone would bake a hollow ghost: the client buffer is
        // already gone (null-buffer unmap), so there is nothing to animate.
        return None;
    }
    let physical_scale = Scale::from(output_scale);
    let bounds = elements
        .iter()
        .map(|element| return Element::geometry(element, physical_scale))
        .reduce(|a, b| return a.merge(b))?;
    if bounds.size.w <= 0 || bounds.size.h <= 0 {
        return None;
    }
    let buffer_size: Size<i32, Buffer> = Size::from((bounds.size.w, bounds.size.h));
    let mut texture = renderer.create_buffer(Fourcc::Abgr8888, buffer_size).ok()?;
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        let mut frame = renderer
            .render(&mut target, bounds.size, Transform::Normal)
            .ok()?;
        let plain = Rectangle::from_size(bounds.size);
        frame.clear(Color32F::TRANSPARENT, &[plain]).ok()?;
        for element in elements.into_iter().rev() {
            let element = RelocateRenderElement::from_element(
                element,
                (-bounds.loc.x, -bounds.loc.y),
                Relocate::Relative,
            );
            let dst = Element::geometry(&element, physical_scale);
            let Some(mut damage) = plain.intersection(dst) else {
                continue;
            };
            damage.loc -= dst.loc;
            let cache = UserDataMap::new();
            RenderElement::<smithay::backend::renderer::gles::GlesRenderer>::draw(
                &element,
                &mut frame,
                Element::src(&element),
                dst,
                &[damage],
                &[],
                Some(&cache),
            )
            .ok()?;
        }
        let _sync = frame.finish().ok()?;
    }
    let buffer = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
    let bounds_logical = Rectangle::new(
        Point::from((
            (f64::from(bounds.loc.x) / output_scale).round() as i32,
            (f64::from(bounds.loc.y) / output_scale).round() as i32,
        )),
        Size::from((
            (f64::from(bounds.size.w) / output_scale).round() as i32,
            (f64::from(bounds.size.h) / output_scale).round() as i32,
        )),
    );
    return Some(WindowSnapshot {
        buffer,
        bounds: bounds_logical,
        source_size: Size::from((bounds.size.w, bounds.size.h)),
    });
}

/// Render one frame of `output` into a client's shm buffer.
///
/// This is the compositor side of `ext-image-copy-capture-v1` (`grim`,
/// `wl-mirror`, a portal screencast, ...). `buffer` is the one the client
/// attached to its capture frame; it has to match what the session announced
/// (`mode` size, `Xrgb8888`), and a mismatch fails before anything is rendered.
///
/// The expensive parts — the offscreen texture, the damage tracker, the
/// knowledge of how far behind the client's buffer is — live in the session
/// (`crate::core::capture`), so a steady stream of frames only repaints, reads
/// back and copies what actually changed, and does nothing at all while the
/// screen is idle.
pub fn capture_output(
    core: &mut Core,
    capture: &mut CaptureState,
    key: CaptureKey,
    renderer: &mut GlesRenderer,
    output: &Output,
    draw_cursor: bool,
    buffer: &WlBuffer,
) -> anyhow::Result<CaptureOutcome> {
    let Some(mode) = output.current_mode() else {
        return Err(anyhow::anyhow!(
            "output {} has no mode to capture",
            output.name()
        ));
    };
    let size: Size<i32, Physical> = mode.size;
    if size.w <= 0 || size.h <= 0 {
        return Err(anyhow::anyhow!(
            "output {} has an empty mode to capture",
            output.name()
        ));
    }
    let scale = output.current_scale().fractional_scale();
    // Logged because the output transform is the one thing a backend may set for
    // its own framebuffer reasons (see below) and it is not exposed over IPC.
    debug!(
        output = output.name(),
        ?size,
        scale,
        backend_transform = ?output.current_transform(),
        draw_cursor,
        "capturing output"
    );

    // The element list needs `&mut Core` and the renderer, so it is built
    // before the session is borrowed: `CaptureState` is a separate field of
    // `State` for exactly this reason (`handlers/image_copy_capture.rs`).
    let elements = output_elements(core, renderer, output, draw_cursor);
    let background = core
        .output_background(output)
        .map(|color| return color.to_floats())
        .unwrap_or([0.0, 0.0, 0.0, 1.0]);

    let session = capture.session(key, renderer, size, scale.into())?;
    return session.capture_frame(renderer, &elements, background, buffer);
}

/// Render one frame of `output` and copy `src` out of it into a client's shm
/// buffer.
///
/// This is the compositor side of `zwlr_screencopy_frame_v1`: a one-shot frame
/// object that is filled by a single `copy`, so there is no session and no
/// damage bookkeeping. The client always gets a complete picture of `src`, which
/// is what it is entitled to expect when the compositor sends no `damage`
/// events (`crate::core::capture::ScreencopyTarget`).
///
/// `src` is in framebuffer pixels. The protocol hands out the region in output
/// *logical* coordinates, so the caller scales it (`handlers/screencopy.rs`), and
/// `buffer` must be exactly `src.size` large.
pub fn capture_region(
    core: &mut Core,
    capture: &mut CaptureState,
    renderer: &mut GlesRenderer,
    output: &Output,
    src: Rectangle<i32, Physical>,
    draw_cursor: bool,
    buffer: &WlBuffer,
) -> anyhow::Result<()> {
    if src.size.w <= 0 || src.size.h <= 0 {
        return Err(anyhow::anyhow!(
            "refusing to capture an empty region of {}",
            output.name()
        ));
    }
    let Some(mode) = output.current_mode() else {
        return Err(anyhow::anyhow!(
            "output {} has no mode to capture",
            output.name()
        ));
    };
    let scale = output.current_scale().fractional_scale();
    // The element list needs `&mut Core` and the renderer, so it is built before
    // the target is borrowed (`handlers/screencopy.rs` destructures `State` for
    // the same reason).
    let elements = output_elements(core, renderer, output, draw_cursor);
    let background = core
        .output_background(output)
        .map(|color| return color.to_floats())
        .unwrap_or([0.0, 0.0, 0.0, 1.0]);

    let target = capture.screencopy_target(
        renderer,
        &output.name(),
        output.downgrade(),
        mode.size,
        scale.into(),
    )?;
    return target.capture_region(renderer, &elements, background, buffer, src);
}

/// Render the pointer into the shm buffer of a cursor-capture frame.
///
/// The client composites the sprite itself, at the position and hotspot its
/// cursor session was told about, so nothing but the pointer is drawn here and
/// the rest of the buffer stays transparent — which is why this is the one
/// capture that uses `Argb8888` instead of `Xrgb8888`.
pub fn capture_cursor(
    core: &mut Core,
    renderer: &mut GlesRenderer,
    output: &Output,
    buffer: &WlBuffer,
) -> anyhow::Result<()> {
    let scale = output.current_scale();
    let integer = scale.integer_scale();
    let Some(size) = cursor_capture_size(core, integer) else {
        return Err(anyhow::anyhow!("there is no cursor to capture"));
    };

    // The cursor element is shared with the on-screen rendering, so it is set
    // up exactly like `push_cursor` does before the elements are built.
    if matches!(core.cursor_status, CursorImageStatus::Named(_)) {
        let image = core
            .cursor
            .get_image(integer.max(1) as u32, core.start_time.elapsed());
        core.cursor_element
            .set_buffer(buffer_from_image(&image, integer.max(1)));
    }
    core.cursor_element.set_status(core.cursor_status.clone());
    let scale = Scale::from(scale.fractional_scale());
    let elements = core
        .cursor_element
        .elements(renderer, Point::from((0, 0)), scale);

    let buffer_size: Size<i32, Buffer> = Size::from((size.w, size.h));
    let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Argb8888, buffer_size)?;
    let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
    let mut target = renderer.bind(&mut texture)?;
    // A transparent clear: the client is compositing, not covering.
    tracker.render_output(renderer, &mut target, 0, &elements, [0.0, 0.0, 0.0, 0.0])?;
    crate::core::capture::copy_cursor_into_shm(buffer, buffer_size, renderer, &target)?;
    return Ok(());
}

/// The buffer size of the pointer image, in output pixels.
///
/// This is what a cursor capture session has to allocate, and it changes when
/// the client changes its cursor surface or the theme animates into a frame of
/// another size — which is why the constraints are re-sent from
/// `refresh_cursor_sessions`.
pub fn cursor_capture_size(core: &Core, integer: i32) -> Option<Size<i32, Physical>> {
    return match &core.cursor_status {
        CursorImageStatus::Hidden => None,
        CursorImageStatus::Named(_) => {
            let image = core
                .cursor
                .get_image(integer.max(1) as u32, core.start_time.elapsed());
            let size = Size::<i32, Physical>::from((image.width as i32, image.height as i32));
            (size.w > 0 && size.h > 0).then_some(size)
        }
        CursorImageStatus::Surface(surface) => {
            // `buffer_size` is the surface size in surface-logical pixels; a
            // cursor buffer is never scaled by its client (the compositor picks
            // the scale from the theme), so it is the pixel size here.
            let size = with_renderer_surface_state(surface, |state| return state.buffer_size())??;
            let size = Size::<i32, Physical>::from((size.w, size.h));
            (size.w > 0 && size.h > 0).then_some(size)
        }
    };
}

/// Where the hotspot sits inside the pointer image, in buffer pixels.
///
/// The protocol reports it relative to the captured cursor buffer, while a
/// client-provided cursor keeps its hotspot in surface-logical coordinates, so
/// the scale has to be applied here.
pub fn cursor_capture_hotspot(core: &Core, integer: i32) -> Point<i32, Buffer> {
    return match &core.cursor_status {
        CursorImageStatus::Named(_) => {
            let image = core
                .cursor
                .get_image(integer.max(1) as u32, core.start_time.elapsed());
            Point::from((image.xhot as i32, image.yhot as i32))
        }
        CursorImageStatus::Hidden => Point::from((0, 0)),
        CursorImageStatus::Surface(_) => {
            let hotspot = cursor_hotspot(&core.cursor_status)
                .to_f64()
                .to_physical(Scale::from(f64::from(integer.max(1))))
                .to_i32_round();
            Point::from((hotspot.x, hotspot.y))
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Size;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        return Rectangle::new(Point::from((x, y)), Size::from((w, h)));
    }

    #[test]
    fn client_blur_rect_maps_surface_space_to_output_space() {
        // The surface origin sits at the window rect origin minus the surface
        // geometry offset: a client region starting at (0, 0) is not the
        // window's top-left when the window has client-side decoration.
        let window = rect(100, 50, 400, 300);
        let geometry = rect(10, 20, 380, 280);
        assert_eq!(
            place_client_rect(rect(0, 0, 100, 100), window, geometry, Scale::from(1.0)),
            rect(90, 30, 100, 100)
        );
    }

    #[test]
    fn client_blur_rect_follows_the_opening_animation() {
        // The animation shrinks the surface around the window origin, so the
        // region shrinks and moves with it.
        let window = rect(100, 50, 200, 150);
        let geometry = rect(10, 20, 380, 280);
        assert_eq!(
            place_client_rect(rect(0, 0, 100, 100), window, geometry, Scale::from(0.5)),
            rect(95, 40, 50, 50)
        );
    }

    #[test]
    fn card_clip_is_none_when_the_card_contains_the_window() {
        assert_eq!(
            card_clip(rect(10, 10, 100, 100), rect(0, 0, 200, 200)),
            None
        );
        assert_eq!(
            card_clip(rect(10, 10, 100, 100), rect(10, 10, 100, 100)),
            None
        );
    }

    #[test]
    fn card_clip_normalizes_partial_overlap() {
        // The card starts a quarter into the window and ends halfway.
        let clip = card_clip(rect(0, 0, 100, 100), rect(25, 50, 25, 25)).unwrap();
        assert_eq!(clip, [0.25, 0.5, 0.5, 0.75]);

        // A window sticking out at the top-left has negative bounds.
        let clip = card_clip(rect(0, 0, 100, 100), rect(-50, -50, 300, 300));
        assert_eq!(clip, None);
        let clip = card_clip(rect(0, 0, 100, 100), rect(-50, -50, 100, 100)).unwrap();
        assert_eq!(clip, [-0.5, -0.5, 0.5, 0.5]);
    }
}
