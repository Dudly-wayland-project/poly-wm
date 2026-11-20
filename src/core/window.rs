// Ported from niri (GPL-3.0-or-later): src/window/unmapped.rs, src/handlers/compositor.rs

//! Window mapping bookkeeping.
//!
//! An xdg-toplevel exists in two states: unmapped (no buffer committed yet, or
//! the client committed a null buffer) and mapped (has a buffer). Smithay does
//! not reset `initial_configure_sent` when a surface unmaps, so Poly keeps its
//! own copy of the flag for the initial configure handshake.

use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{SERIAL_COUNTER, Serial};
use smithay::wayland::compositor::get_parent;
use smithay::wayland::seat::WaylandFocus as _;
use smithay::wayland::shell::xdg::ToplevelSurface;
use tracing::debug;

use crate::core::focus::set_window_focus;
use crate::core::handlers::misc::window_title_and_app_id;
use crate::core::layout::LayoutManager;
use crate::state::State;

/// A window that is not displayed yet.
pub struct UnmappedWindow {
    /// The window itself.
    pub window: Window,
    /// Whether we already sent the initial configure for the current mapping cycle.
    pub initial_configure_sent: bool,
}

impl UnmappedWindow {
    /// Wrap a freshly created window.
    pub fn new(window: Window) -> Self {
        return Self {
            window,
            initial_configure_sent: false,
        };
    }
}

/// Whether a surface has a buffer attached, i.e. will be shown.
pub fn is_mapped(surface: &WlSurface) -> bool {
    return with_renderer_surface_state(surface, |state| return state.buffer().is_some())
        .unwrap_or(false);
}

/// Walk up the subsurface tree to the surface the client created.
pub fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    return root;
}

/// Tell `surface` which scale to render at.
///
/// Two protocols carry the same information: `wl_surface.preferred_buffer_scale`
/// (integer, `wl_surface` v6) and `wp_fractional_scale_v1.preferred_scale`
/// (120ths). Clients that support neither ignore both and keep rendering at 1x.
///
/// The output transform is deliberately not forwarded: the winit backend reports
/// `Flipped180` to keep nested output upright on screen, and a client that
/// applied that transform would draw upside down.
pub fn send_scale(surface: &WlSurface, scale: smithay::output::Scale) {
    let fractional = scale.fractional_scale();
    smithay::wayland::compositor::with_states(surface, |states| {
        smithay::wayland::compositor::send_surface_state(
            surface,
            states,
            fractional.round() as i32,
            smithay::utils::Transform::Normal,
        );
        smithay::wayland::fractional_scale::with_fractional_scale(states, |state| {
            state.set_preferred_scale(fractional);
        });
    });
}

/// Find the toplevel window whose root surface is `root`.
///
/// The layout's root table is the source of truth, not the space: a minimized
/// window keeps its mapping state but is deliberately absent from the space, and
/// its commits, title changes and teardown must still reach the layout.
pub fn window_for_root(state: &State, root: &WlSurface) -> Option<Window> {
    return state
        .core
        .layout
        .by_root(root)
        .map(|managed| return managed.window.clone());
}

/// React to a commit of a root surface: map, unmap or just repaint.
pub fn on_root_commit(state: &mut State, root: &WlSurface) {
    // Not displayed yet: this commit might map it.
    if state.core.unmapped_windows.contains_key(root) {
        if is_mapped(root) {
            map_window(state, root);
        } else {
            send_initial_configure(state, root);
        }
        return;
    }

    let Some(window) = window_for_root(state, root) else {
        // Unknown root surface: a window we already dropped, or a tooltip-like
        // surface without a window. Nothing to do.
        return;
    };

    window.on_commit();
    LayoutManager::on_window_commit(state, &window);

    // A commit with a null buffer unmaps the window.
    if !is_mapped(root) {
        unmap_window(state, root, window);
    }
}

/// Send the initial configure, if this mapping cycle has not seen one yet.
fn send_initial_configure(state: &mut State, root: &WlSurface) {
    let (window, toplevel) = {
        let Some(unmapped) = state.core.unmapped_windows.get_mut(root) else {
            return;
        };
        if unmapped.initial_configure_sent {
            return;
        }
        unmapped.initial_configure_sent = true;
        (unmapped.window.clone(), unmapped.window.toplevel().cloned())
    };
    if let Some(toplevel) = toplevel {
        // A tiled window learns its slot now, so its first buffer already has
        // the right size and it maps straight into place.
        let size = LayoutManager::initial_size(state, &window);
        debug!("sending the initial configure with size {size:?}");
        apply_tiled_state(window_no_csd(state, &window), &toplevel);
        if let Some(size) = size {
            toplevel.with_pending_state(|pending| {
                pending.size = Some(size);
            });
        }
        toplevel.send_configure();
    }
}

/// The effective `no_csd` setting of a window: `general.prefer_no_csd`, unless
/// the first matching window rule overrides it.
pub fn window_no_csd(state: &State, window: &Window) -> bool {
    let identity = window_title_and_app_id(window);
    let (app_id, title) = identity.as_pair();
    return crate::core::render::visual::WindowVisuals::resolve(&state.core.config, app_id, title)
        .no_csd;
}

/// Whether the client should see its four edges marked `tiled`.
///
/// Ported from niri (`src/utils/mod.rs::update_tiled_state`): the tiled state
/// is what removes client-side rounded corners and shadows from clients that
/// ignore `xdg-decoration` (GTK 4, for instance), so it follows the same
/// intent as `no_csd`.
///
/// A client that negotiated `xdg-decoration` decides: answering `ClientSide`
/// means it keeps its own decorations, so its edges must *not* be reported as
/// tiled (it explicitly asked for CSD). Only when nothing was negotiated -- the
/// client never bound the global, or never created a decoration object -- the
/// window's resolved `no_csd` setting is the fallback.
///
/// The decision only looks at the already-locked pending state: the toplevel
/// attributes lock is not reentrant.
pub fn decide_tiled(pending: &smithay::wayland::shell::xdg::ToplevelState, no_csd: bool) -> bool {
    return match pending.decoration_mode {
        Some(mode) => mode == Mode::ServerSide,
        None => no_csd,
    };
}

/// Set or clear the four tiled edges of a toplevel.
pub fn set_tiled_states(pending: &mut smithay::wayland::shell::xdg::ToplevelState, tiled: bool) {
    for state in [
        ToplevelState::TiledLeft,
        ToplevelState::TiledRight,
        ToplevelState::TiledTop,
        ToplevelState::TiledBottom,
    ] {
        if tiled {
            pending.states.set(state);
        } else {
            pending.states.unset(state);
        }
    }
}

/// Tell the client its edges touch the screen, so GTK/Qt drop their CSD
/// shadows and rounded corners. Only meaningful with `no_csd`.
fn apply_tiled_state(no_csd: bool, toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|pending| {
        let tiled = decide_tiled(pending, no_csd);
        set_tiled_states(pending, tiled);
    });
}

/// Display a window that just got its first buffer.
///
/// The layout decides the rectangle before the window enters the space, so the
/// opening animation fades it in where it belongs instead of at a centered
/// fallback that the following reflow would immediately move away from.
pub fn map_window(state: &mut State, root: &WlSurface) {
    let Some(unmapped) = state.core.unmapped_windows.remove(root) else {
        return;
    };
    let UnmappedWindow {
        window,
        initial_configure_sent,
    } = unmapped;

    if !initial_configure_sent {
        // The client ignored the configure handshake; give it one anyway.
        if let Some(toplevel) = window.toplevel() {
            apply_tiled_state(window_no_csd(state, &window), toplevel);
            toplevel.send_configure();
        }
    }

    window.on_commit();

    let focus_on_map = LayoutManager::should_focus_on_map(state, &window);
    let window_id = LayoutManager::map_window(state, &window, root);
    create_foreign_toplevel(state, &window, root, window_id);
    if focus_on_map {
        focus_window(
            state,
            &window,
            Some(root.clone()),
            SERIAL_COUNTER.next_serial(),
        );
    } else {
        LayoutManager::relayout(state);
    }
    state.core.queue_redraw_all();
}

/// Publish the window to taskbars (`ext-foreign-toplevel-list-v1`).
///
/// The identifier is the same stable id the IPC reports, so a taskbar can use
/// it directly with `focus-window`/`close-window` instead of matching titles.
fn create_foreign_toplevel(state: &mut State, window: &Window, root: &WlSurface, id: u64) {
    let identity = window_title_and_app_id(window);
    let handle = state
        .core
        .foreign_toplevel_list_state
        .new_toplevel_with_identifier::<State>(
            identity.title.unwrap_or_default(),
            identity.app_id.unwrap_or_default(),
            id.to_string(),
        );
    // A window that unmapped and mapped again gets a new id, so the entry from
    // its previous mapping has to be closed here; otherwise a taskbar shows the
    // same window twice and keeps the older entry forever.
    let previous = state.core.foreign_toplevels.insert(root.clone(), handle);
    if let Some(previous) = previous {
        previous.send_closed();
    }
}

/// Update the taskbar entry after the client changed its title or app id.
pub fn update_foreign_toplevel(state: &mut State, root: &WlSurface) {
    let Some(handle) = state.core.foreign_toplevels.get(root) else {
        return;
    };
    let Some(window) = window_for_root(state, root) else {
        return;
    };
    let identity = window_title_and_app_id(&window);
    if let Some(title) = identity.title {
        handle.send_title(&title);
    }
    if let Some(app_id) = identity.app_id {
        handle.send_app_id(&app_id);
    }
    handle.send_done();
}

/// React to a destroyed toplevel: the window is gone, so is its place on screen.
pub fn on_window_destroyed(state: &mut State, root: &WlSurface) {
    debug!("window destroyed");

    // A surface destruction hook may have baked the last frame already (the
    // client can destroy the wl_surface before the xdg_toplevel, especially on
    // disconnect). Otherwise bake it now, while the buffer is still alive.
    let closing = state
        .core
        .layout
        .take_pending_unmap_closing(root)
        .or_else(|| {
            if state.core.layout.by_root(root).is_some() {
                take_closing_snapshot(state, root)
            } else {
                None
            }
        });

    // A window that never mapped is simply forgotten.
    state.core.unmapped_windows.remove(root);

    // Taskbars must not keep an entry for a window that is gone. Dropping the
    // handle is not enough: every `ExtForeignToplevelHandleV1` resource keeps a
    // strong clone in its user data, so the last reference goes away with the
    // client, not with the window, and `closed` would only be sent on
    // disconnect. Say it explicitly, or a taskbar keeps showing the app as
    // running after its last window closed.
    if let Some(handle) = state.core.foreign_toplevels.remove(root) {
        handle.send_closed();
    }

    // The keyboard may be pointing at the window that just died.
    let held_focus = keyboard_focused_on(state, root);
    // Remember where it was before the layout forgets it. Focus fallback must
    // stay on that workspace instead of picking whatever window is topmost in
    // the whole space.
    let previous_workspace = state
        .core
        .layout
        .by_root(root)
        .and_then(|managed| return state.core.layout.workspace_of(managed.id));
    // The layout neighbour that should inherit the keyboard. Looked up before
    // `remove_window`, because the closed window has to still be in the list
    // for its position to tell us which window came before it.
    let preferred_focus = focus_fallback(state, root);

    // The layout must forget the window before `refresh` drops it from the
    // space, otherwise it would keep a dead element in its ordering.
    LayoutManager::remove_window(state, root);
    if let Some(closing) = closing {
        LayoutManager::start_closing(state, closing);
    }

    // Dead windows are dropped by `refresh`; it may also drop others whose
    // clients are gone by now.
    state.core.space.refresh();

    if held_focus {
        focus_next_window(state, previous_workspace, preferred_focus);
    }

    LayoutManager::relayout(state);
    crate::core::focus::update_keyboard_focus(state);
    state.core.queue_redraw_all();
}

/// Snapshot a mapped window when any of its surfaces is being destroyed.
///
/// This runs from a destruction hook installed when the wl_surface is created,
/// before Smithay's renderer-state reset hook (which is added on the first
/// buffer commit). That ordering keeps the last buffer texture alive long
/// enough to bake it even when the client destroys the surface (or disconnects)
/// without first committing a null buffer. The snapshot is stashed under the
/// root surface and consumed by [`on_window_destroyed`].
pub(crate) fn on_surface_destroyed(state: &mut State, surface: &WlSurface) {
    let root = {
        let layout = &state.core.layout;
        layout
            .root_for_surface(surface)
            .or_else(|| {
                return layout
                    .by_root(surface)
                    .is_some()
                    .then(|| return surface.clone());
            })
            .unwrap_or_else(|| return root_surface(surface))
    };

    if state.core.layout.by_root(&root).is_some()
        && !state.core.layout.has_pending_unmap_closing(&root)
    {
        if let Some(closing) = take_closing_snapshot(state, &root) {
            state
                .core
                .layout
                .stash_pending_unmap_closing(&root, closing);
        }
    }

    // The surface (and, if it was the root, everything recorded below it) is
    // dead now. Keep the table from growing across client lifetimes.
    state.core.layout.forget_surface(surface);
}

/// Bake the last visible frame of a window for the close animation.
///
/// Runs while the client buffer is still attached: from toplevel destruction,
/// or from the root pre-commit hook when a client commits a null buffer
/// (minimize/hide). Returns `None` when animations are off, the window was
/// never mapped, the backend has no GLES renderer (headless), or the surface
/// already lost its buffer; cleanup then proceeds without animation.
pub(crate) fn take_closing_snapshot(
    state: &mut State,
    root: &WlSurface,
) -> Option<crate::core::layout::ClosingWindow> {
    let spec = state.core.config.general.animations.window_close();
    if spec.duration.is_zero() {
        return None;
    }
    let now = std::time::Instant::now();
    let (id, output, workspace, tiled, fullscreen, rect, alpha, open_scale, visuals, window) = {
        let managed = state.core.layout.by_root(root)?;
        // A minimized window is not on screen: there is no frame to bake, and
        // closing it must not pop a snapshot out of nowhere.
        if managed.minimized {
            return None;
        }
        let output = managed.output.clone()?;
        let (_, workspace) = state.core.layout.workspace_of(managed.id)?;
        (
            managed.id,
            output,
            workspace,
            managed.tiled,
            managed.fullscreen == Some(crate::core::layout::FullscreenMode::Fullscreen),
            // The live window may still be scaling in; bake the frame the user
            // actually sees instead of jumping to the target size at close.
            managed.open_rect_at(now),
            managed.alpha_at(now) * managed.visuals.opacity,
            managed.open_scale_at(now),
            managed.visuals.clone(),
            managed.window.clone(),
        )
    };
    let output_geometry = state.core.space.output_geometry(&output)?;
    let output_scale = output.current_scale().fractional_scale();
    let snapshot = {
        let info = crate::core::render::SnapshotWindow {
            id,
            window,
            root: root.clone(),
            rect,
            alpha,
            scale: open_scale,
            fullscreen: fullscreen.then_some(crate::core::layout::FullscreenMode::Fullscreen),
            visuals,
        };
        let crate::state::State { backend, core, .. } = state;
        backend
            .with_gles_renderer_for_output(&output, |renderer| {
                crate::core::render::snapshot_window(
                    core,
                    renderer,
                    &info,
                    output_geometry,
                    output_scale,
                )
            })
            .flatten()
    };
    let Some(snapshot) = snapshot else {
        debug!("close animation skipped: could not bake the last window frame (id {id})");
        return None;
    };
    debug!("close snapshot baked for window {id}");
    // `snapshot_window` bakes in workspace-local coordinates (it shifts the
    // window rect to the output before `push_window` subtracts it again).
    // Keep the snapshot rect local too, matching `window_rect`; `push_closing`
    // applies the workspace projection to both.
    let snapshot_rect = snapshot.bounds;
    // The animation clock is set in `start_closing`, i.e. after this bake and
    // after the window is actually removed. Baking can take a while (notably
    // on llvmpipe); starting earlier would let it consume the whole duration.
    return Some(crate::core::layout::ClosingWindow::new(
        output,
        workspace,
        tiled,
        fullscreen,
        rect,
        snapshot_rect,
        snapshot.source_size,
        snapshot.buffer,
        spec,
    ));
}

/// Whether the keyboard is currently giving focus to `root`.
fn keyboard_focused_on(state: &State, root: &WlSurface) -> bool {
    return state
        .core
        .seat
        .get_keyboard()
        .and_then(|keyboard| return keyboard.current_focus())
        .is_some_and(|surface| return root_surface(&surface) == *root);
}

/// Move the keyboard focus to a remaining window, or to nobody.
///
/// `workspace` is where the removed window lived. When it is known, only
/// windows on that workspace are candidates: closing a window must not move
/// focus to a window on a workspace that is currently off screen.
///
/// `preferred` is the layout neighbour the closed window handed focus to (see
/// [`LayoutManager::focus_fallback`]), resolved before the window left the
/// ordering. Without it, or when that window is gone too, the topmost window
/// of the workspace takes over, and finally the topmost window in the space.
fn focus_next_window(
    state: &mut State,
    workspace: Option<(smithay::output::Output, usize)>,
    preferred: Option<u64>,
) {
    let next = preferred
        .and_then(|id| return state.core.layout.window_handle(id))
        .or_else(|| return topmost_window(state, workspace));
    match next {
        Some(next) => {
            let surface = next.wl_surface().map(|surface| return surface.into_owned());
            focus_window(state, &next, surface, SERIAL_COUNTER.next_serial());
        }
        None => {
            if let Some(keyboard) = state.core.seat.get_keyboard() {
                keyboard.set_focus(state, None, SERIAL_COUNTER.next_serial());
            }
        }
    }
}

/// The topmost remaining window of `workspace`, or of the whole space.
fn topmost_window(
    state: &State,
    workspace: Option<(smithay::output::Output, usize)>,
) -> Option<Window> {
    let Some((output, index)) = workspace else {
        return state.core.space.elements().last().cloned();
    };
    let Some(ids) = state.core.layout.workspace_ids(&output, index) else {
        return state.core.space.elements().last().cloned();
    };
    // Space elements are stored back to front; the last one is on top.
    return state
        .core
        .space
        .elements()
        .rev()
        .find(|window| {
            return state
                .core
                .layout
                .by_window(window)
                .is_some_and(|managed| return ids.contains(&managed.id));
        })
        .cloned();
}

/// The window `root` hands the keyboard to when it goes away, looked up while
/// it is still part of the workspace ordering.
fn focus_fallback(state: &State, root: &WlSurface) -> Option<u64> {
    let managed = state.core.layout.by_root(root)?;
    let (output, index) = state.core.layout.workspace_of(managed.id)?;
    return state.core.layout.focus_fallback(&output, index, managed.id);
}

/// Put a window back into the unmapped set and refund its buffers.
pub fn unmap_window(state: &mut State, root: &WlSurface, window: Window) {
    debug!("window unmapped");
    let held_focus = keyboard_focused_on(state, root);
    // Baked by the null-buffer pre-commit hook while the old buffer was still
    // alive. Destroy-after-unmap does not have a second chance to see it.
    let closing = state.core.layout.take_pending_unmap_closing(root);
    let previous_workspace = state
        .core
        .layout
        .by_root(root)
        .and_then(|managed| return state.core.layout.workspace_of(managed.id));
    // Same as in `on_window_destroyed`: resolve the layout neighbour while the
    // window still has a position in the list.
    let preferred_focus = focus_fallback(state, root);

    LayoutManager::remove_window(state, root);
    if let Some(closing) = closing {
        LayoutManager::start_closing(state, closing);
    }
    state.core.space.unmap_elem(&window);
    window.set_activated(false);

    // A newly-unmapped toplevel must run the initial configure sequence afresh.
    state
        .core
        .unmapped_windows
        .insert(root.clone(), UnmappedWindow::new(window));

    // The keyboard must not point at a window that left the screen.
    if held_focus {
        focus_next_window(state, previous_workspace, preferred_focus);
    }

    LayoutManager::relayout(state);
    crate::core::focus::update_keyboard_focus(state);
    state.core.queue_redraw_all();
}

/// Make `window` the focused window and give the keyboard to `surface`.
pub fn focus_window(
    state: &mut State,
    window: &Window,
    surface: Option<WlSurface>,
    serial: Serial,
) {
    for element in state.core.space.elements() {
        if element == window {
            continue;
        }
        // `relayout` below sends the configure that carries both the new
        // `Activated` state and the tiled state of every window, so there is
        // no need to configure here.
        element.set_activated(false);
    }

    window.set_activated(true);

    // Remember the window focus so that on-demand layers and popups can hand
    // the keyboard back to it (see `core::focus`).
    let root = surface.clone().or_else(|| {
        return window
            .toplevel()
            .map(|toplevel| return toplevel.wl_surface().clone());
    });
    set_window_focus(state, root);

    let Some(keyboard) = state.core.seat.get_keyboard() else {
        return;
    };
    keyboard.set_focus(state, surface, serial);

    // Reflow and update the `Activated` state of every window. Focus changes
    // the tiled insertion point, and clients expect a configure.
    LayoutManager::relayout(state);
}
