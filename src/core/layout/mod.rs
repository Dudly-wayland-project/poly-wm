// Ported from niri (GPL-3.0-or-later): src/layout/*, src/window/*

//! Window state and layout.
//!
//! This module is the compositor's source of truth for where a mapped window
//! is: whether it is tiled or floating, which output it belongs to, its
//! target rectangle and the animation towards that rectangle. The built-in
//! fallback is a row of equal-width columns; named scripted layouts replace it
//! per output when configured.
//!
//! Geometry changes go through a configure transaction: the old size stays on
//! screen until every client in the reflow acknowledged its new size, so a
//! layout switch does not tear (`layout/transaction.rs`).

mod transaction;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use poly_config::{
    AnimationSpec, Animations, Config, LayoutContext, LayoutEffects, LayoutOutput, LayoutPrograms,
    LayoutRect, LayoutWindow, LayoutWorkspace, WindowRule,
};
use smithay::backend::renderer::element::texture::TextureBuffer;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as ToplevelState;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::Resource as _;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Size};
use smithay::wayland::compositor::{
    BufferAssignment, CompositorHandler as _, HookId, SurfaceAttributes, add_blocker,
    add_pre_commit_hook, remove_pre_commit_hook, with_states,
};
use smithay::wayland::seat::WaylandFocus as _;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use tracing::{debug, trace, warn};

use crate::core::focus::{set_window_focus, update_keyboard_focus};
use crate::core::handlers::layer_shell::work_area;
use crate::core::handlers::misc::window_title_and_app_id;
use crate::core::window::{focus_window, root_surface};
use crate::state::State;
use transaction::{Transaction, TransactionWatcher};

/// The compositor-managed fullscreen state of a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullscreenMode {
    /// The compositor gives the window the whole output and covers Top layers.
    Fullscreen,
    /// The client believes it is fullscreen, but the layout still places it.
    Windowed,
}

/// A mapped window and the state the layout engine owns for it.
pub struct ManagedWindow {
    /// Stable id, also used by the scripted layout bridge.
    pub id: u64,
    /// The window itself.
    pub window: Window,
    /// Root surface.
    pub root: WlSurface,
    /// The output the window belongs to.
    pub output: Option<smithay::output::Output>,
    /// Whether the layout places it. Floating windows keep their own rect.
    pub tiled: bool,
    /// Whether it is minimized.
    ///
    /// A minimized window keeps its place in its workspace's tiled or floating
    /// list, so the IPC window list and the taskbar keep it, but it is removed
    /// from the space: it is not drawn, hit-tested or sent frame callbacks, and
    /// the layout reflows over its slot. Focusing it restores it.
    pub minimized: bool,
    /// Whether it is fullscreen.
    pub fullscreen: Option<FullscreenMode>,
    /// The rectangle to return to when a floating window stops being
    /// fullscreen.
    floating_rect: Rectangle<i32, Logical>,
    /// The rectangle the current animation is heading to.
    target_rect: Rectangle<i32, Logical>,
    /// Geometry animation in progress.
    animation: Option<Animation>,
    /// While a tiled window is being reordered, the rectangle that follows the
    /// pointer. `rect_at` returns this instead of the layout target, so the
    /// window stays under the cursor while the layout reflows around it.
    drag_rect: Option<Rectangle<i32, Logical>>,
    /// The slot the dragged tiled window would land in (the drop indicator).
    drop_preview: Option<Rectangle<i32, Logical>>,
    /// The open fade/scale animation, if one is running.
    open_animation: Option<OpenAnimation>,
    /// A configure this window has not acknowledged yet.
    pending: Option<WindowPending>,
    /// The interactive resize of this window, if one is running (the pointer
    /// grab only produces rectangles; the state lives here, like niri's
    /// `InteractiveResize`).
    resize: Option<InteractiveResize>,
    /// Whether the next frame boundary must send this window a frame callback
    /// regardless of throttling or visibility.
    ///
    /// Set after every configure (niri's `needs_frame_callback`): a client
    /// that renders on frame callbacks would otherwise wait for the next
    /// throttled callback before it can draw the new size, which is the
    /// half-frame lag of a GTK window while it is resized.
    needs_frame_callback: bool,
    /// Resolved visual settings: radius, opacity, border, shadow, blur and the
    /// window shader.
    pub visuals: crate::core::render::visual::WindowVisuals,
}

struct WindowPending {
    serial: Serial,
    transaction: Transaction,
}

/// Which edges of a window stay on screen while the pointer drags the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeAnchor {
    /// The right edge is anchored, the left edge follows the pointer.
    pub fixed_right: bool,
    /// The bottom edge is anchored, the top edge follows the pointer.
    pub fixed_bottom: bool,
}

/// One interactive resize, from the first pointer motion past the threshold
/// until the client committed the final size.
///
/// A resize is not a layout animation: the compositor asks for a new size at
/// most once per client commit and draws whatever the client currently has,
/// anchored on the edges the pointer does not move. Sending a configure for
/// every pointer frame makes clients fall further and further behind (a 1000 Hz
/// mouse sends ~1000 size requests per second); drawing the committed size
/// instead of stretching the old buffer to the requested one is what keeps the
/// window sharp while it grows.
struct InteractiveResize {
    /// Which edges stay put.
    anchor: ResizeAnchor,
    /// The corner that stays on screen: the anchored `x`/`y`.
    origin: Point<i32, Logical>,
    /// The size the pointer asks for.
    desired: Size<i32, Logical>,
    /// The size the client acknowledged last (what the throttle compares).
    committed: Size<i32, Logical>,
    /// The window geometry of the client's last commit (what is drawn). It
    /// differs from `committed` when the client clamps the size it was asked
    /// for; drawing `committed` would then stretch the buffer.
    drawn: Size<i32, Logical>,
    /// The size in the last configure this resize sent.
    sent: Option<Size<i32, Logical>>,
    /// The grab ended; the final configure is on its way and `Resizing` is
    /// cleared on the next commit (or after [`RESIZE_END_TIMEOUT`]).
    end_since: Option<Instant>,
}

impl InteractiveResize {
    /// The requested size, at least one pixel per axis.
    fn size(&self) -> Size<i32, Logical> {
        return Size::from((self.desired.w.max(1), self.desired.h.max(1)));
    }

    /// Whether a new size configure may go out now (niri's `CanSend`).
    ///
    /// Only size changes are throttled: while the client still draws a size
    /// other than the one the previous configure asked for, another configure
    /// would only queue work it cannot keep up with.
    fn caught_up(&self) -> bool {
        return self.sent.is_none_or(|sent| return self.committed == sent);
    }

    /// The rectangle `size` occupies with the anchored edges fixed.
    fn rect_for(&self, size: Size<i32, Logical>) -> Rectangle<i32, Logical> {
        let x = if self.anchor.fixed_right {
            self.origin.x - size.w
        } else {
            self.origin.x
        };
        let y = if self.anchor.fixed_bottom {
            self.origin.y - size.h
        } else {
            self.origin.y
        };
        return Rectangle::new(Point::from((x, y)), size);
    }
}

/// How long an ended resize waits for the last commit before it clears
/// `Resizing` anyway. A client that never commits the final size (it started a
/// nested event loop, say) must not keep the state forever.
const RESIZE_END_TIMEOUT: Duration = Duration::from_millis(250);

/// The size the client has acknowledged last.
///
/// The acked configure state (not the surface geometry) is what the throttle
/// compares against: for a `no_csd` window with server-side decorations the
/// geometry matches the configure, but a client that sets a window geometry
/// inset by its own shadows would otherwise never look "caught up".
fn committed_size(window: &Window) -> Size<i32, Logical> {
    let acked = window.toplevel().and_then(|toplevel| {
        return toplevel.with_cached_state(|state| {
            return state
                .last_acked
                .as_ref()
                .map(|configure| return configure.state.size);
        });
    });
    let size = acked
        .flatten()
        .unwrap_or_else(|| return window.geometry().size);
    return Size::from((size.w.max(1), size.h.max(1)));
}

/// The window geometry the client committed last, at least one pixel per axis.
fn drawn_size(window: &Window) -> Size<i32, Logical> {
    let size = window.geometry().size;
    return Size::from((size.w.max(1), size.h.max(1)));
}

/// Ask the client for `size`; `resizing` also sets or clears the protocol state
/// that tells it the compositor is dragging an edge.
fn send_resize_configure(managed: &mut ManagedWindow, size: Size<i32, Logical>, resizing: bool) {
    let Some(toplevel) = managed.window.toplevel() else {
        return;
    };
    toplevel.with_pending_state(|pending| {
        pending.size = Some(size);
        if resizing {
            pending.states.set(ToplevelState::Resizing);
        } else {
            pending.states.unset(ToplevelState::Resizing);
        }
    });
    if toplevel.send_pending_configure().is_some() {
        managed.needs_frame_callback = true;
    }
}

/// The niri-style opening animation: fade in while scaling up from 50%.
struct OpenAnimation {
    started: Instant,
    spec: AnimationSpec,
}

struct Animation {
    from: Rectangle<i32, Logical>,
    to: Rectangle<i32, Logical>,
    started: Instant,
    /// Position easing. `None` when only the size changed.
    movement: Option<AnimationSpec>,
    /// Size easing. `None` when only the position changed.
    resize: Option<AnimationSpec>,
}

/// A window that was closed while its last frame is still fading out.
///
/// Client buffers disappear when the toplevel is destroyed, so the compositor
/// bakes the window (content, border and shadow) into a texture before removing
/// it. The texture is drawn at the window's workspace position and scaled down
/// while it fades out, exactly like niri's `ClosingWindow`.
pub struct ClosingWindow {
    /// Output it was on.
    pub output: smithay::output::Output,
    /// Workspace index inside that output's stack.
    pub workspace: usize,
    /// Whether it was laid out by the layout (affects stacking).
    pub tiled: bool,
    /// Whether it was a compositor-managed fullscreen window.
    pub fullscreen: bool,
    /// The window rectangle when it closed, in workspace-local coordinates
    /// (the projection to screen space happens in `push_closing`).
    pub window_rect: Rectangle<i32, Logical>,
    /// Bounds of the baked texture (window + border + shadow), in workspace-local
    /// coordinates.
    pub snapshot_rect: Rectangle<i32, Logical>,
    /// Size of the baked texture in buffer pixels.
    ///
    /// The buffer is created with scale 1, so this is also the logical size of
    /// the source rectangle sampled by the closing render element. Without it,
    /// the element would sample only the top-left part of the texture (and
    /// crop instead of scale the window).
    pub source_size: Size<i32, Logical>,
    /// The baked window content.
    pub buffer: TextureBuffer<GlesTexture>,
    started: Instant,
    spec: AnimationSpec,
}

impl ClosingWindow {
    /// Create a closing snapshot. The animation clock is started (or restarted)
    /// by [`LayoutManager::start_closing`] when the window is actually removed.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        output: smithay::output::Output,
        workspace: usize,
        tiled: bool,
        fullscreen: bool,
        window_rect: Rectangle<i32, Logical>,
        snapshot_rect: Rectangle<i32, Logical>,
        source_size: Size<i32, Logical>,
        buffer: TextureBuffer<GlesTexture>,
        spec: AnimationSpec,
    ) -> Self {
        return Self {
            output,
            workspace,
            tiled,
            fullscreen,
            window_rect,
            snapshot_rect,
            source_size,
            buffer,
            started: Instant::now(),
            spec,
        };
    }

    /// Eased close progress (`0.0` still visible, `1.0` gone).
    pub fn progress(&self, now: Instant) -> f64 {
        return self
            .spec
            .progress(now.saturating_duration_since(self.started));
    }

    /// Niri's close animation: opacity `1 -> 0`, scale `1.0 -> 0.8`.
    pub fn alpha_at(&self, now: Instant) -> f32 {
        return (1.0 - self.progress(now)) as f32;
    }

    /// Current scale of the baked snapshot, around the window centre.
    pub fn scale_at(&self, now: Instant) -> f64 {
        let progress = self.progress(now);
        return ((1.0 - progress) / 5.0 + 0.8).max(0.0);
    }

    /// Whether this closing window still needs frames.
    pub fn running(&self, now: Instant) -> bool {
        return self
            .spec
            .running(now.saturating_duration_since(self.started));
    }
}

/// One workspace in an output's vertical stack.
///
/// A workspace owns its windows and its layout name: switching workspaces
/// does not re-flow the windows on the other ones. `focus` remembers what had
/// the keyboard when the workspace was left, so coming back restores it.
#[derive(Debug, Default)]
struct Workspace {
    /// Stable id handed to scripts as `ctx.workspace.id` (0 until assigned).
    ///
    /// Ids follow a workspace when it moves in the stack, so a layout can keep
    /// per-workspace state across `move-workspace-up`/`-down`.
    id: u64,
    tiled: Vec<u64>,
    floating: Vec<u64>,
    layout: String,
    focus: Option<u64>,
}

impl Workspace {
    fn with_layout(name: String) -> Self {
        return Self {
            id: 0,
            tiled: Vec::new(),
            floating: Vec::new(),
            layout: name,
            focus: None,
        };
    }

    fn contains(&self, id: u64) -> bool {
        return self.tiled.contains(&id) || self.floating.contains(&id);
    }
}

/// The window `workspace` should focus, skipping minimized windows.
///
/// Floating windows come first (they are on top), then the last tiled one.
/// This is [`Workspace`]'s preferred focus with the windows the user hid left
/// out; it cannot live on `Workspace` because that type does not see the
/// window table.
fn visible_focus(state: &State, workspace: &Workspace) -> Option<u64> {
    let visible = |id: &u64| {
        return state
            .core
            .layout
            .windows
            .get(id)
            .is_some_and(|window| return !window.minimized);
    };
    let remembered = workspace.focus.filter(|id| {
        return workspace.contains(*id) && visible(id);
    });
    return remembered.or_else(|| {
        return workspace
            .floating
            .iter()
            .rev()
            .chain(workspace.tiled.iter().rev())
            .copied()
            .find(|id| return visible(id));
    });
}

/// Where focus should go when `id` disappears from a workspace.
///
/// The order is: the window before `id` in the tiled list (its left neighbour
/// in a column layout), then the window after it (when it was first), then the
/// same two slots in the floating list, then the other list's last window.
///
/// Walking the list order rather than the space stack is what makes closing a
/// window feel local: the space stack is mapping order, so it would focus
/// whatever window happened to be mapped last.
fn focus_candidates(tiled: &[u64], floating: &[u64], id: u64) -> Vec<u64> {
    let neighbours = |list: &[u64]| {
        let mut found = Vec::new();
        let Some(position) = list.iter().position(|entry| return *entry == id) else {
            return found;
        };
        if let Some(before) = position.checked_sub(1).and_then(|at| return list.get(at)) {
            found.push(*before);
        }
        if let Some(after) = list.get(position + 1) {
            found.push(*after);
        }
        return found;
    };
    let mut candidates = neighbours(tiled);
    candidates.extend(neighbours(floating));
    let other_list = if floating.contains(&id) {
        tiled.last()
    } else {
        floating.last()
    };
    candidates.extend(other_list.copied());
    return candidates;
}

/// The vertical workspace stack of one output.
#[derive(Debug)]
struct OutputLayout {
    workspaces: Vec<Workspace>,
    /// Index of the workspace that owns the screen.
    active: usize,
    /// Slide between two workspaces, if one is running.
    switch: Option<WorkspaceSwitch>,
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceSwitch {
    from: f64,
    to: f64,
    started: Instant,
    spec: AnimationSpec,
}

impl OutputLayout {
    fn with_layout(name: String) -> Self {
        return Self {
            workspaces: vec![Workspace::with_layout(name)],
            active: 0,
            switch: None,
        };
    }

    /// Append an empty workspace and return its index.
    fn push_workspace(&mut self, layout: String) -> usize {
        self.workspaces.push(Workspace::with_layout(layout));
        return self.workspaces.len() - 1;
    }

    /// Make sure workspace `index` exists, filling the gap with empty ones.
    fn ensure_workspace(&mut self, index: usize, layout: &str) {
        while self.workspaces.len() <= index {
            self.push_workspace(layout.to_owned());
        }
    }

    fn ws(&self) -> &Workspace {
        let index = self.active.min(self.workspaces.len().saturating_sub(1));
        return &self.workspaces[index];
    }

    fn ws_mut(&mut self) -> &mut Workspace {
        let index = self.active.min(self.workspaces.len().saturating_sub(1));
        return &mut self.workspaces[index];
    }

    /// The interpolated index drawn on screen while a switch animation runs.
    fn render_index(&self, now: Instant) -> f64 {
        let Some(switch) = &self.switch else {
            return self.active as f64;
        };
        let progress = switch
            .spec
            .progress(now.saturating_duration_since(switch.started));
        return switch.from + (switch.to - switch.from) * progress;
    }

    fn is_switching(&self, now: Instant) -> bool {
        return self.switch.as_ref().is_some_and(|switch| {
            return switch
                .spec
                .running(now.saturating_duration_since(switch.started));
        });
    }

    /// Drop a finished switch. Returns whether something was dropped.
    fn finish_switch(&mut self, now: Instant) -> bool {
        if self.is_switching(now) || self.switch.is_none() {
            return false;
        }
        self.switch = None;
        return true;
    }
}

/// One workspace card: where a workspace sits on screen, in compositor
/// coordinates. Only a workspace slide moves the cards off the output.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceCard {
    /// The card rectangle drawn on screen.
    pub rect: Rectangle<i32, Logical>,
}

/// Geometry of an output's workspace stack at one instant.
#[derive(Debug, Clone)]
pub struct WorkspaceView {
    /// The output origin, in compositor coordinates.
    pub origin: Point<i32, Logical>,
    /// Cards, ordered by workspace index.
    pub cards: Vec<WorkspaceCard>,
}

impl WorkspaceView {
    /// Where a workspace-local rectangle is drawn on screen.
    ///
    /// For the active workspace outside a slide this is exactly `local`
    /// (the card sits on the output); a sliding card only translates it.
    pub fn project(
        &self,
        workspace: usize,
        local: Rectangle<i32, Logical>,
    ) -> Option<Rectangle<i32, Logical>> {
        let card = self.cards.get(workspace)?;
        let offset = card.rect.loc - self.origin;
        return Some(Rectangle::new(local.loc + offset, local.size));
    }
}

/// A window placed on an output, with its projected (screen) rectangle.
pub struct PlacedWindow<'a> {
    /// The window and its layout state.
    pub managed: &'a ManagedWindow,
    /// Where it is drawn, in compositor coordinates.
    pub rect: Rectangle<i32, Logical>,
    /// The workspace-switch card this window belongs to, in
    /// compositor coordinates. Windows are clipped to it so a floating window
    /// cannot paint over a neighbouring card.
    pub card: Rectangle<i32, Logical>,
}

/// One workspace of one output, as reported over IPC.
pub struct WorkspaceSnapshot {
    /// 0-based index in the output's vertical stack.
    pub index: usize,
    /// Whether this workspace owns the output's screen.
    pub active: bool,
    /// Name of the layout arranging it.
    pub layout: String,
    /// The window it remembers, or `None` before anything was focused there.
    pub focus: Option<u64>,
    /// Tiled windows, in layout order.
    pub tiled: Vec<u64>,
    /// Floating windows, in stacking order.
    pub floating: Vec<u64>,
    /// Windows in those lists that are minimized.
    ///
    /// They stay in the lists so the taskbar can still show and restore them;
    /// this tells consumers which ids the layout does not currently place.
    pub minimized: Vec<u64>,
}

/// One managed window, as reported over IPC.
pub struct WindowSnapshot {
    /// The stable id.
    pub id: u64,
    /// `xdg_toplevel.set_title`.
    pub title: Option<String>,
    /// `xdg_toplevel.set_app_id`.
    pub app_id: Option<String>,
    /// Output it is on.
    pub output: Option<smithay::output::Output>,
    /// 0-based index in that output's stack.
    pub workspace: Option<usize>,
    /// Whether the layout places it.
    pub tiled: bool,
    /// Whether it is minimized.
    pub minimized: bool,
    /// Fullscreen mode, when fullscreen.
    pub fullscreen: Option<FullscreenMode>,
}

struct PendingLayout {
    targets: HashMap<u64, Rectangle<i32, Logical>>,
    watcher: TransactionWatcher,
    /// Clients whose surface may have been blocked by this transaction. Smithay
    /// applies blocked commits only when told that a blocker was released.
    clients: Vec<Client>,
}

/// A superseded layout transaction whose blocked clients still have to be
/// notified once it finishes.
struct ReleasedTransaction {
    watcher: TransactionWatcher,
    clients: Vec<Client>,
}

/// Where a tiled window sat before an interactive drag pulled it out of the
/// layout. Escape puts it back here.
#[derive(Debug, Clone)]
pub struct TiledSlot {
    /// The output whose tiled list held the window.
    pub output: smithay::output::Output,
    /// The workspace in that output's stack.
    pub workspace: usize,
    /// Its index in that list.
    pub index: usize,
    /// Its visible rectangle at the start of the drag.
    pub rect: Rectangle<i32, Logical>,
}

/// The layout engine's state, owned by [`crate::core::Core`].
pub struct LayoutManager {
    windows: HashMap<u64, ManagedWindow>,
    roots: HashMap<WlSurface, u64>,
    /// Every surface seen in a commit mapped to its root surface. The parent
    /// link of a destroyed subsurface is already cleared when Smithay invokes
    /// destruction hooks, so this table is what lets the close snapshot find
    /// the window while the surface tree is being torn down.
    surface_roots: HashMap<WlSurface, WlSurface>,
    outputs: HashMap<smithay::output::Output, OutputLayout>,
    hooks: HashMap<WlSurface, HookId>,
    next_id: u64,
    /// Next id for a workspace. Ids are assigned lazily because workspaces are
    /// created in code that only borrows an [`OutputLayout`].
    next_workspace_id: u64,
    programs: Option<LayoutPrograms>,
    layout_names: Vec<String>,
    default_layout: String,
    animations: Animations,
    pending: Option<PendingLayout>,
    /// Superseded transactions whose blocked clients still need a
    /// `blocker_cleared` notification once they finish.
    released: Vec<ReleasedTransaction>,
    animation_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Whether the previous tick was still driving at least one animation.
    ///
    /// Used to force one last repaint (and space sync) at the exact target
    /// state: a tick usually lands slightly *after* the duration, so without
    /// this the window could stay frozen one frame short of 100% alpha/scale.
    animations_running: bool,
    dirty_space: bool,
    warned: HashSet<String>,
    /// Windows that were destroyed but are still fading out as baked
    /// snapshots.
    closing: Vec<ClosingWindow>,
    /// Close snapshots baked in a pre-commit hook when the client attaches a
    /// null buffer. The buffer is still alive at that point; the snapshot is
    /// consumed by [`crate::core::window::unmap_window`] after the commit.
    pending_unmap_closing: HashMap<WlSurface, ClosingWindow>,
}

impl LayoutManager {
    /// Create the manager from the initial configuration and its scripted
    /// layouts.
    pub fn new(config: &Config, programs: Option<LayoutPrograms>) -> Self {
        return Self {
            windows: HashMap::new(),
            roots: HashMap::new(),
            surface_roots: HashMap::new(),
            outputs: HashMap::new(),
            hooks: HashMap::new(),
            next_id: 1,
            next_workspace_id: 1,
            programs,
            layout_names: config
                .layouts
                .iter()
                .map(|layout| return layout.name.clone())
                .collect(),
            default_layout: config
                .default_layout
                .clone()
                .unwrap_or_else(|| "columns".to_owned()),
            animations: config.general.animations,
            pending: None,
            released: Vec::new(),
            animation_timer: None,
            animations_running: false,
            dirty_space: false,
            warned: HashSet::new(),
            closing: Vec::new(),
            pending_unmap_closing: HashMap::new(),
        };
    }

    /// Install a reloaded configuration (and its new script state).
    pub fn set_config(&mut self, config: &Config, programs: Option<LayoutPrograms>) {
        self.programs = programs;
        self.layout_names = config
            .layouts
            .iter()
            .map(|layout| return layout.name.clone())
            .collect();
        self.default_layout = config
            .default_layout
            .clone()
            .unwrap_or_else(|| "columns".to_owned());
        self.animations = config.general.animations;
        self.warned.clear();
        // Visual settings come from `general` plus the first matching rule, so
        // a reload has to re-resolve them for every window: the client is not
        // going to send another title or app id.
        for window in self.windows.values_mut() {
            let identity = window_title_and_app_id(&window.window);
            let (app_id, title) = identity.as_pair();
            window.visuals =
                crate::core::render::visual::WindowVisuals::resolve(config, app_id, title);
        }
        let names = self.layout_names.clone();
        let programs = self.programs.as_ref();
        let valid = |name: &str| {
            return name == "columns"
                || names.iter().any(|entry| return entry == name)
                || programs.is_some_and(|programs| return programs.has(name));
        };
        let default = self.default_layout.clone();
        for layout in self.outputs.values_mut() {
            for workspace in &mut layout.workspaces {
                if !valid(&workspace.layout) {
                    workspace.layout = default.clone();
                }
            }
        }
    }

    /// The ids of every managed window, for pruning per-window caches.
    pub fn window_ids(&self) -> HashSet<u64> {
        return self.windows.keys().copied().collect();
    }

    /// The managed window whose root surface is `root`.
    pub fn by_root(&self, root: &WlSurface) -> Option<&ManagedWindow> {
        let id = self.roots.get(root)?;
        return self.windows.get(id);
    }

    /// The managed window for a Smithay window handle.
    pub fn by_window(&self, window: &Window) -> Option<&ManagedWindow> {
        let surface = window.wl_surface()?;
        return self.by_root(&root_surface(&surface));
    }

    /// Consume the forced frame callback request of `window`, if it has one.
    pub fn take_frame_callback_request(&mut self, window: &Window) -> bool {
        let Some(surface) = window.wl_surface() else {
            return false;
        };
        let Some(id) = self.roots.get(&root_surface(&surface)).copied() else {
            return false;
        };
        return self
            .windows
            .get_mut(&id)
            .is_some_and(|managed| return std::mem::take(&mut managed.needs_frame_callback));
    }

    /// The Smithay handle of the managed window `id`.
    pub fn window_handle(&self, id: u64) -> Option<Window> {
        return self
            .windows
            .get(&id)
            .map(|managed| return managed.window.clone());
    }

    /// The index of the workspace that owns `output`'s screen.
    pub fn active_workspace(&self, output: &smithay::output::Output) -> usize {
        return self
            .outputs
            .get(output)
            .map(|layout| return layout.active)
            .unwrap_or(0);
    }

    /// The output and workspace index a window lives on.
    pub fn workspace_of(&self, id: u64) -> Option<(smithay::output::Output, usize)> {
        let window = self.windows.get(&id)?;
        let output = window.output.clone()?;
        let layout = self.outputs.get(&output)?;
        let index = layout
            .workspaces
            .iter()
            .position(|workspace| return workspace.contains(id))?;
        return Some((output, index));
    }

    /// The tiled and floating window ids of workspace `index` on `output`.
    ///
    /// Used by the focus fallback after a window disappears: it must only
    /// consider windows on the workspace the closed window lived on.
    pub(crate) fn workspace_ids(
        &self,
        output: &smithay::output::Output,
        index: usize,
    ) -> Option<HashSet<u64>> {
        let layout = self.outputs.get(output)?;
        let workspace = layout.workspaces.get(index)?;
        return Some(
            workspace
                .tiled
                .iter()
                .chain(workspace.floating.iter())
                .copied()
                .collect(),
        );
    }

    /// The window that should take focus when `id` leaves workspace `index`.
    ///
    /// Focus follows the layout order, not the mapping order: a closed window
    /// hands the keyboard to the window before it in the tiled list (its left
    /// neighbour in a column layout), or to the one after it when it was the
    /// first. Only when that list has no other window does the floating list
    /// get a say. Walking the space stack instead would land on whatever
    /// window mapped last, which is rarely where the user came from.
    pub(crate) fn focus_fallback(
        &self,
        output: &smithay::output::Output,
        index: usize,
        id: u64,
    ) -> Option<u64> {
        let layout = self.outputs.get(output)?;
        let workspace = layout.workspaces.get(index)?;
        return focus_candidates(&workspace.tiled, &workspace.floating, id)
            .into_iter()
            .find(|candidate| {
                return self
                    .windows
                    .get(candidate)
                    .is_some_and(|window| return !window.minimized);
            });
    }

    /// The workspace stack of `output`, projected for drawing at `now`.
    ///
    /// The active card sits on the output rectangle and every other card is
    /// one screen away (a slide moves them all), so outside a slide
    /// `WorkspaceView::project` returns exactly the layout-local rectangle.
    pub fn view(
        &self,
        output: &smithay::output::Output,
        area: Rectangle<i32, Logical>,
        now: Instant,
    ) -> WorkspaceView {
        let Some(layout) = self.outputs.get(output) else {
            return WorkspaceView {
                origin: area.loc,
                cards: vec![WorkspaceCard { rect: area }],
            };
        };
        let cards = workspace_cards(
            area,
            layout.workspaces.len().max(1),
            layout.render_index(now),
        );
        return WorkspaceView {
            origin: area.loc,
            cards,
        };
    }

    /// Windows that can appear on `output`, back to front (the order in which
    /// they are mapped into the space), with their projected rectangle.
    ///
    /// Windows on inactive workspaces are included: they are projected one
    /// screen away, and the caller decides whether that is worth drawing (the
    /// renderer skips whatever misses the output rectangle).
    pub fn windows_on_output(
        &self,
        output: &smithay::output::Output,
        area: Rectangle<i32, Logical>,
        now: Instant,
    ) -> Vec<PlacedWindow<'_>> {
        let view = self.view(output, area, now);
        let mut result = Vec::new();
        let Some(layout) = self.outputs.get(output) else {
            return result;
        };
        fn push<'a>(
            manager: &'a LayoutManager,
            view: &WorkspaceView,
            now: Instant,
            result: &mut Vec<PlacedWindow<'a>>,
            id: u64,
            workspace: usize,
        ) {
            let Some(managed) = manager.windows.get(&id) else {
                return;
            };
            // A minimized window is still in the workspace list but not on
            // screen: the renderer must not draw it, and `sync_space` must not
            // map it back into the space.
            if managed.minimized {
                return;
            }
            let Some(rect) = view.project(workspace, managed.rect_at(now)) else {
                return;
            };
            // Every workspace has a card, even the ones currently off screen
            // (`workspace_cards` builds the whole stack).
            let Some(card) = view.cards.get(workspace) else {
                return;
            };
            result.push(PlacedWindow {
                managed,
                rect,
                card: card.rect,
            });
        }
        for (index, workspace) in layout.workspaces.iter().enumerate() {
            for id in &workspace.tiled {
                push(self, &view, now, &mut result, *id, index);
            }
        }
        for (index, workspace) in layout.workspaces.iter().enumerate() {
            for id in &workspace.floating {
                push(self, &view, now, &mut result, *id, index);
            }
        }
        return result;
    }

    /// Whether a compositor-managed fullscreen window owns the *active*
    /// workspace of `output`.
    pub fn has_fullscreen_on(&self, output: &smithay::output::Output) -> bool {
        let Some(layout) = self.outputs.get(output) else {
            return false;
        };
        return layout
            .ws()
            .tiled
            .iter()
            .chain(layout.ws().floating.iter())
            .any(|id| {
                return self.windows.get(id).is_some_and(|window| {
                    return window.fullscreen == Some(FullscreenMode::Fullscreen)
                        && !window.minimized;
                });
            });
    }

    /// The workspace stack of `output`, for IPC and status bars.
    ///
    /// The index is 0-based here; the IPC layer exposes 1-based numbers.
    pub fn workspaces_of(&self, output: &smithay::output::Output) -> Vec<WorkspaceSnapshot> {
        let Some(layout) = self.outputs.get(output) else {
            return vec![WorkspaceSnapshot {
                index: 0,
                active: true,
                layout: self.default_layout.clone(),
                focus: None,
                tiled: Vec::new(),
                floating: Vec::new(),
                minimized: Vec::new(),
            }];
        };
        return layout
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                return WorkspaceSnapshot {
                    index,
                    active: index == layout.active,
                    layout: workspace.layout.clone(),
                    focus: workspace.focus,
                    tiled: workspace.tiled.clone(),
                    floating: workspace.floating.clone(),
                    minimized: workspace
                        .tiled
                        .iter()
                        .chain(workspace.floating.iter())
                        .copied()
                        .filter(|id| {
                            return self
                                .windows
                                .get(id)
                                .is_some_and(|window| return window.minimized);
                        })
                        .collect(),
                };
            })
            .collect();
    }

    /// One managed window, for IPC and status bars.
    ///
    /// The workspace index is 0-based (see [`Self::workspaces_of`]).
    pub fn window_snapshot(&self, id: u64) -> Option<WindowSnapshot> {
        let managed = self.windows.get(&id)?;
        let identity = window_title_and_app_id(&managed.window);
        let workspace = managed.output.as_ref().and_then(|output| {
            return self.outputs.get(output).and_then(|layout| {
                return layout
                    .workspaces
                    .iter()
                    .position(|workspace| return workspace.contains(id));
            });
        });
        return Some(WindowSnapshot {
            id,
            title: identity.title,
            app_id: identity.app_id,
            output: managed.output.clone(),
            workspace,
            tiled: managed.tiled,
            minimized: managed.minimized,
            fullscreen: managed.fullscreen,
        });
    }

    /// Every managed window id, whether or not it is placed on an output.
    pub fn ids(&self) -> Vec<u64> {
        let mut ids = self.windows.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        return ids;
    }

    /// The output that currently has keyboard focus, falling back to the
    /// pointer output and then the first output.
    pub fn active_output(state: &State) -> Option<smithay::output::Output> {
        if let Some(root) = &state.core.last_window_focus {
            if let Some(managed) = state.core.layout.by_root(root) {
                if let Some(output) = &managed.output {
                    return Some(output.clone());
                }
            }
        }
        // `outputs.<name>.focus = true` decides the focused output while
        // nothing else claims focus; a pointer motion onto another output
        // clears it (`core::input`), so this never fights the mouse
        // (`docs/config.md`).
        if let Some(output) = &state.core.preferred_output {
            if state
                .core
                .space
                .outputs()
                .any(|candidate| return candidate == output)
            {
                return Some(output.clone());
            }
        }
        let location = state.core.pointer_location();
        if let Some(output) = state.core.output_at(location) {
            return Some(output);
        }
        return state.core.space.outputs().next().cloned();
    }

    fn has_layout(&self, name: &str) -> bool {
        if name == "columns" {
            return true;
        }
        return self
            .programs
            .as_ref()
            .is_some_and(|programs| return programs.has(name));
    }

    fn handles_fullscreen(&self, name: &str) -> bool {
        return self
            .programs
            .as_ref()
            .is_some_and(|programs| return programs.handles_fullscreen(name));
    }

    fn ensure_output(&mut self, output: &smithay::output::Output) {
        if self.outputs.contains_key(output) {
            return;
        }
        self.outputs.insert(
            output.clone(),
            OutputLayout::with_layout(self.default_layout.clone()),
        );
    }

    /// Give every workspace created since the last call a stable id.
    fn assign_workspace_ids(&mut self) {
        let next = &mut self.next_workspace_id;
        for layout in self.outputs.values_mut() {
            for workspace in &mut layout.workspaces {
                if workspace.id == 0 {
                    workspace.id = *next;
                    *next = next.wrapping_add(1).max(1);
                }
            }
        }
    }

    /// The layout name of the active workspace on `output`.
    pub fn layout_name(&self, output: &smithay::output::Output) -> Option<&str> {
        return self
            .outputs
            .get(output)
            .map(|layout| return layout.ws().layout.as_str());
    }

    /// The size a not-yet-mapped tiled window will get, for its initial
    /// configure (niri sends the slot size up front too).
    ///
    /// Without it the client draws its first buffer at a size of its own
    /// choosing, maps, and only then learns its slot: the window lands at the
    /// wrong size and the neighbours reflow one configure round-trip later.
    /// The window is inserted the way `map_window` will insert it, the layout
    /// is asked for its rectangle, and the insertion is undone. `None` for
    /// floating windows, which choose their own size.
    pub fn initial_size(state: &mut State, window: &Window) -> Option<Size<i32, Logical>> {
        let identity = window_title_and_app_id(window);
        let (app_id, title) = identity.as_pair();
        let rule = match_window_rule(&state.core.config, app_id, title);
        let floating = rule
            .as_ref()
            .and_then(|rule| return rule.float)
            .unwrap_or_else(|| return opens_floating(window));
        if floating {
            return None;
        }
        let focus_on_map = rule
            .as_ref()
            .and_then(|rule| return rule.focus)
            .unwrap_or(true);
        let output = open_output(state, rule)?;
        let outputs = state.core.space.outputs().cloned().collect::<Vec<_>>();
        ensure_all_outputs(&mut state.core.layout, &outputs);
        state.core.layout.ensure_output(&output);
        let area = Self::layout_area(state, &output)?;
        // The id `map_window` is about to hand out; nothing is allocated.
        let id = state.core.layout.next_id;
        if state.core.layout.windows.contains_key(&id) {
            return None;
        }
        let seed = Rectangle::new(area.loc, area.size);
        let insert_at = {
            let (min_size, max_size) = size_constraints(window);
            let entry = LayoutWindow {
                id,
                app_id: app_id.map(str::to_owned),
                title: title.map(str::to_owned),
                focused: false,
                rect: to_layout_rect(seed),
                min_size,
                max_size,
            };
            Self::insert_position(state, &output, &entry)
        };
        let managed = ManagedWindow {
            id,
            window: window.clone(),
            root: window.wl_surface()?.into_owned(),
            output: Some(output.clone()),
            tiled: true,
            minimized: false,
            fullscreen: None,
            floating_rect: seed,
            target_rect: seed,
            animation: None,
            drag_rect: None,
            drop_preview: None,
            open_animation: None,
            pending: None,
            resize: None,
            needs_frame_callback: false,
            visuals: crate::core::render::visual::WindowVisuals::resolve(
                &state.core.config,
                app_id,
                title,
            ),
        };
        state.core.layout.windows.insert(id, managed);
        let index = state.core.layout.outputs.get_mut(&output).map(|layout| {
            let workspace = layout.ws_mut();
            let index = insert_at
                .unwrap_or(workspace.tiled.len())
                .min(workspace.tiled.len());
            workspace.tiled.insert(index, id);
            return index;
        });
        let focused = if focus_on_map {
            Some(id)
        } else {
            focused_id(state)
        };
        let rect = Self::predict_rect(state, &output, focused, id);
        if let (Some(layout), Some(index)) = (state.core.layout.outputs.get_mut(&output), index) {
            let workspace = layout.ws_mut();
            if workspace.tiled.get(index) == Some(&id) {
                workspace.tiled.remove(index);
            }
        }
        state.core.layout.windows.remove(&id);
        let rect = clamp_to_hints(window, rect?);
        return Some(rect.size);
    }

    /// Register a window as it maps. Returns its stable id.
    pub fn map_window(state: &mut State, window: &Window, root: &WlSurface) -> u64 {
        let identity = window_title_and_app_id(window);
        let (app_id, title) = identity.as_pair();
        let rule = match_window_rule(&state.core.config, app_id, title);
        let output = open_output(state, rule);
        let floating = rule
            .as_ref()
            .and_then(|rule| return rule.float)
            .unwrap_or_else(|| return opens_floating(window));
        let focus_on_map = rule
            .as_ref()
            .and_then(|rule| return rule.focus)
            .unwrap_or(true);

        let outputs = state.core.space.outputs().cloned().collect::<Vec<_>>();
        let size = window.geometry().size;
        let initial = initial_rect(state, output.as_ref(), size);
        let open_animation = {
            let spec = state.core.config.general.animations.window_open();
            (!spec.duration.is_zero()).then(|| {
                return OpenAnimation {
                    started: Instant::now(),
                    spec,
                };
            })
        };
        let output_name = output.as_ref().map(|output| return output.name());
        ensure_all_outputs(&mut state.core.layout, &outputs);
        state.core.layout.assign_workspace_ids();
        let id = state.core.layout.next_id;
        state.core.layout.next_id = state.core.layout.next_id.wrapping_add(1).max(1);

        // A scripted layout may decide where a new tiled window belongs
        // (niri's `insert` hook). The hook sees the workspace without the new
        // window and returns a 1-based position; the compositor keeps the
        // order.
        let insert_at = if floating {
            None
        } else {
            output.as_ref().and_then(|output| {
                let (min_size, max_size) = size_constraints(window);
                let entry = LayoutWindow {
                    id,
                    app_id: app_id.map(str::to_owned),
                    title: title.map(str::to_owned),
                    // The window has not been focused yet at insert time.
                    focused: false,
                    rect: to_layout_rect(initial),
                    min_size,
                    max_size,
                };
                return Self::insert_position(state, output, &entry);
            })
        };

        let manager = &mut state.core.layout;
        let managed = ManagedWindow {
            id,
            window: window.clone(),
            root: root.clone(),
            output: output.clone(),
            tiled: !floating,
            minimized: false,
            fullscreen: None,
            floating_rect: initial,
            target_rect: initial,
            animation: None,
            drag_rect: None,
            drop_preview: None,
            open_animation,
            pending: None,
            resize: None,
            needs_frame_callback: false,
            visuals: crate::core::render::visual::WindowVisuals::resolve(
                &state.core.config,
                app_id,
                title,
            ),
        };
        manager.windows.insert(id, managed);
        manager.roots.insert(root.clone(), id);
        if let Some(output) = output.as_ref() {
            manager.ensure_output(output);
            if let Some(layout) = manager.outputs.get_mut(output) {
                if floating {
                    layout.ws_mut().floating.push(id);
                } else {
                    let workspace = layout.ws_mut();
                    let index = insert_at
                        .unwrap_or(workspace.tiled.len())
                        .min(workspace.tiled.len());
                    workspace.tiled.insert(index, id);
                }
            }
        }
        if !manager.hooks.contains_key(root) {
            let hook = add_pre_commit_hook::<State, _>(root, |state, _dh, surface| {
                on_pre_commit(state, surface);
            });
            manager.hooks.insert(root.clone(), hook);
        }
        manager.dirty_space = true;

        // The window is in its workspace now, so the layout can say where it
        // really goes. Adopt that rectangle as the target *before* mapping it
        // into the space: the opening animation has to fade the window in at
        // its final slot, not at the centred fallback that the reflow would
        // reach one configure round-trip later.
        let rect = if floating {
            clamp_to_hints(window, initial)
        } else {
            let focused = if focus_on_map {
                Some(id)
            } else {
                focused_id(state)
            };
            output
                .as_ref()
                .and_then(|output| return Self::predict_rect(state, output, focused, id))
                .map(|rect| return clamp_to_hints(window, rect))
                .unwrap_or_else(|| return clamp_to_hints(window, initial))
        };
        if let Some(managed) = state.core.layout.windows.get_mut(&id) {
            managed.target_rect = rect;
            // Floating windows keep `floating_rect` as their own geometry, and
            // a tiled window returns there when it starts floating, so both
            // open at the same rectangle.
            managed.floating_rect = rect;
        }
        state
            .core
            .space
            .map_element(window.clone(), rect.loc, false);
        debug!(
            "mapped window {id} ({:?}) on {:?} at {rect:?} floating={floating} focus={focus_on_map}",
            app_id.unwrap_or(""),
            output_name,
        );
        state.core.queue_redraw_all();
        return id;
    }

    /// Whether a newly mapped window should take focus (its rule may say no).
    pub fn should_focus_on_map(state: &State, window: &Window) -> bool {
        let identity = window_title_and_app_id(window);
        let (app_id, title) = identity.as_pair();
        return match_window_rule(&state.core.config, app_id, title)
            .and_then(|rule| return rule.focus)
            .unwrap_or(true);
    }

    /// Remember which root a surface belongs to while it is still alive.
    pub(crate) fn remember_surface_root(&mut self, surface: &WlSurface, root: &WlSurface) {
        self.surface_roots.insert(surface.clone(), root.clone());
    }

    /// Root recorded for `surface`, if any.
    pub(crate) fn root_for_surface(&self, surface: &WlSurface) -> Option<WlSurface> {
        return self.surface_roots.get(surface).cloned();
    }

    /// Drop a surface from the root table after its destruction hook ran.
    pub(crate) fn forget_surface(&mut self, surface: &WlSurface) {
        self.surface_roots.remove(surface);
        self.surface_roots
            .retain(|key, root| key != surface && root != surface);
    }

    /// Take the close snapshot baked by the null-buffer pre-commit hook or a
    /// surface destruction hook.
    pub(crate) fn take_pending_unmap_closing(&mut self, root: &WlSurface) -> Option<ClosingWindow> {
        return self.pending_unmap_closing.remove(root);
    }

    pub(crate) fn has_pending_unmap_closing(&self, root: &WlSurface) -> bool {
        return self.pending_unmap_closing.contains_key(root);
    }

    pub(crate) fn stash_pending_unmap_closing(&mut self, root: &WlSurface, closing: ClosingWindow) {
        self.pending_unmap_closing.insert(root.clone(), closing);
    }

    pub(crate) fn clear_pending_unmap_closing(&mut self, root: &WlSurface) {
        self.pending_unmap_closing.remove(root);
    }

    /// Forget a window that unmapped or was destroyed.
    pub fn remove_window(state: &mut State, root: &WlSurface) {
        let manager = &mut state.core.layout;
        manager.pending_unmap_closing.remove(root);
        manager.forget_surface(root);
        let Some(id) = manager.roots.remove(root) else {
            return;
        };
        if let Some(hook) = manager.hooks.remove(root) {
            remove_pre_commit_hook(root, &hook);
        }
        let Some(managed) = manager.windows.remove(&id) else {
            return;
        };
        if let Some(output) = &managed.output {
            if let Some(layout) = manager.outputs.get_mut(output) {
                for workspace in &mut layout.workspaces {
                    workspace.tiled.retain(|entry| return *entry != id);
                    workspace.floating.retain(|entry| return *entry != id);
                    if workspace.focus == Some(id) {
                        workspace.focus = None;
                    }
                }
            }
        }
        manager.dirty_space = true;
        state.core.queue_redraw_all();
    }

    fn output_rect(
        state: &State,
        output: &smithay::output::Output,
    ) -> Option<Rectangle<i32, Logical>> {
        return state.core.space.output_geometry(output);
    }

    /// The output area not reserved by layer-shell exclusive zones (bars).
    pub fn work_area(
        state: &State,
        output: &smithay::output::Output,
    ) -> Option<Rectangle<i32, Logical>> {
        return work_area(state, output)
            .or_else(|| return state.core.space.output_geometry(output));
    }

    /// The area layouts may fill: the work area minus `general.gap` on every
    /// side.
    ///
    /// `gap` is compositor-level spacing, so it applies to width *and* height:
    /// windows keep a gap from the screen edges (or from a bar's exclusive
    /// zone) exactly like they keep one from each other. Layouts receive this
    /// already-inset rectangle as `ctx.output.work_area`; window-to-window
    /// spacing stays the layout's job (`ctx.gap`).
    pub fn layout_area(
        state: &State,
        output: &smithay::output::Output,
    ) -> Option<Rectangle<i32, Logical>> {
        let area = Self::work_area(state, output)?;
        return Some(inset_area(area, state.core.config.general.gap));
    }

    /// React to an exclusive-zone change (a bar appeared, resized or went
    /// away).
    ///
    /// Layouts already receive the work area, so the correct response is a
    /// relayout rather than nudging windows: centring them would fight the
    /// layout and leave tiled windows in the wrong slots. Floating windows are
    /// not placed by any layout, so they are clamped back into the new area.
    pub fn on_work_area_changed(state: &mut State, output: &smithay::output::Output) {
        let Some(area) = Self::layout_area(state, output) else {
            return;
        };
        let ids = state
            .core
            .layout
            .outputs
            .get(output)
            .map(|layout| {
                return layout
                    .workspaces
                    .iter()
                    .flat_map(|workspace| return workspace.floating.clone())
                    .collect::<Vec<_>>();
            })
            .unwrap_or_default();
        for id in ids {
            let Some(managed) = state.core.layout.windows.get_mut(&id) else {
                continue;
            };
            if managed.fullscreen.is_some() || managed.drag_rect.is_some() {
                continue;
            }
            let rect = managed.floating_rect;
            let max_x = (area.loc.x + area.size.w - rect.size.w).max(area.loc.x);
            let max_y = (area.loc.y + area.size.h - rect.size.h).max(area.loc.y);
            let x = rect.loc.x.clamp(area.loc.x, max_x);
            let y = rect.loc.y.clamp(area.loc.y, max_y);
            if x == rect.loc.x && y == rect.loc.y {
                continue;
            }
            let clamped = Rectangle::new(Point::from((x, y)), rect.size);
            managed.floating_rect = clamped;
            managed.target_rect = clamped;
            managed.animation = None;
        }
        Self::relayout(state);
        state.core.layout.dirty_space = true;
        state.core.queue_redraw(output);
    }

    /// Recompute every output's geometry and apply it.
    pub fn relayout(state: &mut State) {
        let now = Instant::now();
        let outputs = state.core.space.outputs().cloned().collect::<Vec<_>>();
        if outputs.is_empty() {
            return;
        }
        let outputs_all = state.core.space.outputs().cloned().collect::<Vec<_>>();
        ensure_all_outputs(&mut state.core.layout, &outputs_all);
        state.core.layout.assign_workspace_ids();

        let mut desired: HashMap<u64, Rectangle<i32, Logical>> = HashMap::new();
        let focused = focused_id(state);
        for output in &outputs {
            let Some(area) = Self::layout_area(state, output) else {
                continue;
            };
            let Some(output_rect) = Self::output_rect(state, output) else {
                continue;
            };
            let count = state
                .core
                .layout
                .outputs
                .get(output)
                .map(|layout| return layout.workspaces.len())
                .unwrap_or(0);
            for index in 0..count {
                desired.extend(Self::workspace_desired(
                    state,
                    output,
                    index,
                    area,
                    output_rect,
                    now,
                    focused,
                ));
            }
        }

        supersede_pending(state);
        apply_desired(state, desired, now);
        sync_space(state);
    }

    /// The rectangles the active layout gives every window of one workspace.
    ///
    /// `relayout` arranges every workspace this way; `map_window` uses the
    /// same function for the workspace a new window just joined, so the
    /// rectangle it opens at is the one the next `relayout` will keep.
    fn workspace_desired(
        state: &mut State,
        output: &smithay::output::Output,
        index: usize,
        area: Rectangle<i32, Logical>,
        output_rect: Rectangle<i32, Logical>,
        now: Instant,
        focused: Option<u64>,
    ) -> HashMap<u64, Rectangle<i32, Logical>> {
        let mut desired = HashMap::new();
        let Some((context, tiled, floating, layout_name)) =
            Self::workspace_context(state, output, index, area, now, focused)
        else {
            return desired;
        };

        let mut script_result = None;
        let mut script_error = None;
        // The built-in `columns` stack is not a `LayoutProgram`: only
        // ask the script bridge when the workspace really names one.
        if let Some(programs) = &state.core.layout.programs {
            if programs.has(&layout_name) {
                match programs.arrange(&layout_name, &context) {
                    Ok(result) => script_result = Some(result),
                    Err(err) => script_error = Some(err.to_string()),
                }
            }
        }
        if let Some(message) = script_error {
            let key = format!("error:{}:{index}:{layout_name}", output.name());
            if state.core.layout.warned.insert(key) {
                warn!(
                    "layout {layout_name:?} on {} workspace {index} failed: {message}; using the column fallback",
                    output.name()
                );
            }
        }

        let handles_fullscreen =
            state.core.layout.handles_fullscreen(&layout_name) && script_result.is_some();
        if let Some(result) = &script_result {
            let expected = context.windows.iter().map(|window| return window.id);
            for id in expected {
                let key = format!("missing:{}:{index}:{layout_name}:{id}", output.name());
                if !result.contains_key(&id) && state.core.layout.warned.insert(key) {
                    warn!(
                        "layout {layout_name:?} on {} workspace {index} did not return window {id}; keeping its geometry",
                        output.name()
                    );
                }
            }
        }

        let fallback = column_layout(
            &tiled,
            &context.fullscreen,
            area,
            state.core.config.general.gap,
        );
        for id in tiled.iter().copied().chain(floating.iter().copied()) {
            let Some(window) = state.core.layout.windows.get(&id) else {
                continue;
            };
            let rect = if window.fullscreen == Some(FullscreenMode::Fullscreen) {
                match script_result
                    .as_ref()
                    .and_then(|result| return result.get(&id))
                {
                    Some(rect) if handles_fullscreen => from_layout_rect(*rect),
                    _ => output_rect,
                }
            } else if let Some(rect) = script_result
                .as_ref()
                .and_then(|result| return result.get(&id))
            {
                from_layout_rect(*rect)
            } else if window.tiled {
                fallback
                    .get(&id)
                    .copied()
                    .unwrap_or_else(|| return window.target_rect)
            } else {
                window.floating_rect
            };
            desired.insert(id, rect);
        }
        return desired;
    }

    /// Where the layout will put `id` in the active workspace of `output`.
    ///
    /// `map_window` needs the final rectangle before the window is mapped into
    /// the space. Mapping it at the centered `initial_rect` first and letting
    /// the following reflow move it makes the opening animation fade the
    /// window in at the middle of the screen and then jump to its slot.
    fn predict_rect(
        state: &mut State,
        output: &smithay::output::Output,
        focused: Option<u64>,
        id: u64,
    ) -> Option<Rectangle<i32, Logical>> {
        let area = Self::layout_area(state, output)?;
        let output_rect = Self::output_rect(state, output)?;
        let index = state.core.layout.active_workspace(output);
        let now = Instant::now();
        let desired =
            Self::workspace_desired(state, output, index, area, output_rect, now, focused);
        return desired.get(&id).copied();
    }

    /// Ask the active layout of a workspace where a new window belongs.
    ///
    /// Returns a 0-based tiled insertion index; `None` means append, which is
    /// also what the built-in layout and layouts without an `insert` hook do.
    fn insert_position(
        state: &State,
        output: &smithay::output::Output,
        entry: &LayoutWindow,
    ) -> Option<usize> {
        let area = Self::layout_area(state, output)?;
        let index = state.core.layout.active_workspace(output);
        let now = Instant::now();
        let focused = focused_id(state);
        let (context, _, _, layout_name) =
            Self::workspace_context(state, output, index, area, now, focused)?;
        let programs = state.core.layout.programs.as_ref()?;
        // The built-in column stack is not a `LayoutProgram`, so it has no
        // `insert` hook and must not reach the error path below.
        if !programs.has(&layout_name) {
            return None;
        }
        return match programs.insert(&layout_name, &context, entry) {
            Ok(index) => index,
            Err(err) => {
                warn!("layout {layout_name:?} insert() failed: {err}");
                None
            }
        };
    }

    /// Build the script context for one workspace.
    ///
    /// Returns the context plus the workspace's tiled and floating lists and
    /// its layout name, which the arrange loop needs after the call.
    fn workspace_context(
        state: &State,
        output: &smithay::output::Output,
        index: usize,
        area: Rectangle<i32, Logical>,
        now: Instant,
        focused: Option<u64>,
    ) -> Option<(LayoutContext, Vec<u64>, Vec<u64>, String)> {
        let layout = state.core.layout.outputs.get(output)?;
        let workspace = layout.workspaces.get(index)?;
        // Minimized windows stay in the lists (the taskbar restores them by id)
        // but take no part in the layout: they get no rectangle, do not count
        // for the scripted `ctx.windows`, and the remaining windows reflow over
        // their slot.
        let visible = |id: &u64| {
            return state
                .core
                .layout
                .windows
                .get(id)
                .is_some_and(|window| return !window.minimized);
        };
        let tiled = workspace
            .tiled
            .iter()
            .copied()
            .filter(visible)
            .collect::<Vec<_>>();
        let floating = workspace
            .floating
            .iter()
            .copied()
            .filter(visible)
            .collect::<Vec<_>>();
        let layout_name = workspace.layout.clone();
        let workspace_id = workspace.id;

        let fullscreen = tiled
            .iter()
            .chain(floating.iter())
            .filter_map(|id| {
                let window = state.core.layout.windows.get(id)?;
                return (window.fullscreen == Some(FullscreenMode::Fullscreen)).then_some(*id);
            })
            .collect::<Vec<_>>();

        let mut context_windows = Vec::new();
        for id in tiled.iter().copied() {
            let Some(window) = state.core.layout.windows.get(&id) else {
                continue;
            };
            if window.fullscreen == Some(FullscreenMode::Fullscreen) {
                continue;
            }
            let identity = window_title_and_app_id(&window.window);
            let (app_id, title) = identity.as_pair();
            let (min_size, max_size) = size_constraints(&window.window);
            context_windows.push(LayoutWindow {
                id,
                app_id: app_id.map(str::to_owned),
                title: title.map(str::to_owned),
                focused: focused == Some(id),
                rect: to_layout_rect(window.rect_at(now)),
                min_size,
                max_size,
            });
        }

        let context = LayoutContext {
            output: LayoutOutput {
                name: output.name(),
                width: area.size.w,
                height: area.size.h,
                scale: output.current_scale().fractional_scale(),
                work_area: to_layout_rect(area),
            },
            workspace: LayoutWorkspace {
                id: workspace_id,
                index: index + 1,
            },
            windows: context_windows,
            floating: floating
                .iter()
                .copied()
                .filter(|id| return !fullscreen.contains(id))
                .collect(),
            fullscreen,
            focused,
        };
        return Some((context, tiled, floating, layout_name));
    }

    /// Toggle the focused window between tiled and floating.
    pub fn toggle_floating(state: &mut State) {
        let Some(id) = focused_id(state) else {
            return;
        };
        let Some(output) = state
            .core
            .layout
            .windows
            .get(&id)
            .and_then(|w| return w.output.clone())
        else {
            return;
        };
        let (was_fullscreen, was_tiled, current_rect) = {
            let Some(window) = state.core.layout.windows.get(&id) else {
                return;
            };
            (
                window.fullscreen.is_some(),
                window.tiled,
                window.rect_at(Instant::now()),
            )
        };
        if was_fullscreen {
            set_fullscreen_mode(state, id, None);
        }
        if was_tiled {
            // Pull the window out at its current on-screen geometry.
            let rect = current_rect;
            if let Some(window) = state.core.layout.windows.get_mut(&id) {
                window.tiled = false;
                window.floating_rect = rect;
                window.target_rect = rect;
                window.animation = None;
            }
            if let Some(layout) = state.core.layout.outputs.get_mut(&output) {
                layout.ws_mut().tiled.retain(|entry| return *entry != id);
                if !layout.ws().floating.contains(&id) {
                    layout.ws_mut().floating.push(id);
                }
            }
        } else {
            if let Some(window) = state.core.layout.windows.get_mut(&id) {
                window.tiled = true;
                window.fullscreen = None;
                window.animation = None;
                window.drag_rect = None;
                window.drop_preview = None;
            }
            let center = Point::from((
                current_rect.loc.x as f64 + current_rect.size.w as f64 / 2.0,
                current_rect.loc.y as f64 + current_rect.size.h as f64 / 2.0,
            ));
            let insert_at = insertion_index(state, &output, id, center);
            if let Some(layout) = state.core.layout.outputs.get_mut(&output) {
                layout.ws_mut().floating.retain(|entry| return *entry != id);
                let len = layout.ws().tiled.len();
                layout.ws_mut().tiled.insert(insert_at.min(len), id);
            }
        }
        state.core.layout.dirty_space = true;
        Self::relayout(state);
        state.core.queue_redraw_all();
    }

    /// `toggle-fullscreen`: compositor-managed fullscreen.
    pub fn toggle_fullscreen(state: &mut State) {
        let Some(id) = focused_id(state) else {
            return;
        };
        let current = state
            .core
            .layout
            .windows
            .get(&id)
            .and_then(|window| return window.fullscreen);
        let next = match current {
            Some(FullscreenMode::Fullscreen) => None,
            _ => Some(FullscreenMode::Fullscreen),
        };
        set_fullscreen_mode(state, id, next);
        Self::relayout(state);
        state.core.queue_redraw_all();
    }

    /// `toggle-windowed-fullscreen`: fullscreen xdg state, tiled geometry.
    pub fn toggle_windowed_fullscreen(state: &mut State) {
        let Some(id) = focused_id(state) else {
            return;
        };
        let current = state
            .core
            .layout
            .windows
            .get(&id)
            .and_then(|window| return window.fullscreen);
        let next = match current {
            Some(FullscreenMode::Windowed) => None,
            _ => Some(FullscreenMode::Windowed),
        };
        set_fullscreen_mode(state, id, next);
        Self::relayout(state);
        state.core.queue_redraw_all();
    }

    /// Set the fullscreen mode of the window whose root surface is `root`,
    /// as requested by the client (`xdg_toplevel.set_fullscreen`).
    pub fn set_fullscreen_for_root(
        state: &mut State,
        root: &WlSurface,
        mode: Option<FullscreenMode>,
    ) {
        let Some(id) = state.core.layout.roots.get(root).copied() else {
            return;
        };
        set_fullscreen_mode(state, id, mode);
        Self::relayout(state);
        state.core.queue_redraw_all();
    }

    /// Set the layout of the active output.
    pub fn set_layout(state: &mut State, name: &str) {
        let Some(output) = Self::active_output(state) else {
            return;
        };
        if let Err(err) = Self::set_layout_on(state, &output, None, name) {
            warn!("{err}");
        }
    }

    /// Switch one workspace of `output` to a named layout.
    ///
    /// `workspace` defaults to the output's active workspace. Returns the
    /// reason when the switch did not happen, so IPC can reply with it.
    pub fn set_layout_on(
        state: &mut State,
        output: &smithay::output::Output,
        workspace: Option<usize>,
        name: &str,
    ) -> Result<(), String> {
        if !state.core.layout.has_layout(name) {
            return Err(format!("no layout named {name:?} is configured"));
        }
        state.core.layout.ensure_output(output);
        let default_layout = state.core.layout.default_layout.clone();
        let Some(layout) = state.core.layout.outputs.get_mut(output) else {
            return Err(format!("output {} has no workspace stack", output.name()));
        };
        let index = workspace.unwrap_or(layout.active);
        layout.ensure_workspace(index, &default_layout);
        let index = index.min(layout.workspaces.len().saturating_sub(1));
        if let Some(workspace) = layout.workspaces.get_mut(index) {
            workspace.layout = name.to_owned();
        }
        debug!(
            "output {} workspace {index} now uses layout {name}",
            output.name()
        );
        Self::relayout(state);
        state.core.queue_redraw_all();
        return Ok(());
    }

    /// Switch the active output to the next configured layout.
    pub fn cycle_layout(state: &mut State) {
        let Some(output) = Self::active_output(state) else {
            return;
        };
        let mut names = state.core.layout.layout_names.clone();
        if !names.iter().any(|name| return name == "columns") {
            names.push("columns".to_owned());
        }
        if names.is_empty() {
            return;
        }
        state.core.layout.ensure_output(&output);
        let current = state
            .core
            .layout
            .outputs
            .get(&output)
            .map(|layout| return layout.ws().layout.as_str())
            .unwrap_or("columns");
        let index = names
            .iter()
            .position(|name| return name == current)
            .map_or(0, |index| return (index + 1) % names.len());
        let name = names[index].clone();
        if let Some(layout) = state.core.layout.outputs.get_mut(&output) {
            layout.ws_mut().layout = name;
        }
        Self::relayout(state);
        state.core.queue_redraw_all();
    }

    /// Invoke an action declared by the active workspace's layout.
    ///
    /// Layout keybindings and IPC both land here. The handler runs against the
    /// same per-layout state as `arrange`, and its effects are applied by the
    /// compositor rather than by the script directly.
    pub fn run_layout_action(state: &mut State, name: &str, args: &[String]) {
        let Some(output) = Self::active_output(state) else {
            warn!("layout action {name:?}: there is no output");
            return;
        };
        state.core.layout.ensure_output(&output);
        state.core.layout.assign_workspace_ids();
        let index = state.core.layout.active_workspace(&output);
        let Some(area) = Self::layout_area(state, &output) else {
            return;
        };
        let now = Instant::now();
        let focused = focused_id(state);
        let Some((context, _, _, layout_name)) =
            Self::workspace_context(state, &output, index, area, now, focused)
        else {
            return;
        };
        let Some(programs) = state.core.layout.programs.as_ref() else {
            warn!("layout action {name:?}: no scripted layouts are configured");
            return;
        };
        if !programs.has(&layout_name) {
            warn!(
                "layout action {name:?}: the active workspace uses the built-in {layout_name:?} layout, which has no actions"
            );
            return;
        }
        let result = programs.invoke_action(&layout_name, name, &context, args);
        let mut effects = match result {
            Ok(effects) => effects,
            Err(err) => {
                warn!("layout action {name:?} on {layout_name:?} failed: {err}");
                return;
            }
        };
        if state
            .core
            .layout
            .programs
            .as_ref()
            .is_some_and(|programs| return programs.take_relayout())
        {
            effects.relayout = true;
        }
        Self::apply_layout_effects(state, &output, index, effects);
    }

    /// Apply the effects a layout action returned.
    fn apply_layout_effects(
        state: &mut State,
        output: &smithay::output::Output,
        index: usize,
        effects: LayoutEffects,
    ) {
        let mut changed = false;

        if let Some(order) = effects.order {
            // `ctx.windows` only lists the windows the layout places, so a
            // scripted layout orders the visible windows. Minimized ones keep
            // their position in the list (the taskbar restores them by id, and
            // their slot is where they come back), so the returned order is
            // validated against the visible subset and merged back in.
            let minimized = state
                .core
                .layout
                .windows
                .iter()
                .filter(|(_, window)| return window.minimized)
                .map(|(id, _)| return *id)
                .collect::<HashSet<_>>();
            let result = (|| -> Result<(), String> {
                let Some(layout) = state.core.layout.outputs.get_mut(output) else {
                    return Err(format!("output {} has no workspace stack", output.name()));
                };
                let Some(workspace) = layout.workspaces.get_mut(index) else {
                    return Err(format!("output {} has no workspace {index}", output.name()));
                };
                let Some(merged) = merge_visible_order(&workspace.tiled, order, &minimized) else {
                    return Err(format!(
                        "layout action returned an order that is not a permutation of workspace {index} on {}",
                        output.name()
                    ));
                };
                workspace.tiled = merged;
                return Ok(());
            })();
            match result {
                Ok(()) => changed = true,
                Err(err) => warn!("{err}"),
            }
        }

        let mut relaid_out = false;
        if let Some(id) = effects.focus {
            let belongs =
                state
                    .core
                    .layout
                    .workspace_of(id)
                    .is_some_and(|(window_output, workspace)| {
                        return window_output == *output && workspace == index;
                    });
            if !belongs {
                warn!(
                    "layout action focused window {id}, which is not on workspace {index} of {}",
                    output.name()
                );
            } else if Self::focus_window_id(state, id) {
                changed = true;
                relaid_out = true;
            }
        }
        if let Some(workspace) = effects.switch_workspace {
            let current = state.core.layout.active_workspace(output) + 1;
            if workspace != current {
                Self::focus_workspace(state, workspace);
                changed = true;
                relaid_out = true;
            }
        }

        if !relaid_out && (effects.relayout || changed) {
            Self::relayout(state);
            state.core.queue_redraw_all();
        }
    }

    /// Focus the next or previous window in the output's stacking order.
    pub fn focus_relative(state: &mut State, delta: isize) {
        let Some(output) = Self::active_output(state) else {
            return;
        };
        let (mut ids, floating) = state
            .core
            .layout
            .outputs
            .get(&output)
            .map(|layout| {
                return (layout.ws().tiled.clone(), layout.ws().floating.clone());
            })
            .unwrap_or_default();
        // Floating windows stack above tiled ones, so they come later in the
        // focus cycle. A compositor-managed fullscreen window owns the output
        // on its own until it leaves fullscreen.
        ids.extend(floating);
        // Minimized windows are not on screen, so the cycle skips them.
        ids.retain(|id| {
            return state
                .core
                .layout
                .windows
                .get(id)
                .is_some_and(|window| return !window.minimized);
        });
        let fullscreen_ids = ids
            .iter()
            .copied()
            .filter(|id| {
                return state.core.layout.windows.get(id).is_some_and(|window| {
                    return window.fullscreen == Some(FullscreenMode::Fullscreen);
                });
            })
            .collect::<Vec<_>>();
        let ids = if fullscreen_ids.is_empty() {
            ids
        } else {
            fullscreen_ids
        };
        if ids.is_empty() {
            return;
        }
        let focused = focused_id(state);
        let index = focused
            .and_then(|id| return ids.iter().position(|entry| return *entry == id))
            .map_or(0, |index| {
                return (index as isize + delta).rem_euclid(ids.len() as isize) as usize;
            });
        let Some(id) = ids.get(index) else {
            return;
        };
        let (window, surface) = {
            let Some(window) = state.core.layout.windows.get(id) else {
                return;
            };
            (window.window.clone(), Some(window.root.clone()))
        };
        focus_window(state, &window, surface, SERIAL_COUNTER.next_serial());
    }

    /// Focus the window with this id (`focus-window`, used by scripts over IPC).
    ///
    /// The window may live on another workspace or on another output: its
    /// workspace is brought on screen first. Taking the keyboard makes that
    /// output the active one, because [`Self::active_output`] follows the
    /// focused window. A minimized window is restored first. Returns whether
    /// there was such a window.
    pub fn focus_window_id(state: &mut State, id: u64) -> bool {
        let Some((output, index)) = state.core.layout.workspace_of(id) else {
            return false;
        };
        // Restoring is what focusing a minimized window means: it has to rejoin
        // the space before it can hold the keyboard. `focus_window` relayouts,
        // which maps it again.
        Self::unminimize(state, id);
        let Some(managed) = state.core.layout.windows.get(&id) else {
            return false;
        };
        let (window, root) = (managed.window.clone(), managed.root.clone());
        if state.core.layout.active_workspace(&output) != index {
            state.core.layout.ensure_output(&output);
            Self::switch_workspace(state, &output, index);
        }
        focus_window(state, &window, Some(root), SERIAL_COUNTER.next_serial());
        state.core.queue_redraw_all();
        return true;
    }

    /// Minimize the focused window (`minimize-window` without an id).
    ///
    /// Focus moves to the next window of the workspace, so the keyboard does
    /// not stay on a window that is no longer on screen.
    pub fn minimize_focused(state: &mut State) {
        let Some(id) = focused_id(state) else {
            debug!("minimize-window: no window has the focus");
            return;
        };
        Self::minimize_window(state, id);
    }

    /// Minimize a window by id.
    ///
    /// The window keeps its workspace and its place in the tiled/floating list
    /// — the taskbar still lists it and `poly msg windows` still reports it —
    /// but it leaves the space, so it is no longer drawn, hit-tested or sent
    /// frame callbacks, and the layout reflows over its slot. Restoring it (or
    /// focusing it) puts it back. Returns whether it was minimized by this call.
    pub fn minimize_window(state: &mut State, id: u64) -> bool {
        let Some((output, _)) = state.core.layout.workspace_of(id) else {
            debug!("minimize-window: window {id} is not managed");
            return false;
        };
        let had_focus = focused_id(state) == Some(id);
        let window = {
            let Some(managed) = state.core.layout.windows.get_mut(&id) else {
                return false;
            };
            if managed.minimized {
                return false;
            }
            managed.minimized = true;
            managed.animation = None;
            managed.drag_rect = None;
            managed.drop_preview = None;
            managed.window.clone()
        };
        // The window leaves the space, so it cannot keep the `Activated` state
        // (Smithay only clears it for elements the space still holds) and it
        // must not receive frame callbacks while hidden.
        state.core.space.unmap_elem(&window);
        window.set_activated(false);
        if had_focus {
            set_window_focus(state, None);
        }
        state.core.layout.dirty_space = true;
        Self::relayout(state);
        if had_focus {
            Self::restore_workspace_focus(state, &output);
        }
        crate::core::focus::update_keyboard_focus(state);
        state.core.queue_redraw_all();
        return true;
    }

    /// Restore the most recently minimized window of the active workspace
    /// (`restore-window` without an id) and focus it.
    pub fn restore_last_minimized(state: &mut State) {
        let Some(id) = Self::last_minimized(state) else {
            debug!("restore-window: the active workspace has no minimized window");
            return;
        };
        Self::restore_window(state, id);
    }

    /// Restore a minimized window and focus it (`restore-window` with an id).
    ///
    /// Returns whether the window was minimized before the call. Focusing is
    /// deliberate: restoring is only useful if the window comes back to the
    /// user, and the taskbar entry it was clicked from is gone afterwards.
    pub fn restore_window(state: &mut State, id: u64) -> bool {
        if !Self::unminimize(state, id) {
            return false;
        }
        Self::focus_window_id(state, id);
        return true;
    }

    /// Put a minimized window back into the space without changing focus.
    ///
    /// `relayout` maps it again at its layout slot; the caller decides where
    /// the keyboard goes. Returns whether the window was minimized.
    fn unminimize(state: &mut State, id: u64) -> bool {
        let Some(managed) = state.core.layout.windows.get_mut(&id) else {
            return false;
        };
        if !managed.minimized {
            return false;
        }
        managed.minimized = false;
        managed.animation = None;
        state.core.layout.dirty_space = true;
        return true;
    }

    /// The id of the most recently minimized window on the active workspace.
    ///
    /// The workspace lists keep minimized windows in layout order, so the last
    /// one in them is the newest: `toggle_floating` and the layout actions
    /// append, and minimizing does not reorder.
    fn last_minimized(state: &State) -> Option<u64> {
        let output = Self::active_output(state)?;
        let layout = state.core.layout.outputs.get(&output)?;
        let workspace = layout.ws();
        return workspace
            .floating
            .iter()
            .rev()
            .chain(workspace.tiled.iter().rev())
            .copied()
            .find(|id| {
                return state
                    .core
                    .layout
                    .windows
                    .get(id)
                    .is_some_and(|window| return window.minimized);
            });
    }

    /// Move the focused tiled window one slot left or right.
    pub fn move_window_relative(state: &mut State, delta: isize) {
        let Some(id) = focused_id(state) else {
            return;
        };
        let Some(output) = state
            .core
            .layout
            .windows
            .get(&id)
            .and_then(|w| return w.output.clone())
        else {
            return;
        };
        let Some(layout) = state.core.layout.outputs.get(&output) else {
            return;
        };
        // Only visible windows take part in the move: swapping with a minimized
        // neighbour would reorder something the user cannot see.
        let visible = layout
            .ws()
            .tiled
            .iter()
            .copied()
            .filter(|entry| {
                return state
                    .core
                    .layout
                    .windows
                    .get(entry)
                    .is_some_and(|window| return !window.minimized);
            })
            .collect::<Vec<_>>();
        let Some(index) = visible.iter().position(|entry| return *entry == id) else {
            return;
        };
        let len = visible.len();
        let next = (index as isize + delta).rem_euclid(len as isize) as usize;
        let target = visible[next];
        if target == id {
            return;
        }
        let Some(layout) = state.core.layout.outputs.get_mut(&output) else {
            return;
        };
        let Some(from) = layout
            .ws()
            .tiled
            .iter()
            .position(|entry| return *entry == id)
        else {
            return;
        };
        let Some(to) = layout
            .ws()
            .tiled
            .iter()
            .position(|entry| return *entry == target)
        else {
            return;
        };
        layout.ws_mut().tiled.swap(from, to);
        Self::relayout(state);
        state.core.queue_redraw_all();
    }

    /// Focus the workspace with this 1-based number (`focus-workspace`).
    ///
    /// Workspaces are created on demand: asking for the third one on a fresh
    /// output creates the two empty ones above it, so `Mod+3` always leads
    /// somewhere.
    pub fn focus_workspace(state: &mut State, index: usize) {
        let Some(output) = Self::active_output(state) else {
            return;
        };
        if index == 0 {
            warn!("workspace numbers start at 1");
            return;
        }
        state.core.layout.ensure_output(&output);
        let default_layout = state.core.layout.default_layout.clone();
        if let Some(layout) = state.core.layout.outputs.get_mut(&output) {
            layout.ensure_workspace(index - 1, &default_layout);
        }
        Self::switch_workspace(state, &output, index - 1);
    }

    /// Move to the workspace above (`delta = -1`) or below (`delta = 1`).
    ///
    /// At the bottom edge a new empty workspace is appended (niri does the
    /// same, which is how scrolling creates workspaces); at the top edge
    /// nothing happens.
    pub fn focus_workspace_relative(state: &mut State, delta: isize) {
        let Some(output) = Self::active_output(state) else {
            return;
        };
        state.core.layout.ensure_output(&output);
        let default_layout = state.core.layout.default_layout.clone();
        let Some(layout) = state.core.layout.outputs.get_mut(&output) else {
            return;
        };
        let current = layout.active;
        let target = current as isize + delta;
        if target < 0 {
            return;
        }
        let target = target as usize;
        layout.ensure_workspace(target, &default_layout);
        Self::switch_workspace(state, &output, target);
    }

    /// Move the focused window into another workspace and follow it.
    pub fn move_window_to_workspace(state: &mut State, index: usize) {
        if index == 0 {
            warn!("workspace numbers start at 1");
            return;
        }
        Self::move_focused_to_workspace(state, index - 1);
    }

    /// Move the focused window one workspace up (`-1`) or down (`1`).
    pub fn move_window_to_workspace_relative(state: &mut State, delta: isize) {
        let Some(id) = focused_id(state) else {
            return;
        };
        let Some((_, workspace)) = state.core.layout.workspace_of(id) else {
            return;
        };
        let target = workspace as isize + delta;
        if target < 0 {
            return;
        }
        Self::move_focused_to_workspace(state, target as usize);
    }

    /// Swap the active workspace with its neighbour (`move-workspace-*`).
    ///
    /// This reorders the stack instead of sliding: the window stays on screen
    /// and only its position in the stack changes.
    pub fn move_workspace(state: &mut State, delta: isize) {
        let Some(output) = Self::active_output(state) else {
            return;
        };
        let Some(layout) = state.core.layout.outputs.get_mut(&output) else {
            return;
        };
        let current = layout.active;
        let target = current as isize + delta;
        if target < 0 || target as usize >= layout.workspaces.len() {
            return;
        }
        layout.workspaces.swap(current, target as usize);
        layout.active = target as usize;
        layout.switch = None;
        state.core.layout.dirty_space = true;
        Self::relayout(state);
        state.core.queue_redraw_all();
    }

    /// Enter workspace `index` on `output`, saving and restoring its focus.
    fn switch_workspace(state: &mut State, output: &smithay::output::Output, index: usize) {
        // Re-selecting the workspace that is already active is a no-op: it must
        // not teleport focus to whatever that workspace last remembered.
        if state
            .core
            .layout
            .outputs
            .get(output)
            .is_some_and(|layout| return layout.active == index)
        {
            return;
        }
        Self::set_active_workspace(state, output, index);
        if !Self::restore_workspace_focus(state, output) {
            Self::relayout(state);
        }
        state.core.queue_redraw_all();
        ensure_animation_timer(state);
    }

    /// Change the active workspace and start the slide animation.
    fn set_active_workspace(state: &mut State, output: &smithay::output::Output, index: usize) {
        let now = Instant::now();
        let spec = workspace_animation(state);
        // Remember the focus of the workspace we are leaving.
        let focused = focused_id(state);
        let default_layout = state.core.layout.default_layout.clone();
        let Some(layout) = state.core.layout.outputs.get_mut(output) else {
            return;
        };
        layout.ensure_workspace(index, &default_layout);
        let target = index.min(layout.workspaces.len().saturating_sub(1));
        if let Some(id) = focused {
            let current = layout.active;
            if let Some(workspace) = layout.workspaces.get_mut(current) {
                if workspace.contains(id) {
                    workspace.focus = Some(id);
                }
            }
        }
        if layout.active == target {
            return;
        }
        let from = layout.render_index(now);
        layout.active = target;
        layout.switch = if spec.duration.is_zero() {
            None
        } else {
            Some(WorkspaceSwitch {
                from,
                to: target as f64,
                started: now,
                spec,
            })
        };
        state.core.layout.dirty_space = true;
    }

    /// Give the keyboard back to the window the active workspace remembers.
    ///
    /// Returns whether it focused a window (and thus ran a relayout itself).
    fn restore_workspace_focus(state: &mut State, output: &smithay::output::Output) -> bool {
        let Some(layout) = state.core.layout.outputs.get(output) else {
            return false;
        };
        let remembered = visible_focus(state, layout.ws());
        let Some(id) = remembered else {
            // An empty workspace has nothing to focus: leave the keyboard with
            // no window (layers may still hold it).
            set_window_focus(state, None);
            update_keyboard_focus(state);
            return false;
        };
        let Some(window) = state.core.layout.windows.get(&id) else {
            return false;
        };
        let window = window.window.clone();
        let root = state
            .core
            .layout
            .windows
            .get(&id)
            .map(|w| return w.root.clone());
        focus_window(state, &window, root, SERIAL_COUNTER.next_serial());
        return true;
    }

    /// Move the focused window to workspace `index` of its output and follow.
    fn move_focused_to_workspace(state: &mut State, index: usize) {
        let Some(id) = focused_id(state) else {
            return;
        };
        let Some((output, current)) = state.core.layout.workspace_of(id) else {
            return;
        };
        if current == index {
            return;
        }
        let default_layout = state.core.layout.default_layout.clone();
        let tiled = state
            .core
            .layout
            .windows
            .get(&id)
            .is_some_and(|window| return window.tiled);
        let Some(layout) = state.core.layout.outputs.get_mut(&output) else {
            return;
        };
        layout.ensure_workspace(index, &default_layout);
        for workspace in &mut layout.workspaces {
            workspace.tiled.retain(|entry| return *entry != id);
            workspace.floating.retain(|entry| return *entry != id);
            if workspace.focus == Some(id) {
                workspace.focus = None;
            }
        }
        if let Some(workspace) = layout.workspaces.get_mut(index) {
            if tiled {
                workspace.tiled.push(id);
            } else {
                workspace.floating.push(id);
            }
            workspace.focus = Some(id);
        }
        // The workspace it left keeps a sensible focus. Minimized windows are
        // not candidates: they are not on screen.
        let left = state
            .core
            .layout
            .outputs
            .get(&output)
            .and_then(|layout| layout.workspaces.get(current))
            .and_then(|workspace| return visible_focus(state, workspace));
        if let Some(left) = left {
            let previous = state
                .core
                .layout
                .outputs
                .get_mut(&output)
                .and_then(|layout| layout.workspaces.get_mut(current));
            if let Some(workspace) = previous {
                workspace.focus = Some(left);
            }
        }
        // Leaving a workspace cancels its fullscreen: the window is now shown
        // in the new one at its normal layout size.
        if let Some(window) = state.core.layout.windows.get_mut(&id) {
            window.fullscreen = None;
            window.animation = None;
            window.drag_rect = None;
            window.drop_preview = None;
            window.output = Some(output.clone());
        }
        state.core.layout.dirty_space = true;
        Self::switch_workspace(state, &output, index);
    }

    /// Called once per event-loop iteration.
    pub fn tick(state: &mut State) {
        let now = Instant::now();
        // Layout handlers call `ctx.relayout()` when they changed state that
        // only `arrange` can see. Consume the request here instead of arranging
        // directly from the script. Clear it again afterwards: an `arrange` that
        // calls `relayout()` is already laying out and must not queue a new
        // frame forever.
        let relayout_requested = state
            .core
            .layout
            .programs
            .as_ref()
            .is_some_and(|programs| return programs.take_relayout());
        if relayout_requested {
            Self::relayout(state);
            if let Some(programs) = state.core.layout.programs.as_ref() {
                programs.take_relayout();
            }
        }
        notify_released_transactions(state);
        // A resize that ended but whose client never commits the final size
        // (it entered a nested event loop, say) must not keep drawing at the
        // old committed size forever. The layout target already is the last
        // requested rectangle, so dropping the state snaps to it.
        let timed_out = state
            .core
            .layout
            .windows
            .iter()
            .filter_map(|(id, managed)| {
                let since = managed.resize.as_ref()?.end_since?;
                return (now.saturating_duration_since(since) >= RESIZE_END_TIMEOUT).then_some(*id);
            })
            .collect::<Vec<_>>();
        if !timed_out.is_empty() {
            for id in timed_out {
                if let Some(managed) = state.core.layout.windows.get_mut(&id) {
                    managed.resize = None;
                }
            }
            state.core.layout.dirty_space = true;
            state.core.queue_redraw_all();
        }
        let completed = state
            .core
            .layout
            .pending
            .as_ref()
            .is_some_and(|pending| return pending.watcher.is_completed());
        if completed {
            if let Some(pending) = state.core.layout.pending.take() {
                for window in state.core.layout.windows.values_mut() {
                    if window
                        .pending
                        .as_ref()
                        .is_some_and(|pending| return pending.transaction.is_completed())
                    {
                        window.pending = None;
                    }
                }
                apply_targets(state, pending.targets, now);
                // Smithay keeps a commit blocked in the client's transaction
                // queue until it is told that the blocker was released. Without
                // this call a window that committed before its reflow partner
                // would stay frozen forever (only its last buffer still drawn).
                notify_blockers_cleared(state, &pending.clients);
                state.core.layout.dirty_space = true;
                state.core.queue_redraw_all();
            }
        }

        let now = Instant::now();
        let closing_before = state.core.layout.closing.len();
        state
            .core
            .layout
            .closing
            .retain(|closing| return closing.running(now));
        if state.core.layout.closing.len() != closing_before {
            state.core.queue_redraw_all();
        }
        let animating = state
            .core
            .layout
            .windows
            .values()
            .any(|window| return window.is_animating(now))
            || !state.core.layout.closing.is_empty();
        let switching = state
            .core
            .layout
            .outputs
            .values()
            .any(|layout| return layout.is_switching(now));
        // A sliding Toast needs frames too, and it is the only animation that
        // exists without a window behind it (a broken configuration at startup).
        let toasts_sliding = state.core.toasts.animating(now);
        let animating_now = animating || switching || toasts_sliding;
        let was_animating =
            std::mem::replace(&mut state.core.layout.animations_running, animating_now);
        if animating_now || was_animating || state.core.layout.dirty_space {
            sync_space(state);
        }
        // Every output has to drop its own finished switch: `.any` would
        // short-circuit and leave the other outputs with a stale transition.
        for layout in state.core.layout.outputs.values_mut() {
            layout.finish_switch(now);
        }
        if animating_now {
            // Kick the frame loop only when it is not already running. `tick`
            // runs on every event-loop iteration, so queueing unconditionally
            // asked the backend for a frame many times per real frame; each
            // attempt that arrived before the frame boundary returned
            // `Skipped` (measured: ~3 wasted attempts per presented frame).
            // From here on the backends drive it: a submitted frame asks for
            // the next one while `animations_running` holds.
            if !was_animating {
                state.core.queue_redraw_all();
            }
            ensure_animation_timer(state);
        } else {
            if let Some(token) = state.core.layout.animation_timer.take() {
                state.core.event_loop_handle.remove(token);
            }
            if was_animating {
                // The tick that crossed the duration boundary runs after the
                // last frame was queued at < 100%. Repaint once at the exact
                // target (alpha 1, scale 1, final workspace index).
                state.core.queue_redraw_all();
            }
        }
    }

    /// Start the fade-out for an already baked window snapshot.
    pub fn start_closing(state: &mut State, mut closing: ClosingWindow) {
        // The snapshot may have been baked earlier than the actual close (a
        // subsurface destroyed first, or a slow STM transaction). Start the
        // clock now, when the window is removed from the layout, like niri's
        // `start_close_animation_for_window`; otherwise the animation can
        // expire before the first frame is ever drawn.
        closing.started = Instant::now();
        if !closing.running(closing.started) {
            return;
        }
        state.core.layout.closing.push(closing);
        state.core.queue_redraw_all();
        ensure_animation_timer(state);
    }

    /// Closing windows, regardless of output, for rendering.
    pub fn closing_windows(&self) -> &[ClosingWindow] {
        return &self.closing;
    }

    /// Whether the last tick was still driving an animation.
    ///
    /// Backends read this when a frame is finished: on a real VBlank an ongoing
    /// animation is the reason to ask for the next frame right away (niri does
    /// the same through `unfinished_animations_remain`), which keeps the
    /// animation locked to the refresh rate instead of a wall-clock timer.
    pub fn animations_running(&self) -> bool {
        return self.animations_running;
    }

    /// The visible rectangle of a managed window, for hits and grabs.
    pub fn window_rect(state: &State, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let managed = state.core.layout.by_window(window)?;
        let now = Instant::now();
        let local = managed.rect_at(now);
        let Some((output, workspace)) = state.core.layout.workspace_of(managed.id) else {
            return Some(local);
        };
        let Some(area) = Self::output_rect(state, &output) else {
            return Some(local);
        };
        return Some(
            state
                .core
                .layout
                .view(&output, area, now)
                .project(workspace, local)
                .unwrap_or(local),
        );
    }

    /// Make a tiled window floating at `rect` (start of an interactive move or
    /// resize).
    pub fn float_window(state: &mut State, window: &Window, rect: Rectangle<i32, Logical>) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        let Some(output) = state
            .core
            .layout
            .windows
            .get(&id)
            .and_then(|w| return w.output.clone())
        else {
            return;
        };
        let Some(managed) = state.core.layout.windows.get_mut(&id) else {
            return;
        };
        if managed.fullscreen.is_some() {
            return;
        }
        trace!(
            "float window {id}: rect {rect:?} was target {:?}",
            managed.target_rect
        );
        managed.tiled = false;
        managed.floating_rect = rect;
        managed.target_rect = rect;
        managed.animation = None;
        if let Some(layout) = state.core.layout.outputs.get_mut(&output) {
            layout.ws_mut().tiled.retain(|entry| return *entry != id);
            if !layout.ws().floating.contains(&id) {
                layout.ws_mut().floating.push(id);
            }
        }
        state.core.layout.dirty_space = true;
        state.core.queue_redraw_all();
    }

    /// Update a floating window's rectangle during a drag.
    pub fn move_floating(state: &mut State, window: &Window, rect: Rectangle<i32, Logical>) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        let Some(managed) = state.core.layout.windows.get_mut(&id) else {
            return;
        };
        if !managed.tiled {
            managed.floating_rect = rect;
            managed.target_rect = rect;
            managed.animation = None;
            // A configure transaction started by the reflow that floated this
            // window must not snap it back while the user is still dragging.
            if let Some(pending) = &mut state.core.layout.pending {
                pending.targets.remove(&id);
            }
            state.core.layout.dirty_space = true;
        }
    }

    /// Start reordering a tiled window (`window_drag.tiled = "reorder"`).
    ///
    /// The window stays in the layout list; only its visual rectangle detaches
    /// and follows the pointer. The layout reflows around its new slot, which
    /// is also drawn as the drop indicator.
    pub fn begin_tiled_reorder(state: &mut State, window: &Window) -> bool {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return false;
        };
        let now = Instant::now();
        let Some(managed) = state.core.layout.windows.get_mut(&id) else {
            return false;
        };
        if !managed.tiled || managed.fullscreen.is_some() {
            return false;
        }
        let visual = managed.rect_at(now);
        let slot = managed.target_rect;
        managed.drag_rect = Some(visual);
        managed.drop_preview = Some(slot);
        managed.animation = None;
        if let Some(pending) = &mut state.core.layout.pending {
            pending.targets.remove(&id);
        }
        state.core.layout.dirty_space = true;
        state.core.queue_redraw_all();
        return true;
    }

    /// Move a reordering tiled window to the layout position under `pointer`.
    ///
    /// `visual` is where the window follows the pointer. The layout target and
    /// drop indicator are recomputed only when the insertion position actually
    /// changes, so a slow drag does not reflow on every motion event.
    pub fn update_tiled_reorder(
        state: &mut State,
        window: &Window,
        pointer: Point<f64, Logical>,
        visual: Rectangle<i32, Logical>,
    ) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        let Some((output, _)) = Self::work_area_at(state, pointer) else {
            if let Some(managed) = state.core.layout.windows.get_mut(&id) {
                managed.drag_rect = Some(visual);
            }
            state.core.layout.dirty_space = true;
            state.core.queue_redraw_all();
            return;
        };

        let mut changed = false;
        let current_output = state
            .core
            .layout
            .windows
            .get(&id)
            .and_then(|managed| return managed.output.clone());
        if current_output.as_ref() != Some(&output) {
            Self::assign_output(state, window, output.clone());
            changed = true;
        }

        let index = insertion_index(state, &output, id, pointer);
        if reorder_tiled(state, &output, id, index) {
            changed = true;
        }

        if let Some(managed) = state.core.layout.windows.get_mut(&id) {
            managed.drag_rect = Some(visual);
            if managed.drop_preview.is_none() {
                managed.drop_preview = Some(managed.target_rect);
            }
        }
        if changed {
            Self::relayout(state);
        }
        state.core.layout.dirty_space = true;
        state.core.queue_redraw_all();
    }

    /// Drop a reordering tiled window into its previewed slot.
    pub fn finish_tiled_reorder(state: &mut State, window: &Window) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        let now = Instant::now();
        let Some(managed) = state.core.layout.windows.get_mut(&id) else {
            return;
        };
        let from = managed
            .drag_rect
            .unwrap_or_else(|| return managed.rect_at(now));
        managed.drag_rect = None;
        managed.drop_preview = None;
        managed.animation = None;
        // Leave the window where the user dropped it; `relayout` then animates
        // from here into the previewed layout slot.
        managed.target_rect = from;
        Self::relayout(state);
        state.core.queue_redraw_all();
    }

    /// Put a reordering tiled window back into its original slot (Escape).
    pub fn cancel_tiled_reorder(state: &mut State, window: &Window, slot: &TiledSlot) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        if let Some(managed) = state.core.layout.windows.get_mut(&id) {
            managed.drag_rect = None;
            managed.drop_preview = None;
        }
        Self::restore_tiled_slot(state, window, slot);
    }

    /// The drop indicator of a reordering window on `output`, if any: the
    /// workspace it would land in and its rectangle there.
    pub fn drag_hint_for(
        &self,
        output: &smithay::output::Output,
    ) -> Option<(usize, Rectangle<i32, Logical>, &ManagedWindow)> {
        return self.windows.values().find_map(|window| {
            if window.drag_rect.is_some() && window.output.as_ref() == Some(output) {
                let (_, workspace) = self.workspace_of(window.id)?;
                return window
                    .drop_preview
                    .map(|rect| return (workspace, rect, window));
            }
            return None;
        });
    }

    /// Whether `managed` is currently a tiled window following the pointer.
    pub fn is_reordering(managed: &ManagedWindow) -> bool {
        return managed.drag_rect.is_some();
    }

    /// Begin an interactive resize of a floating window.
    ///
    /// `rect` is the window's rectangle when the drag started and `anchor`
    /// names the edges that stay on screen. The first configure announces
    /// `Resizing`, so the client can switch to its cheap resize path right
    /// away, and carries the first new size.
    pub fn begin_resize(
        state: &mut State,
        window: &Window,
        rect: Rectangle<i32, Logical>,
        anchor: ResizeAnchor,
    ) -> bool {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return false;
        };
        let Some(managed) = state.core.layout.windows.get_mut(&id) else {
            return false;
        };
        if managed.fullscreen.is_some() || managed.tiled || managed.drag_rect.is_some() {
            return false;
        }
        let origin = Point::from((
            if anchor.fixed_right {
                rect.loc.x + rect.size.w
            } else {
                rect.loc.x
            },
            if anchor.fixed_bottom {
                rect.loc.y + rect.size.h
            } else {
                rect.loc.y
            },
        ));
        managed.resize = Some(InteractiveResize {
            anchor,
            origin,
            desired: rect.size,
            committed: committed_size(&managed.window),
            drawn: drawn_size(&managed.window),
            sent: None,
            end_since: None,
        });
        managed.animation = None;
        if let Some(pending) = &mut state.core.layout.pending {
            pending.targets.remove(&id);
        }
        Self::drive_resize(state, id, true);
        return true;
    }

    /// Apply a resize request to a floating window.
    ///
    /// Called once per pointer frame. The size is only recorded; a configure
    /// goes out when the client acknowledged the previous one (see
    /// [`InteractiveResize::caught_up`]) or when the grab asks for the last
    /// one. Without that throttle a 1000 Hz mouse produces one configure per
    /// pointer frame and the client can never catch up.
    pub fn resize_floating(state: &mut State, window: &Window, rect: Rectangle<i32, Logical>) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        let resizing = {
            let Some(managed) = state.core.layout.windows.get_mut(&id) else {
                return;
            };
            if managed.fullscreen.is_some() {
                return;
            }
            match &mut managed.resize {
                Some(resize) => {
                    if resize.end_since.is_some() {
                        return;
                    }
                    resize.desired = rect.size;
                    true
                }
                None => {
                    // No grab started one (an internal caller): apply it directly.
                    managed.floating_rect = rect;
                    managed.target_rect = rect;
                    managed.animation = None;
                    false
                }
            }
        };
        if !resizing {
            state.core.layout.dirty_space = true;
            state.core.queue_redraw_all();
            return;
        }
        Self::drive_resize(state, id, false);
    }

    /// Finish an interactive resize: the last size is always sent, and that
    /// configure clears `Resizing`.
    pub fn end_resize(state: &mut State, window: &Window) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        {
            let Some(managed) = state.core.layout.windows.get_mut(&id) else {
                return;
            };
            let Some(resize) = &mut managed.resize else {
                return;
            };
            if resize.end_since.is_none() {
                resize.end_since = Some(Instant::now());
            }
        }
        // The final configure is allowed to overtake the throttle: it is the
        // size the user chose, sent exactly once, and it is also the configure
        // that clears `Resizing`.
        Self::drive_resize(state, id, true);
    }

    /// Abort an interactive resize and put `window` back at `rect` (Escape).
    pub fn cancel_resize(state: &mut State, window: &Window, rect: Rectangle<i32, Logical>) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        {
            let Some(managed) = state.core.layout.windows.get_mut(&id) else {
                return;
            };
            managed.resize = None;
            managed.floating_rect = rect;
            managed.target_rect = rect;
            managed.animation = None;
            // The clear of `Resizing` is part of the configure, even when the
            // size itself did not change.
            send_resize_configure(managed, rect.size, false);
        }
        state.core.layout.dirty_space = true;
        state.core.queue_redraw_all();
    }

    /// The window committed; the client may just have acknowledged a resize
    /// configure. Send the next size if it caught up, or release the resize
    /// state once the final size is on screen.
    pub fn on_window_commit(state: &mut State, window: &Window) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        let active = state
            .core
            .layout
            .windows
            .get(&id)
            .is_some_and(|managed| return managed.resize.is_some());
        if !active {
            return;
        }
        Self::drive_resize(state, id, false);
    }

    /// Send the resize configure if the client caught up, or when `force`d.
    fn drive_resize(state: &mut State, id: u64, force: bool) {
        // Clients that batch size changes handle the throttle fine; the switch
        // exists for clients that do not (niri: `disable_resize_throttling`).
        let throttle = state.core.config.window_drag.throttle_resize;
        let mut configure = None;
        let finished;
        {
            let Some(managed) = state.core.layout.windows.get_mut(&id) else {
                return;
            };
            let Some(resize) = &mut managed.resize else {
                return;
            };
            // The client acks a configure on its next commit; that ack is the
            // only signal that the previous request landed.
            resize.committed = committed_size(&managed.window);
            resize.drawn = drawn_size(&managed.window);
            let size = resize.size();
            let ended = resize.end_since.is_some();
            // The layout reports the requested rectangle, anchored on the
            // edges the pointer does not move; `rect_at` replaces its size
            // with the client's committed one until the client catches up.
            let rect = resize.rect_for(size);
            managed.floating_rect = rect;
            managed.target_rect = rect;
            managed.animation = None;
            // Only a new size needs a configure (the first and the last one
            // are forced: they set and clear `Resizing`). Re-sending the same
            // size on every commit would make the client lay out and redraw
            // again, commit, get the same configure, and so on: it would spend
            // every frame on that loop instead of following the pointer.
            let changed = resize.sent != Some(size);
            if force || (changed && (!throttle || resize.caught_up())) {
                resize.sent = Some(size);
                configure = Some((size, !ended));
            }
            // The final configure is out and the client already drew that
            // size: nothing is left to wait for.
            finished = ended && resize.sent == Some(size) && resize.committed == size;
        }
        if let Some(pending) = &mut state.core.layout.pending {
            pending.targets.remove(&id);
        }
        if let Some(managed) = state.core.layout.windows.get_mut(&id) {
            if let Some((size, resizing)) = configure {
                send_resize_configure(managed, size, resizing);
            }
            if finished {
                managed.resize = None;
            }
        }
        // Throttled frames still have to repaint: the anchored rectangle may
        // have moved even though no new configure went out.
        state.core.layout.dirty_space = true;
        state.core.queue_redraw_all();
    }

    /// Where a tiled window sits now, for cancelling a drag.
    pub fn tiled_slot(state: &State, window: &Window) -> Option<TiledSlot> {
        let managed = state.core.layout.by_window(window)?;
        if !managed.tiled {
            return None;
        }
        let (output, workspace) = state.core.layout.workspace_of(managed.id)?;
        let layout = state.core.layout.outputs.get(&output)?;
        let entry = layout.workspaces.get(workspace)?;
        let index = entry.tiled.iter().position(|id| return *id == managed.id)?;
        let slot = TiledSlot {
            output,
            workspace,
            index,
            rect: managed.rect_at(Instant::now()),
        };
        trace!(
            "tiled slot for window {}: index {} rect {:?} target {:?}",
            managed.id, slot.index, slot.rect, managed.target_rect
        );
        return Some(slot);
    }

    /// Put a dragged-out window back into the tiled position it came from.
    pub fn restore_tiled_slot(state: &mut State, window: &Window, slot: &TiledSlot) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        trace!("restoring window {id} to tiled slot {:?}", slot.rect);
        let Some(managed) = state.core.layout.windows.get_mut(&id) else {
            return;
        };
        managed.tiled = true;
        managed.fullscreen = None;
        managed.output = Some(slot.output.clone());
        managed.target_rect = slot.rect;
        managed.animation = None;
        managed.floating_rect = slot.rect;
        for layout in state.core.layout.outputs.values_mut() {
            for workspace in &mut layout.workspaces {
                workspace.tiled.retain(|entry| return *entry != id);
                workspace.floating.retain(|entry| return *entry != id);
            }
        }
        state.core.layout.ensure_output(&slot.output);
        let default_layout = state.core.layout.default_layout.clone();
        if let Some(layout) = state.core.layout.outputs.get_mut(&slot.output) {
            layout.ensure_workspace(slot.workspace, &default_layout);
            if let Some(workspace) = layout.workspaces.get_mut(slot.workspace) {
                let index = slot.index.min(workspace.tiled.len());
                workspace.tiled.insert(index, id);
            }
        }
        state.core.layout.dirty_space = true;
        Self::relayout(state);
        state.core.queue_redraw_all();
    }

    /// Move a window's output association (cross-output drag).
    pub fn assign_output(state: &mut State, window: &Window, output: smithay::output::Output) {
        let Some(id) = state
            .core
            .layout
            .by_window(window)
            .map(|managed| return managed.id)
        else {
            return;
        };
        let same = state
            .core
            .layout
            .windows
            .get(&id)
            .is_some_and(|managed| return managed.output.as_ref() == Some(&output));
        if same {
            return;
        }
        let tiled = state
            .core
            .layout
            .windows
            .get(&id)
            .is_some_and(|managed| return managed.tiled);
        for layout in state.core.layout.outputs.values_mut() {
            for workspace in &mut layout.workspaces {
                workspace.tiled.retain(|entry| return *entry != id);
                workspace.floating.retain(|entry| return *entry != id);
            }
        }
        if let Some(managed) = state.core.layout.windows.get_mut(&id) {
            managed.output = Some(output.clone());
        }
        state.core.layout.ensure_output(&output);
        if let Some(layout) = state.core.layout.outputs.get_mut(&output) {
            if tiled {
                layout.ws_mut().tiled.push(id);
            } else {
                layout.ws_mut().floating.push(id);
            }
        }
        state.core.layout.dirty_space = true;
        state.core.queue_redraw_all();
    }

    /// The work area of the output containing `location`, if any.
    pub fn work_area_at(
        state: &State,
        location: Point<f64, Logical>,
    ) -> Option<(smithay::output::Output, Rectangle<i32, Logical>)> {
        let output = state.core.output_at(location)?;
        let area = Self::work_area(state, &output)?;
        return Some((output, area));
    }

    /// Whether a fullscreen window on the active workspace of `output` covers
    /// `location` and Top layers.
    pub fn fullscreen_at(
        &self,
        output: &smithay::output::Output,
        area: Rectangle<i32, Logical>,
        location: Point<f64, Logical>,
        now: Instant,
    ) -> bool {
        let Some(layout) = self.outputs.get(output) else {
            return false;
        };
        let view = self.view(output, area, now);
        let workspace = layout.active;
        return layout
            .ws()
            .tiled
            .iter()
            .chain(layout.ws().floating.iter())
            .any(|id| {
                let Some(window) = self.windows.get(id) else {
                    return false;
                };
                if window.fullscreen != Some(FullscreenMode::Fullscreen) {
                    return false;
                }
                return view
                    .project(workspace, window.rect_at(now))
                    .is_some_and(|rect| return rect.to_f64().contains(location));
            });
    }
}

/// The duration and curve of a workspace slide.
fn workspace_animation(state: &State) -> AnimationSpec {
    return state.core.config.general.animations.workspace_switch();
}

/// Lay out the workspace cards at one instant.
///
/// Every card is the output rectangle, stacked vertically and anchored so
/// that `render_index` (the active workspace, fractional during a slide)
/// sits on the output: a slide shows two full screens moving past each other.
fn workspace_cards(
    area: Rectangle<i32, Logical>,
    count: usize,
    render_index: f64,
) -> Vec<WorkspaceCard> {
    let count = count.max(1);
    let height = f64::from(area.size.h);
    let mut cards = Vec::with_capacity(count);
    for index in 0..count {
        let top = (index as f64 - render_index) * height;
        cards.push(WorkspaceCard {
            rect: Rectangle::new(
                Point::from((area.loc.x, area.loc.y + top.round() as i32)),
                area.size,
            ),
        });
    }
    return cards;
}

fn ensure_all_outputs(manager: &mut LayoutManager, outputs: &[smithay::output::Output]) {
    for output in outputs {
        manager.ensure_output(output);
    }
    let first = outputs.first().cloned();
    let ids = manager.windows.keys().copied().collect::<Vec<_>>();
    for id in ids {
        let output = manager
            .windows
            .get(&id)
            .and_then(|window| return window.output.clone());
        if output
            .as_ref()
            .is_some_and(|output| return outputs.contains(output))
        {
            continue;
        }
        let Some(first) = first.clone() else {
            continue;
        };
        let tiled = manager
            .windows
            .get(&id)
            .is_some_and(|window| return window.tiled);
        if let Some(window) = manager.windows.get_mut(&id) {
            window.output = Some(first.clone());
        }
        manager.ensure_output(&first);
        if let Some(layout) = manager.outputs.get_mut(&first) {
            if tiled {
                layout.ws_mut().tiled.push(id);
            } else {
                layout.ws_mut().floating.push(id);
            }
        }
    }
}

fn ensure_animation_timer(state: &mut State) {
    // Remember that a frame sequence is running even if the first timer tick
    // lands after a very short duration; `tick` then repaints the exact target
    // state once instead of leaving the last sub-100% frame on screen.
    state.core.layout.animations_running = true;
    if state.core.layout.animation_timer.is_some() {
        return;
    }
    // The animation itself is advanced by the backends: a submitted frame ends
    // in `frame_finish` (udev) or a host `Redraw` (winit), and that is where
    // the next frame is requested. Both then land exactly on the refresh grid,
    // one animation step per presented frame.
    //
    // This timer is only a watchdog for the case where no backend drives
    // frames at all (headless, session paused). It must stay far slower than a
    // frame, otherwise it would queue redraws on its own wall-clock phase,
    // racing the backend: that is what the old fixed 16 ms timer did, and the
    // resulting two clocks left 11 wasted render attempts per presented frame.
    let interval = crate::core::redraw::animation_interval(&state.core);
    let deadline = Instant::now() + (interval * 6).max(crate::core::redraw::ANIMATION_WATCHDOG);
    let handle = state.core.event_loop_handle.clone();
    match handle.insert_source(
        smithay::reexports::calloop::timer::Timer::from_deadline(deadline),
        |_, _, state: &mut State| {
            state.core.layout.animation_timer = None;
            LayoutManager::tick(state);
            return smithay::reexports::calloop::timer::TimeoutAction::Drop;
        },
    ) {
        Ok(token) => state.core.layout.animation_timer = Some(token),
        Err(err) => warn!("error arming the animation timer: {err:?}"),
    }
}

/// Drop the in-flight layout transaction and remember its clients: their
/// surfaces may have blocked a commit that must be released once the
/// transaction is gone.
fn supersede_pending(state: &mut State) {
    if let Some(pending) = state.core.layout.pending.take() {
        // Dropping the per-window transaction references completes the old
        // transaction; its watcher then reports the release.
        for window in state.core.layout.windows.values_mut() {
            window.pending = None;
        }
        state.core.layout.released.push(ReleasedTransaction {
            watcher: pending.watcher,
            clients: pending.clients,
        });
    }
}

/// Notify clients of transactions that have finished since the last iteration.
///
/// A transaction can be superseded by a newer reflow before it completes. The
/// blocked commit must still be released, otherwise the window stays frozen.
fn notify_released_transactions(state: &mut State) {
    let mut index = 0;
    while index < state.core.layout.released.len() {
        if state.core.layout.released[index].watcher.is_completed() {
            let released = state.core.layout.released.swap_remove(index);
            notify_blockers_cleared(state, &released.clients);
        } else {
            index += 1;
        }
    }
}

/// Tell Smithay that layout blockers for `clients` are gone, so it applies
/// commits it queued behind them.
fn notify_blockers_cleared(state: &mut State, clients: &[Client]) {
    if clients.is_empty() {
        return;
    }
    let display_handle = state.core.display_handle.clone();
    for client in clients {
        state
            .client_compositor_state(client)
            .blocker_cleared(state, &display_handle);
    }
}

fn apply_desired(state: &mut State, desired: HashMap<u64, Rectangle<i32, Logical>>, now: Instant) {
    let transaction = Transaction::new();
    let mut targets = HashMap::new();
    let mut clients = Vec::new();
    let mut has_pending = false;
    let general_no_csd = state.core.config.general.prefer_no_csd;
    let focus_root = state.core.last_window_focus.clone();
    let event_loop_handle = state.core.event_loop_handle.clone();

    for (id, mut rect) in desired {
        let (output, root, fullscreen) = {
            let Some(managed) = state.core.layout.windows.get(&id) else {
                continue;
            };
            (
                managed.output.clone(),
                managed.root.clone(),
                managed.fullscreen,
            )
        };
        if fullscreen != Some(FullscreenMode::Fullscreen) {
            let Some(managed) = state.core.layout.windows.get(&id) else {
                continue;
            };
            rect = clamp_to_hints(&managed.window, rect);
        }
        if rect.size.w <= 0 || rect.size.h <= 0 {
            continue;
        }
        // While a tiled window is being dragged to a new layout slot, the
        // layout still decides where it will land, but the window itself must
        // follow the pointer and must not be reconfigured mid-drag. Record the
        // slot as the drop indicator and keep its visual rectangle untouched.
        {
            let Some(managed) = state.core.layout.windows.get_mut(&id) else {
                continue;
            };
            if managed.drag_rect.is_some() {
                managed.drop_preview = Some(rect);
                managed.target_rect = rect;
                managed.animation = None;
                continue;
            }
            // An interactive resize owns this window's configure pacing. A
            // layout reflow in the middle of a drag must not bypass the
            // throttle and queue another size on top of the one in flight.
            if managed.resize.is_some() {
                continue;
            }
        }
        trace!("layout desired window {id}: {rect:?}");
        let bounds = output
            .as_ref()
            .and_then(|output| return LayoutManager::work_area(state, output))
            .map(|area| return area.size);
        let focused = focus_root
            .as_ref()
            .is_some_and(|focused| return focused == &root);
        let serial = {
            let Some(managed) = state.core.layout.windows.get_mut(&id) else {
                continue;
            };
            send_configure(managed, rect, focused, bounds, general_no_csd)
        };
        if let Some(serial) = serial {
            trace!("layout configure window {id}: serial {serial:?} rect {rect:?}");
            if let Some(managed) = state.core.layout.windows.get_mut(&id) {
                if let Some(client) = managed.root.client() {
                    if !clients
                        .iter()
                        .any(|other: &Client| return other.id() == client.id())
                    {
                        clients.push(client);
                    }
                }
                managed.pending = Some(WindowPending {
                    serial,
                    transaction: transaction.clone(),
                });
            }
            has_pending = true;
        }
        targets.insert(id, rect);
    }

    if has_pending {
        let watcher = transaction.watcher();
        transaction.register_deadline_timer(&event_loop_handle);
        state.core.layout.pending = Some(PendingLayout {
            targets,
            watcher,
            clients,
        });
    } else {
        apply_targets(state, targets, now);
    }
}

fn apply_targets(state: &mut State, targets: HashMap<u64, Rectangle<i32, Logical>>, now: Instant) {
    let manager = &mut state.core.layout;
    let movement_spec = manager.animations.window_movement();
    let resize_spec = manager.animations.window_resize();
    for (id, target) in targets {
        let Some(window) = manager.windows.get_mut(&id) else {
            continue;
        };
        trace!("layout target window {id}: {target:?}");
        if window.target_rect == target {
            continue;
        }
        let from = window.rect_at(now);
        window.target_rect = target;
        // A fullscreen window keeps its floating rectangle so leaving
        // fullscreen returns it to where the user had put it.
        if !window.tiled && window.fullscreen.is_none() {
            window.floating_rect = target;
        }
        // A layout reflow can move and resize a window at once. niri runs a
        // separate animation for each property, so a pure move does not pay
        // the resize duration and vice versa.
        let movement =
            (from.loc != target.loc && !movement_spec.duration.is_zero()).then_some(movement_spec);
        let resize =
            (from.size != target.size && !resize_spec.duration.is_zero()).then_some(resize_spec);
        window.animation = if movement.is_some() || resize.is_some() {
            Some(Animation {
                from,
                to: target,
                started: now,
                movement,
                resize,
            })
        } else {
            None
        };
    }
    manager.dirty_space = true;
    state.core.queue_redraw_all();
}

fn sync_space(state: &mut State) {
    let now = Instant::now();
    let outputs = state.core.space.outputs().cloned().collect::<Vec<_>>();
    let mut tiled = Vec::new();
    let mut floating = Vec::new();
    let mut fullscreen = Vec::new();
    for output in &outputs {
        let Some(area) = state.core.space.output_geometry(output) else {
            continue;
        };
        for placed in state.core.layout.windows_on_output(output, area, now) {
            let id = placed.managed.id;
            let location = placed.rect.loc;
            if placed.managed.fullscreen == Some(FullscreenMode::Fullscreen) {
                fullscreen.push((id, location));
            } else if placed.managed.tiled {
                tiled.push((id, location));
            } else {
                floating.push((id, location));
            }
        }
    }
    let mut order = tiled;
    order.extend(floating);
    order.extend(fullscreen);
    let known = order
        .iter()
        .map(|(id, _)| return *id)
        .collect::<HashSet<_>>();
    for id in state.core.layout.windows.keys().copied() {
        if known.contains(&id) {
            continue;
        }
        let Some(managed) = state.core.layout.windows.get(&id) else {
            continue;
        };
        // A minimized window is deliberately absent from the space; the
        // fallback exists for windows the layout does not place yet, not for
        // ones the user hid.
        if managed.minimized {
            continue;
        }
        let location = managed.rect_at(now).loc;
        order.push((id, location));
    }

    for (id, location) in order {
        let Some(window) = state
            .core
            .layout
            .windows
            .get(&id)
            .map(|managed| return managed.window.clone())
        else {
            continue;
        };
        // Always map, even when the location is unchanged: `Space::map_element`
        // is what applies the back-to-front order. Skipping unchanged windows
        // would leave them at their old stacking position.
        trace!("layout places window {id} at {location:?}");
        state.core.space.map_element(window, location, false);
    }
    state.core.layout.dirty_space = false;
}

impl ManagedWindow {
    /// The rendered rectangle at `now`.
    pub fn rect_at(&self, now: Instant) -> Rectangle<i32, Logical> {
        if let Some(rect) = self.drag_rect {
            return rect;
        }
        // An interactive resize draws the client's committed size on the edges
        // the pointer does not move. Stretching the old buffer to the requested
        // size instead is what makes a window look blurred while it grows; the
        // client only ever sees the sizes the compositor asked for, so it
        // catches up within a frame or two.
        if let Some(resize) = &self.resize {
            return resize.rect_for(resize.drawn);
        }
        let Some(animation) = &self.animation else {
            return self.target_rect;
        };
        let elapsed = now.saturating_duration_since(animation.started);
        let lerp = |from: i32, to: i32, progress: f64| {
            return (f64::from(from) + (f64::from(to) - f64::from(from)) * progress).round() as i32;
        };
        let (x, y) = match animation.movement {
            Some(spec) => {
                let progress = spec.progress(elapsed);
                (
                    lerp(animation.from.loc.x, animation.to.loc.x, progress),
                    lerp(animation.from.loc.y, animation.to.loc.y, progress),
                )
            }
            None => (animation.to.loc.x, animation.to.loc.y),
        };
        let (w, h) = match animation.resize {
            Some(spec) => {
                let progress = spec.progress(elapsed);
                (
                    lerp(animation.from.size.w, animation.to.size.w, progress).max(1),
                    lerp(animation.from.size.h, animation.to.size.h, progress).max(1),
                )
            }
            None => (animation.to.size.w, animation.to.size.h),
        };
        return Rectangle::new(Point::from((x, y)), Size::from((w, h)));
    }

    /// Eased progress of the opening animation (`0.0 ..= 1.0`).
    pub fn open_progress(&self, now: Instant) -> f64 {
        let Some(open) = &self.open_animation else {
            return 1.0;
        };
        return open
            .spec
            .progress(now.saturating_duration_since(open.started));
    }

    /// The scale applied while the window opens (niri: 50% → 100%).
    pub fn open_scale_at(&self, now: Instant) -> f64 {
        let progress = self.open_progress(now);
        return (progress / 2.0 + 0.5).max(0.0);
    }

    /// The window rectangle after applying the opening scale around its centre.
    ///
    /// The scaled rectangle is what is actually drawn (and what a close
    /// snapshot must capture if the window is closed mid-open); hit testing
    /// keeps using [`ManagedWindow::rect_at`] at the target geometry.
    pub fn open_rect_at(&self, now: Instant) -> Rectangle<i32, Logical> {
        let rect = self.rect_at(now);
        let scale = self.open_scale_at(now);
        if scale >= 0.9999 {
            return rect;
        }
        let size = rect.size.to_f64() * scale;
        let centre = rect.loc.to_f64() + rect.size.to_f64() / 2.0;
        let loc = Point::from((centre.x - size.w / 2.0, centre.y - size.h / 2.0));
        return Rectangle::new(loc.to_i32_round(), size.to_i32_round());
    }

    /// Opacity for the open animation.
    pub fn alpha_at(&self, now: Instant) -> f32 {
        return self.open_progress(now) as f32;
    }

    fn is_animating(&self, now: Instant) -> bool {
        if let Some(animation) = &self.animation {
            let elapsed = now.saturating_duration_since(animation.started);
            if animation
                .movement
                .is_some_and(|spec| return spec.running(elapsed))
                || animation
                    .resize
                    .is_some_and(|spec| return spec.running(elapsed))
            {
                return true;
            }
        }
        if let Some(open) = &self.open_animation {
            if open
                .spec
                .running(now.saturating_duration_since(open.started))
            {
                return true;
            }
        }
        return false;
    }
}

fn on_pre_commit(state: &mut State, surface: &WlSurface) {
    let root = root_surface(surface);
    let Some(id) = state.core.layout.roots.get(&root).copied() else {
        return;
    };

    // A null-buffer commit unmaps the window, and Smithay releases the last
    // client buffer when it processes that commit. Take the snapshot *before*
    // the state is applied, while the previous buffer and its textures are
    // still valid, so minimize/hide gets the same close animation as destroy.
    let pending_buffer = with_states(surface, |data| {
        let mut attributes = data.cached_state.get::<SurfaceAttributes>();
        return attributes
            .pending()
            .buffer
            .as_ref()
            .map(|buffer| return matches!(buffer, BufferAssignment::Removed));
    });
    match pending_buffer {
        Some(true) => {
            // A null-buffer commit is the final chance to see the last frame.
            // It also replaces any snapshot taken when a subsurface was
            // destroyed earlier: that one may miss content committed since.
            if let Some(closing) = crate::core::window::take_closing_snapshot(state, &root) {
                state
                    .core
                    .layout
                    .pending_unmap_closing
                    .insert(root.clone(), closing);
            }
        }
        Some(false) => {
            // A new buffer means the window stays mapped; a snapshot from an
            // earlier (possibly blocked) null commit is stale.
            state.core.layout.pending_unmap_closing.remove(&root);
        }
        None => {}
    }

    let Some(managed) = state.core.layout.windows.get(&id) else {
        return;
    };
    let Some(pending) = &managed.pending else {
        return;
    };
    let Some(toplevel) = managed.window.toplevel() else {
        return;
    };
    let acked = toplevel.with_cached_state(|state| {
        return state
            .last_acked
            .as_ref()
            .map(|configure| return configure.serial);
    });
    if acked != Some(pending.serial) {
        return;
    }

    let blocker = {
        let Some(managed) = state.core.layout.windows.get_mut(&id) else {
            return;
        };
        let Some(pending) = managed.pending.take() else {
            return;
        };
        if pending.transaction.is_last() {
            None
        } else {
            Some(pending.transaction.blocker())
        }
    };
    if let Some(blocker) = blocker {
        add_blocker(surface, blocker);
    }
}

fn send_configure(
    managed: &mut ManagedWindow,
    rect: Rectangle<i32, Logical>,
    focused: bool,
    bounds: Option<Size<i32, Logical>>,
    general_no_csd: bool,
) -> Option<Serial> {
    let toplevel = managed.window.toplevel()?;
    let fullscreen = managed.fullscreen;
    // A window rule may ask this particular window for client-side
    // decorations: then no tiled state, so the client
    // keeps its own titlebar and rounded corners.
    let no_csd = managed.visuals.no_csd;

    toplevel.with_pending_state(|pending| {
        pending.size = Some(Size::from((rect.size.w.max(1), rect.size.h.max(1))));
        pending.bounds = bounds;
        // `prefer_no_csd` publishes the decoration globals, so a rule that
        // turns CSD back on for one window has to negotiate it explicitly.
        // Windows without such a rule keep whatever the client requested.
        if general_no_csd && !no_csd {
            pending.decoration_mode = Some(
                smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode::ClientSide,
            );
        }
        // The negotiated decoration mode wins over the rule's `no_csd`: a
        // client that was told `ClientSide` keeps its decorations, so its
        // edges must not be reported as tiled (niri does the same). Floating
        // windows are marked tiled as well: they still get Poly's border and
        // shadow, so they must drop their own.
        let report_tiled = crate::core::window::decide_tiled(pending, no_csd);
        crate::core::window::set_tiled_states(pending, report_tiled);
        if fullscreen.is_some() {
            pending.states.set(ToplevelState::Fullscreen);
        } else {
            pending.states.unset(ToplevelState::Fullscreen);
        }
        if focused {
            pending.states.set(ToplevelState::Activated);
        } else {
            pending.states.unset(ToplevelState::Activated);
        }
    });
    let serial = toplevel.send_pending_configure();
    if serial.is_some() {
        managed.needs_frame_callback = true;
    }
    return serial;
}

fn set_fullscreen_mode(state: &mut State, id: u64, mode: Option<FullscreenMode>) {
    let Some(managed) = state.core.layout.windows.get_mut(&id) else {
        return;
    };
    if managed.fullscreen == mode {
        return;
    }
    managed.fullscreen = mode;
    managed.animation = None;
    state.core.layout.dirty_space = true;
}

pub(crate) fn focused_id(state: &State) -> Option<u64> {
    let root = state.core.last_window_focus.as_ref()?;
    return state.core.layout.roots.get(root).copied();
}

/// Merge an order returned by a scripted layout into the full tiled list.
///
/// The layout only sees the windows it places, so `order` is a permutation of
/// the visible ones. Minimized windows keep their slot: the taskbar restores
/// them by id, and coming back where they were is what a user expects. Returns
/// `None` when `order` is not a permutation of the visible windows.
fn merge_visible_order(
    current: &[u64],
    order: Vec<u64>,
    minimized: &HashSet<u64>,
) -> Option<Vec<u64>> {
    let visible = current
        .iter()
        .copied()
        .filter(|id| return !minimized.contains(id))
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let valid = order.len() == visible.len()
        && order
            .iter()
            .all(|id| return visible.contains(id) && seen.insert(*id));
    if !valid {
        return None;
    }
    let mut ordered = order.into_iter();
    let mut merged = Vec::with_capacity(current.len());
    for id in current.iter().copied() {
        if minimized.contains(&id) {
            merged.push(id);
        } else if let Some(next) = ordered.next() {
            merged.push(next);
        }
    }
    return Some(merged);
}

/// The index in the full tiled list (including `excluded`) where a window
/// dropped at `point` should be inserted.
///
/// Windows are read in row-major order. The drop target is the nearest window;
/// the pointer's dominant normalized offset from its centre decides whether the
/// dragged window goes before or after it. This works for the fallback columns
/// and for scripted grids without knowing the layout algorithm.
fn insertion_index(
    state: &State,
    output: &smithay::output::Output,
    excluded: u64,
    point: Point<f64, Logical>,
) -> usize {
    let Some(layout) = state.core.layout.outputs.get(output) else {
        return 0;
    };
    let now = Instant::now();
    let remaining = layout
        .ws()
        .tiled
        .iter()
        .copied()
        .filter(|id| return *id != excluded)
        .collect::<Vec<_>>();
    // Minimized windows have no on-screen rectangle, so they cannot be a drop
    // target. They stay in `remaining` (that is where the window is inserted),
    // but they are never picked as the anchor.
    let others = remaining
        .iter()
        .filter_map(|id| {
            let window = state.core.layout.windows.get(id)?;
            if window.minimized {
                return None;
            }
            return Some((*id, window.rect_at(now)));
        })
        .collect::<Vec<_>>();
    if others.is_empty() {
        return remaining.len();
    }
    let mut ordered = others.clone();
    ordered.sort_by_key(|(_, rect)| {
        return (rect.loc.y + rect.size.h / 2, rect.loc.x + rect.size.w / 2);
    });

    let mut nearest = 0usize;
    let mut nearest_distance = f64::MAX;
    for (index, (_, rect)) in ordered.iter().enumerate() {
        let center: Point<f64, Logical> = Point::from((
            f64::from(rect.loc.x) + f64::from(rect.size.w) / 2.0,
            f64::from(rect.loc.y) + f64::from(rect.size.h) / 2.0,
        ));
        let dx = (point.x - center.x) / (f64::from(rect.size.w.max(1)) / 2.0);
        let dy = (point.y - center.y) / (f64::from(rect.size.h.max(1)) / 2.0);
        let distance = dx * dx + dy * dy;
        if distance < nearest_distance {
            nearest_distance = distance;
            nearest = index;
        }
    }

    let Some((_, rect)) = ordered.get(nearest) else {
        return remaining.len();
    };
    let center: Point<f64, Logical> = Point::from((
        f64::from(rect.loc.x) + f64::from(rect.size.w) / 2.0,
        f64::from(rect.loc.y) + f64::from(rect.size.h) / 2.0,
    ));
    let dx = (point.x - center.x) / (f64::from(rect.size.w.max(1)) / 2.0);
    let dy = (point.y - center.y) / (f64::from(rect.size.h.max(1)) / 2.0);
    let after = if dx.abs() >= dy.abs() {
        dx > 0.0
    } else {
        dy > 0.0
    };
    let position = nearest + usize::from(after);
    if position >= ordered.len() {
        return remaining.len();
    }
    let anchor = ordered[position].0;
    return remaining
        .iter()
        .position(|id| return *id == anchor)
        .unwrap_or(remaining.len());
}

/// Move `id` to `index` in `output`'s tiled list; returns whether it changed.
fn reorder_tiled(
    state: &mut State,
    output: &smithay::output::Output,
    id: u64,
    index: usize,
) -> bool {
    let Some(layout) = state.core.layout.outputs.get_mut(output) else {
        return false;
    };
    let Some(current) = layout
        .ws()
        .tiled
        .iter()
        .position(|entry| return *entry == id)
    else {
        return false;
    };
    let mut order = layout.ws().tiled.clone();
    order.remove(current);
    let index = index.min(order.len());
    if current == index {
        return false;
    }
    order.insert(index, id);
    layout.ws_mut().tiled = order;
    state.core.layout.dirty_space = true;
    return true;
}

fn open_output(state: &State, rule: Option<&WindowRule>) -> Option<smithay::output::Output> {
    if let Some(pattern) = rule.and_then(|rule| return rule.open_on_output.as_deref()) {
        if let Some(output) = state
            .core
            .space
            .outputs()
            .find(|output| return poly_config::glob_matches(pattern, &output.name()))
        {
            return Some(output.clone());
        }
        warn!("window rule output {pattern:?} does not match any output");
    }
    return LayoutManager::active_output(state);
}

fn match_window_rule<'a>(
    config: &'a Config,
    app_id: Option<&str>,
    title: Option<&str>,
) -> Option<&'a WindowRule> {
    return config
        .window_rules
        .iter()
        .find(|rule| return rule.matches(app_id, title));
}

/// A first guess at a window's rectangle, before the layout has run.
///
/// `map_window` takes the client's own geometry, clipped and centered on the
/// output, as a seed: it is the fallback when no layout can answer, and the
/// `rect` hint the `insert` hook sees.
fn initial_rect(
    state: &State,
    output: Option<&smithay::output::Output>,
    size: Size<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let Some(output) = output else {
        return Rectangle::new(Point::from((0, 0)), size);
    };
    let Some(area) = LayoutManager::layout_area(state, output) else {
        return Rectangle::new(Point::from((0, 0)), size);
    };
    let w = size.w.min(area.size.w).max(1);
    let h = size.h.min(area.size.h).max(1);
    return Rectangle::new(
        area.loc + Point::from(((area.size.w - w) / 2, (area.size.h - h) / 2)),
        Size::from((w, h)),
    );
}

/// Shrink `area` by `gap` on every side, never collapsing it.
///
/// Tiny outputs (or absurd gaps) shrink the inset instead of producing an
/// empty or inverted rectangle, so a layout always gets at least 1×1.
fn inset_area(area: Rectangle<i32, Logical>, gap: i32) -> Rectangle<i32, Logical> {
    let gap = gap.max(0);
    let inset = gap.min(area.size.w.saturating_sub(1) / 2);
    let inset = inset.min(area.size.h.saturating_sub(1) / 2);
    return Rectangle::new(
        area.loc + Point::from((inset, inset)),
        Size::from((
            (area.size.w - inset * 2).max(1),
            (area.size.h - inset * 2).max(1),
        )),
    );
}

fn column_layout(
    tiled: &[u64],
    fullscreen: &[u64],
    area: Rectangle<i32, Logical>,
    gap: i32,
) -> HashMap<u64, Rectangle<i32, Logical>> {
    let mut result = HashMap::new();
    let ids = tiled
        .iter()
        .copied()
        .filter(|id| return !fullscreen.contains(id))
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return result;
    }
    let count = ids.len() as i32;
    let total_gap = gap.saturating_mul(count.saturating_sub(1));
    let each = ((area.size.w - total_gap) / count).max(1);
    let mut x = area.loc.x;
    for (index, id) in ids.iter().enumerate() {
        let last = index + 1 == ids.len();
        let w = if last {
            (area.loc.x + area.size.w - gap * (count - 1 - index as i32) - x).max(1)
        } else {
            each
        };
        result.insert(
            *id,
            Rectangle::new(Point::from((x, area.loc.y)), Size::from((w, area.size.h))),
        );
        x += w + gap;
    }
    return result;
}

fn clamp_to_hints(window: &Window, rect: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    let (min, max) = size_constraints(window);
    let min_w = if min.0 > 0 { min.0 } else { 1 };
    let min_h = if min.1 > 0 { min.1 } else { 1 };
    let mut w = rect.size.w.max(min_w);
    let mut h = rect.size.h.max(min_h);
    if max.0 > 0 {
        w = w.min(max.0);
    }
    if max.1 > 0 {
        h = h.min(max.1);
    }
    return Rectangle::new(rect.loc, Size::from((w.max(1), h.max(1))));
}

fn size_constraints(window: &Window) -> ((i32, i32), (i32, i32)) {
    let Some(surface) = window.wl_surface() else {
        return ((0, 0), (0, 0));
    };
    return with_states(&surface, |states| {
        let mut guard = states.cached_state.get::<SurfaceCachedState>();
        let state = guard.current();
        return (
            (state.min_size.w, state.min_size.h),
            (state.max_size.w, state.max_size.h),
        );
    });
}

/// Whether a window should open floating when no rule says otherwise.
///
/// Mirrors niri/Hyprland: transient windows (dialogs, usually with a parent)
/// float, and so do windows that mark themselves as non-resizable by setting
/// `min_size == max_size`. In xdg-shell a zero size means "no constraint", so
/// only axes with a positive minimum count. Hyprland floats a window fixed in
/// either axis, niri the fixed-height ones; both are covered here.
fn opens_floating(window: &Window) -> bool {
    let Some(toplevel) = window.toplevel() else {
        return false;
    };
    if toplevel.parent().is_some() {
        return true;
    }
    let (min, max) = size_constraints(window);
    return hints_lock_size(min, max);
}

/// Whether xdg-shell size hints mark the window as non-resizable.
///
/// `(0, 0)` disables a constraint, so only positive minima count. A window is
/// locked when *either* axis has `min == max` (Hyprland's rule); niri checks
/// the height only, which is the common case for panels and dialogs.
fn hints_lock_size(min: (i32, i32), max: (i32, i32)) -> bool {
    return min.0 > 0 && min.1 > 0 && (min.0 == max.0 || min.1 == max.1);
}

fn to_layout_rect(rect: Rectangle<i32, Logical>) -> LayoutRect {
    return LayoutRect {
        x: rect.loc.x,
        y: rect.loc.y,
        w: rect.size.w,
        h: rect.size.h,
    };
}

fn from_layout_rect(rect: LayoutRect) -> Rectangle<i32, Logical> {
    return Rectangle::new(
        Point::from((rect.x, rect.y)),
        Size::from((rect.w.max(1), rect.h.max(1))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        return Rectangle::new(Point::from((x, y)), Size::from((w, h)));
    }

    #[test]
    fn fixed_size_hints_open_floating() {
        // Fully fixed (dialogs, panels): both axes locked.
        assert!(hints_lock_size((400, 300), (400, 300)));
        // Locked in one axis only: Hyprland floats these, niri the height one.
        assert!(hints_lock_size((400, 300), (400, 0)));
        assert!(hints_lock_size((400, 300), (0, 300)));
        // Resizable windows carry no maximum (0 = unconstrained).
        assert!(!hints_lock_size((400, 300), (0, 0)));
        assert!(!hints_lock_size((0, 0), (400, 300)));
        // A zero minimum means "no constraint", even if the maximum is set.
        assert!(!hints_lock_size((0, 300), (0, 300)));
    }

    #[test]
    fn gap_insets_every_side() {
        let inset = inset_area(area(0, 0, 1000, 600), 8);
        assert_eq!(inset, area(8, 8, 984, 584));
    }

    #[test]
    fn gap_insets_inside_a_bar_reserved_area() {
        // Work area already excludes a 30px bar; the inset stacks on top.
        let inset = inset_area(area(0, 30, 1000, 570), 8);
        assert_eq!(inset, area(8, 38, 984, 554));
    }

    #[test]
    fn gap_never_collapses_the_area() {
        assert_eq!(inset_area(area(10, 20, 4, 4), 8), area(11, 21, 2, 2));
        assert_eq!(inset_area(area(0, 0, 1, 1), 8), area(0, 0, 1, 1));
        assert_eq!(inset_area(area(0, 0, 1000, 600), 0), area(0, 0, 1000, 600));
    }

    #[test]
    fn gap_zero_when_negative() {
        assert_eq!(inset_area(area(0, 0, 100, 100), -5), area(0, 0, 100, 100));
    }

    #[test]
    fn columns_split_the_inset_area() {
        let inset = inset_area(area(0, 0, 1920, 1080), 8);
        let windows = column_layout(&[1, 2], &[], inset, 8);
        assert_eq!(windows.get(&1), Some(&area(8, 8, 948, 1064)));
        assert_eq!(windows.get(&2), Some(&area(964, 8, 948, 1064)));
    }

    #[test]
    fn closing_a_window_focuses_its_layout_neighbour() {
        // The tiled list is layout order, not mapping order. A window in the
        // middle hands focus to its left neighbour, which is the column the
        // user came from, not the last window in the workspace.
        let tiled = [10, 11, 12];
        assert_eq!(focus_candidates(&tiled, &[], 11), vec![10, 12]);
        // The first window has no left neighbour, so focus goes right.
        assert_eq!(focus_candidates(&tiled, &[], 10), vec![11]);
        // The last one goes left.
        assert_eq!(focus_candidates(&tiled, &[], 12), vec![11]);
    }

    #[test]
    fn closing_a_window_prefers_its_own_list() {
        let tiled = [10, 11];
        let floating = [20, 21];
        // A floating window hands focus to the floating neighbour first, then
        // falls back to the topmost tiled window.
        assert_eq!(focus_candidates(&tiled, &floating, 20), vec![21, 11]);
        // A tiled window falls back to the topmost floating window only after
        // its tiled neighbours are exhausted.
        assert_eq!(focus_candidates(&tiled, &floating, 11), vec![10, 21]);
        // A lone tiled window hands focus to the topmost floating one.
        assert_eq!(focus_candidates(&[10], &floating, 10), vec![21]);
    }

    #[test]
    fn a_lone_window_has_no_focus_candidate() {
        assert!(focus_candidates(&[10], &[], 10).is_empty());
        assert!(focus_candidates(&[], &[], 10).is_empty());
    }

    #[test]
    fn a_layout_order_keeps_minimized_windows_in_their_slot() {
        // Windows 10 and 12 are minimized, so the layout only sees 11 and 13.
        let minimized = HashSet::from([10, 12]);
        let merged = merge_visible_order(&[10, 11, 12, 13], vec![13, 11], &minimized);
        assert_eq!(merged, Some(vec![10, 13, 12, 11]));
        // Nothing is minimized: the order is used as it is.
        assert_eq!(
            merge_visible_order(&[10, 11], vec![11, 10], &HashSet::new()),
            Some(vec![11, 10])
        );
        // Everything is minimized: the list is unchanged.
        assert_eq!(
            merge_visible_order(&[10], vec![], &HashSet::from([10])),
            Some(vec![10])
        );
        // The layout may not invent, drop or repeat a window.
        assert_eq!(
            merge_visible_order(&[10, 11], vec![11], &HashSet::new()),
            None
        );
        assert_eq!(
            merge_visible_order(&[10, 11], vec![11, 11], &HashSet::new()),
            None
        );
        assert_eq!(
            merge_visible_order(&[10, 11], vec![11, 12], &HashSet::new()),
            None
        );
        // Minimized windows are not part of the order the layout may return.
        assert_eq!(
            merge_visible_order(&[10, 11], vec![10, 11], &HashSet::from([10])),
            None
        );
    }

    #[test]
    fn columns_reflow_over_a_minimized_window() {
        // `workspace_context` hands the layout only the visible windows, so a
        // minimized column simply disappears and the others share its width.
        let inset = inset_area(area(0, 0, 1920, 1080), 8);
        let windows = column_layout(&[1, 3], &[], inset, 8);
        assert_eq!(windows.get(&1), Some(&area(8, 8, 948, 1064)));
        assert_eq!(windows.get(&3), Some(&area(964, 8, 948, 1064)));
        assert_eq!(windows.get(&2), None);
    }

    #[test]
    fn plain_view_projects_the_active_workspace_unchanged() {
        // Every non-active card sits exactly one output away, so projecting
        // the active workspace is the identity and the others are off screen.
        let output = area(0, 0, 1000, 600);
        let view = WorkspaceView {
            origin: output.loc,
            cards: workspace_cards(output, 3, 1.0),
        };
        let local = area(40, 50, 300, 200);
        assert_eq!(view.project(1, local), Some(local));
        assert_eq!(view.project(0, local), Some(area(40, -550, 300, 200)));
        assert_eq!(view.project(2, local), Some(area(40, 650, 300, 200)));
        // A workspace without a card cannot be projected.
        assert_eq!(view.project(9, local), None);
    }

    #[test]
    fn a_slide_moves_whole_screens_without_a_gap() {
        let output = area(100, 0, 1000, 600);
        let cards = workspace_cards(output, 2, 0.5);
        assert_eq!(cards[0].rect, area(100, -300, 1000, 600));
        assert_eq!(cards[1].rect, area(100, 300, 1000, 600));
    }

    fn resize(anchor: ResizeAnchor, origin: (i32, i32)) -> InteractiveResize {
        return InteractiveResize {
            anchor,
            origin: Point::from(origin),
            desired: Size::from((200, 100)),
            committed: Size::from((150, 80)),
            drawn: Size::from((150, 80)),
            sent: None,
            end_since: None,
        };
    }

    #[test]
    fn resize_anchors_the_edges_the_pointer_does_not_move() {
        let bottom_right = resize(
            ResizeAnchor {
                fixed_right: true,
                fixed_bottom: true,
            },
            (600, 500),
        );
        // Dragging the left/top edges grows the window towards the top left;
        // the committed size keeps the bottom-right corner on screen.
        assert_eq!(
            bottom_right.rect_for(bottom_right.committed),
            area(450, 420, 150, 80)
        );
        assert_eq!(
            bottom_right.rect_for(bottom_right.size()),
            area(400, 400, 200, 100)
        );

        let top_left = resize(
            ResizeAnchor {
                fixed_right: false,
                fixed_bottom: false,
            },
            (100, 50),
        );
        assert_eq!(
            top_left.rect_for(top_left.committed),
            area(100, 50, 150, 80)
        );
        assert_eq!(top_left.rect_for(top_left.size()), area(100, 50, 200, 100));
    }

    #[test]
    fn resize_throttle_waits_for_the_acknowledged_size() {
        let mut resize = resize(
            ResizeAnchor {
                fixed_right: false,
                fixed_bottom: false,
            },
            (0, 0),
        );
        // No configure went out yet: the next size may be sent immediately.
        assert!(resize.caught_up());
        resize.sent = Some(Size::from((300, 200)));
        // The client still draws the previous size: a new configure would only
        // queue work it cannot keep up with.
        resize.committed = Size::from((250, 200));
        assert!(!resize.caught_up());
        resize.committed = Size::from((300, 200));
        assert!(resize.caught_up());
    }

    #[test]
    fn resize_size_never_collapses_to_zero() {
        let mut resize = resize(
            ResizeAnchor {
                fixed_right: false,
                fixed_bottom: false,
            },
            (0, 0),
        );
        resize.desired = Size::from((0, 0));
        assert_eq!(resize.size(), Size::from((1, 1)));
    }
}
