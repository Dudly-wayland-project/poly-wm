// Ported from niri (GPL-3.0-or-later): src/niri.rs (state layout and redraw plumbing)
// Ported from smallvil (MIT): src/state.rs

//! The backend-agnostic compositor core.
//!
//! [`Core`] owns everything that is not tied to how frames reach the screen:
//! the space of windows, the wayland protocol state, the seat, per-output
//! redraw scheduling and the software cursor. Backends render it and feed it
//! input.

pub mod actions;
pub mod capture;
pub mod config;
pub mod cursor;
pub mod display;
pub mod focus;
pub mod handlers;
pub mod input;
pub mod ipc;
pub mod layout;
pub mod notify;
pub mod output;
pub mod popups;
pub mod redraw;
pub mod render;
pub mod ui;
pub mod window;

use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use poly_config::{Config, ConfigSource, Keybind, LayoutPrograms, ModKey};
use smithay::backend::renderer::element::solid::SolidColorBuffer;
use smithay::desktop::{
    PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output,
    utils::under_from_surface_tree,
};
use smithay::input::Seat;
use smithay::input::SeatState;
use smithay::input::keyboard::XkbConfig;
use smithay::input::pointer::CursorImageStatus;
use smithay::output::Output;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, LoopHandle, LoopSignal, RegistrationToken};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{ClockSource, Logical, Monotonic, Point};
use smithay::wayland::alpha_modifier::AlphaModifierState;
use smithay::wayland::background_effect::BackgroundEffectState;
use smithay::wayland::commit_timing::CommitTimingManagerState;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::content_type::ContentTypeState;
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::fifo::FifoManagerState;
use smithay::wayland::foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelListState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::idle_notify::IdleNotifierState;
use smithay::wayland::image_capture_source::{ImageCaptureSourceState, OutputCaptureSourceState};
use smithay::wayland::image_copy_capture::{CursorSession, ImageCopyCaptureState, Session};
use smithay::wayland::input_method::InputMethodManagerState;
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::pointer_gestures::PointerGesturesState;
use smithay::wayland::pointer_warp::PointerWarpManager;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::ext_data_control::DataControlState as ExtDataControlState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::selection::wlr_data_control::DataControlState as WlrDataControlState;
use smithay::wayland::session_lock::{LockSurface, SessionLockManagerState, SessionLocker};
use smithay::wayland::shell::kde::decoration::KdeDecorationState;
use smithay::wayland::shell::wlr_layer::{Layer, WlrLayerShellState};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::tablet_manager::TabletManagerState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xdg_foreign::XdgForeignState;
use smithay::wayland::xdg_system_bell::XdgSystemBellState;
use smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager;
use smithay::wayland::xdg_toplevel_tag::XdgToplevelTagManager;
use tracing::warn;
use xcursor::parser::Image;

use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode;
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;

use crate::core::cursor::{Cursor, CursorElement};
use crate::core::input::scroll_tracker::ScrollTracker;
use crate::core::redraw::OutputState;
use crate::core::render::CardShadowCache;
use crate::core::window::UnmappedWindow;
use crate::state::State;

/// Everything the compositor does that is independent of the backend.
pub struct Core {
    /// Handle to the wayland display, for creating globals and flushing clients.
    pub display_handle: DisplayHandle,
    /// Stopping this signal makes the event loop return.
    pub stop_signal: LoopSignal,
    /// Start of the compositor process, used as the shader/cursor clock.
    pub start_time: Instant,
    /// `CLOCK_MONOTONIC` at [`Core::start_time`], so protocol timestamps
    /// (frame callbacks, presentation) can be reported on the correct epoch.
    pub monotonic_start: Duration,

    /// Every mapped window.
    pub space: Space<Window>,
    /// Popup tracking (positioning, grabs, cleanup).
    pub popups: PopupManager,
    /// Per-output compositor state, keyed by the output itself.
    pub output_state: HashMap<Output, OutputState>,
    /// Windows that have no buffer committed yet, keyed by their root surface.
    pub unmapped_windows: HashMap<WlSurface, UnmappedWindow>,

    /// The wayland seat.
    pub seat: Seat<State>,
    /// Seat bookkeeping, needed by the `SeatHandler` implementation.
    pub seat_state: SeatState<State>,

    /// The loaded configuration.
    pub config: Config,
    /// Mapped-window state, per-output tiling and the scripted layout bridge.
    pub layout: crate::core::layout::LayoutManager,
    /// Where the configuration came from (`reload-config`, the file watcher).
    pub config_source: ConfigSource,
    /// Every file the configuration loaded, for the reload watcher.
    pub config_includes: Vec<PathBuf>,
    /// The mod key when the configuration does not pick one: Super on a real
    /// session, Alt when nested inside another compositor (winit).
    pub default_mod_key: ModKey,
    /// The mod key in effect (configuration value, or the backend default).
    pub mod_key: ModKey,

    /// Pointer buttons whose press fired a binding: their release is swallowed
    /// too, so the client never sees half a click.
    pub suppressed_buttons: HashSet<u32>,
    /// Set by Escape while a move/resize grab runs: the grab's `unset` puts the
    /// window back.
    pub grab_cancelled: bool,
    /// The running binding repeat, if any.
    pub bind_repeat_timer: Option<RegistrationToken>,
    /// The event loop, for arming the binding repeat timer.
    pub event_loop_handle: LoopHandle<'static, State>,
    /// The IPC server: the socket, the connected clients and the event-stream
    /// baseline.
    pub ipc: crate::core::ipc::IpcServer,
    /// Accumulates high-resolution wheel and touchpad scrolling.
    pub scroll_tracker_horizontal: ScrollTracker,
    /// Accumulates high-resolution wheel and touchpad scrolling.
    pub scroll_tracker_vertical: ScrollTracker,

    /// Cursor theme.
    pub cursor: Cursor,
    /// Cursor as the currently focused client wants it.
    pub cursor_status: CursorImageStatus,
    /// Software cursor render element.
    pub cursor_element: CursorElement,
    /// Cursor images already turned into render buffers.
    pub pointer_images: Vec<(
        Image,
        smithay::backend::renderer::element::memory::MemoryRenderBuffer,
    )>,

    /// `wl_compositor`.
    pub compositor_state: CompositorState,
    /// `xdg_shell`.
    pub xdg_shell_state: XdgShellState,
    /// `wl_shm`.
    pub shm_state: ShmState,
    /// `wl_output` / `zxdg_output_manager_v1`.
    ///
    /// This state and the two below only exist to keep the protocol globals
    /// they created (in [`Core::new`]) alive; nothing reads them yet.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    /// `wl_data_device`.
    pub data_device_state: DataDeviceState,
    /// `zwp_primary_selection_device_v1`.
    pub primary_selection_state: PrimarySelectionState,
    /// `zwlr_data_control_manager_v1` (clipboard managers).
    #[allow(dead_code)]
    pub wlr_data_control_state: WlrDataControlState,
    /// `ext_data_control_manager_v1` (clipboard managers).
    #[allow(dead_code)]
    pub ext_data_control_state: ExtDataControlState,
    /// `zwp_linux_dmabuf_v1` (the global itself is created by the backend, which
    /// owns the render node the feedback refers to).
    pub dmabuf_state: DmabufState,
    /// `wp_viewporter`.
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    /// `wp_presentation`.
    #[allow(dead_code)]
    pub presentation_state: PresentationState,

    /// `wp_single_pixel_buffer_v1`.
    #[allow(dead_code)]
    pub single_pixel_buffer_state: SinglePixelBufferState,
    /// `wp_fractional_scale_v1`.
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,
    /// `wp_alpha_modifier_v1`.
    #[allow(dead_code)]
    pub alpha_modifier_state: AlphaModifierState,
    /// `ext_background_effect_manager_v1`.
    ///
    /// Only the global itself is kept here: a client's blur region lives in the
    /// double-buffered `BackgroundEffectSurfaceCachedState` on its surface, and
    /// the renderer reads it from there (`render::push_window`).
    #[allow(dead_code)]
    pub background_effect_state: BackgroundEffectState,
    /// `wp_content_type_v1`.
    #[allow(dead_code)]
    pub content_type_state: ContentTypeState,
    /// `wp_fifo_v1` (frame synchronization hints).
    #[allow(dead_code)]
    pub fifo_state: FifoManagerState,
    /// `wp_commit_timing_v1`.
    #[allow(dead_code)]
    pub commit_timing_state: CommitTimingManagerState,
    /// `zwp_tablet_manager_v2`.
    #[allow(dead_code)]
    pub tablet_state: TabletManagerState,
    /// `wp_cursor_shape_v1`.
    #[allow(dead_code)]
    pub cursor_shape_state: CursorShapeManagerState,
    /// `zwp_relative_pointer_v1`.
    #[allow(dead_code)]
    pub relative_pointer_state: RelativePointerManagerState,
    /// `zwp_pointer_gestures_v1`.
    #[allow(dead_code)]
    pub pointer_gestures_state: PointerGesturesState,
    /// `wp_pointer_warp_v1`.
    #[allow(dead_code)]
    pub pointer_warp_state: PointerWarpManager,
    /// `zwp_keyboard_shortcuts_inhibit_v1`.
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    /// `xdg_activation_v1` (focus stealing prevention tokens).
    pub xdg_activation_state: XdgActivationState,
    /// `zxdg_exporter_v2` / `zxdg_importer_v2`.
    #[allow(dead_code)]
    pub xdg_foreign_state: XdgForeignState,
    /// `ext_foreign_toplevel_list_v1` (taskbars).
    pub foreign_toplevel_list_state: ForeignToplevelListState,
    /// `xdg_system_bell_v1`.
    #[allow(dead_code)]
    pub xdg_system_bell_state: XdgSystemBellState,
    /// `xdg_toplevel_icon_manager_v1`.
    #[allow(dead_code)]
    pub xdg_toplevel_icon_state: XdgToplevelIconManager,
    /// `xdg_toplevel_tag_manager_v1` (toplevel tags and descriptions).
    #[allow(dead_code)]
    pub xdg_toplevel_tag_state: XdgToplevelTagManager,
    /// `zxdg_decoration_manager_v1`.
    ///
    /// Clients can only see this global while `prefer_no_csd` is on: then we
    /// answer `ServerSide` and they drop their own decorations (AGENTS.md
    /// §2.10). With `prefer_no_csd` off they fall back to client-side
    /// decorations, which is what users of that mode expect.
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    /// `org_kde_kwin_server_decoration_manager`.
    #[allow(dead_code)]
    pub kde_decoration_state: KdeDecorationState,
    /// `ext_idle_notifier_v1`.
    pub idle_notifier_state: IdleNotifierState<State>,
    /// `zwp_idle_inhibit_manager_v1`.
    #[allow(dead_code)]
    pub idle_inhibit_state: IdleInhibitManagerState,
    /// `ext_session_lock_manager_v1`.
    pub session_lock_state: SessionLockManagerState,
    /// `zwp_text_input_manager_v3`.
    #[allow(dead_code)]
    pub text_input_state: TextInputManagerState,
    /// `zwp_input_method_manager_v2`.
    #[allow(dead_code)]
    pub input_method_state: InputMethodManagerState,
    /// `zwp_virtual_keyboard_manager_v1`.
    ///
    /// Not strictly required by the protocol set, but fcitx5's Wayland
    /// input-method frontend refuses to initialize without it, so it is
    /// effectively mandatory for a working IME.
    #[allow(dead_code)]
    pub virtual_keyboard_state: VirtualKeyboardManagerState,
    /// `zwlr_layer_shell_v1` (status bars, notifications, launchers).
    pub layer_shell_state: WlrLayerShellState,
    /// `ext-image-capture-source-v1` (the source objects themselves).
    #[allow(dead_code)]
    pub image_capture_source_state: ImageCaptureSourceState,
    /// `ext-output-image-capture-source-v1` (capture sources made from outputs).
    #[allow(dead_code)]
    pub output_capture_source_state: OutputCaptureSourceState,
    /// `ext-image-copy-capture-v1` (screenshot and screencopy clients).
    pub image_copy_capture_state: ImageCopyCaptureState,
    /// Live image-copy sessions.
    ///
    /// Smithay stops a session when its [`Session`] is dropped, so every one
    /// handed to us in `new_session` is kept here until `session_destroyed`
    /// removes it.
    pub image_copy_sessions: Vec<Session>,
    /// Live pointer-cursor capture sessions
    /// (`ext_image_copy_capture_manager_v1.create_pointer_cursor_session`).
    ///
    /// Same ownership rule as the sessions above: dropping the value stops the
    /// session, so it lives here until the client destroys the object.
    pub cursor_sessions: Vec<CursorSession>,

    /// Surfaces holding a keyboard-shortcuts inhibitor: while one of them has
    /// focus, bindings are not dispatched (`zwp_keyboard_shortcuts_inhibit_v1`).
    pub keyboard_shortcuts_inhibitors: HashMap<WlSurface, KeyboardShortcutsInhibitor>,
    /// Surfaces currently inhibiting idle.
    pub idle_inhibiting_surfaces: HashSet<WlSurface>,
    /// Layer surfaces that are not showing a buffer right now, by root surface.
    pub unmapped_layer_surfaces: HashSet<WlSurface>,
    /// The layer surface currently holding on-demand keyboard focus, if any.
    pub layer_focus: Option<WlSurface>,
    /// Foreign-toplevel handles handed to taskbars, keyed by root surface.
    pub foreign_toplevels: HashMap<WlSurface, ForeignToplevelHandle>,
    /// The running popup grab, if any.
    pub popup_grab: Option<crate::core::popups::PopupGrabState>,
    /// The root surface of the window that last held keyboard focus, so that
    /// focus can be returned to it after a launcher closes.
    pub last_window_focus: Option<WlSurface>,
    /// The output the configuration marks with `focus = true`, used as the
    /// focused output until a pointer motion or a window on another output
    /// claims focus (`docs/config.md`).
    pub preferred_output: Option<Output>,
    /// Set when an output appeared, went away or changed its modes while the
    /// backend held a `&mut Core` instead of the whole `State` (a DRM hotplug
    /// arrives inside a device event). The event loop applies the `outputs`
    /// section on the next turn, before the frame is rendered.
    pub output_config_pending: bool,
    /// The root configuration file and its include closure, watched for edits
    /// so that saving the file reloads it.
    pub config_watcher: crate::utils::watcher::ConfigWatcher,
    /// The XWayland integration, when `xwayland.enable` is on: the X11 sockets
    /// and the watch that starts `xwayland-satellite` on demand.
    pub satellite: Option<crate::utils::xwayland::satellite::Satellite>,
    /// The session-lock state machine (ext-session-lock).
    pub lock_state: LockState,
    /// Lock surfaces per output while the session is locked.
    pub lock_surfaces: Vec<(Output, LockSurface)>,
    /// Outputs that presented a locked frame since the lock request. The lock
    /// is only confirmed once every output is in here (the `locked` event must
    /// not be sent before a locked frame replaced the session content).
    pub lock_rendered_outputs: HashSet<Output>,
    /// The solid background drawn below lock surfaces, per output.
    pub lock_backgrounds: HashMap<Output, SolidColorBuffer>,
    /// The black backdrop behind compositor-managed fullscreen windows.
    pub fullscreen_backgrounds: HashMap<Output, SolidColorBuffer>,
    /// Cached solid buffers for the tiled drag drop indicator.
    pub drag_hint_buffers: Option<DragHintBuffers>,
    /// Cached solid buffers for the workspace overview, per output.
    pub overview_buffers: HashMap<Output, OverviewBuffers>,

    /// Compiled window, border, shadow and blur programs.
    pub shaders: crate::core::render::Shaders,
    /// Per-window border/shadow/blur element caches.
    pub decorations: crate::core::render::Decorations,
    /// On-screen messages for shader, config and layout errors.
    pub toasts: crate::core::ui::Toasts,
    /// Keep redrawing: a shader reads `u_time`, so every frame differs.
    pub continuous_redraw: bool,
    /// Bumped on every configuration reload, so shaders recompile lazily.
    pub config_generation: u64,
}

/// Solid buffers for the drag-and-drop indicator: a translucent slot without a
/// border, so it reads as a shadow of the window that will land there.
pub struct DragHintBuffers {
    fill: SolidColorBuffer,
}

impl DragHintBuffers {
    /// Allocate the buffer at unit size; `render.rs` resizes it.
    pub fn new() -> Self {
        let clear = [0.0f32, 0.0, 0.0, 0.0];
        return Self {
            fill: SolidColorBuffer::new((1, 1), clear),
        };
    }

    /// The translucent area.
    pub fn fill_mut(&mut self) -> &mut SolidColorBuffer {
        return &mut self.fill;
    }
}

/// Solid buffers backing the workspace overview: one card per workspace and
/// the dark backdrop below the whole stack.
pub struct OverviewBuffers {
    cards: Vec<SolidColorBuffer>,
    backdrop: SolidColorBuffer,
    shadows: Vec<Option<CardShadowCache>>,
}

impl OverviewBuffers {
    /// Allocate the buffers at unit size; `render.rs` resizes them.
    pub fn new() -> Self {
        let clear = [0.0f32, 0.0, 0.0, 0.0];
        return Self {
            cards: Vec::new(),
            backdrop: SolidColorBuffer::new((1, 1), clear),
            shadows: Vec::new(),
        };
    }

    /// The card background of workspace `index`, allocated on first use.
    pub fn card_mut(&mut self, index: usize) -> &mut SolidColorBuffer {
        while self.cards.len() <= index {
            self.cards.push(SolidColorBuffer::new((1, 1), [0.0; 4]));
        }
        return &mut self.cards[index];
    }

    /// The cached shadow of workspace `index`, allocated on first use.
    ///
    /// The cache is what keeps an idle overview from repainting every card:
    /// a fresh `PixelShaderElement` would get a new element id on every frame.
    pub fn shadow_mut(&mut self, index: usize) -> &mut Option<CardShadowCache> {
        while self.shadows.len() <= index {
            self.shadows.push(None);
        }
        return &mut self.shadows[index];
    }

    /// The backdrop below every card.
    pub fn backdrop_mut(&mut self) -> &mut SolidColorBuffer {
        return &mut self.backdrop;
    }
}

/// Where the session is in the ext-session-lock handshake.
#[derive(Debug, Default)]
pub enum LockState {
    /// Nobody is locking.
    #[default]
    Unlocked,
    /// A client asked to lock; we are waiting for its lock surfaces.
    WaitingForSurfaces(SessionLocker),
    /// The session is locked; the lock object identifies the owning client.
    Locked(ExtSessionLockV1),
}

impl LockState {
    /// Whether output contents must be replaced by lock surfaces.
    pub fn is_locked(&self) -> bool {
        return !matches!(self, LockState::Unlocked);
    }
}

impl Core {
    /// Whether the session is locked (lock surfaces must replace everything).
    pub fn is_locked(&self) -> bool {
        return self.lock_state.is_locked();
    }

    /// The lock surface drawn on an output while the session is locked.
    pub fn lock_surface_for_output(&self, output: &Output) -> Option<&LockSurface> {
        return self
            .lock_surfaces
            .iter()
            .find(|(lock_output, _)| return lock_output == output)
            .map(|(_, surface)| return surface);
    }

    /// Set up all protocol state and the seat.
    pub fn new(
        event_loop: &EventLoop<'static, State>,
        display_handle: DisplayHandle,
        config: Config,
        layout_programs: Option<LayoutPrograms>,
        config_source: ConfigSource,
        default_mod_key: ModKey,
    ) -> anyhow::Result<Self> {
        let compositor_state = CompositorState::new::<State>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<State>(&display_handle);
        let shm_state = ShmState::new::<State>(&display_handle, Vec::new());
        let output_manager_state =
            OutputManagerState::new_with_xdg_output::<State>(&display_handle);
        let data_device_state = DataDeviceState::new::<State>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<State>(&display_handle);
        let wlr_data_control_state = WlrDataControlState::new::<State, _>(
            &display_handle,
            Some(&primary_selection_state),
            |_client| true,
        );
        let ext_data_control_state = ExtDataControlState::new::<State, _>(
            &display_handle,
            Some(&primary_selection_state),
            |_client| true,
        );
        let dmabuf_state = DmabufState::new();
        let viewporter_state = ViewporterState::new::<State>(&display_handle);
        let presentation_state =
            PresentationState::new::<State>(&display_handle, Monotonic::ID as u32);

        // Protocol globals. Most of these need no logic from Poly: Smithay
        // wires them into the surface state that renderers and windows consult.
        let single_pixel_buffer_state = SinglePixelBufferState::new::<State>(&display_handle);
        let fractional_scale_state = FractionalScaleManagerState::new::<State>(&display_handle);
        let alpha_modifier_state = AlphaModifierState::new::<State>(&display_handle);
        // Clients blur their own background through this (`foot`, `kitty`).
        // The protocol layer lives in Smithay; what the region means for
        // rendering is ours (`handlers/background_effect.rs`).
        let background_effect_state = BackgroundEffectState::new::<State>(&display_handle);
        let content_type_state = ContentTypeState::new::<State>(&display_handle);
        let fifo_state = FifoManagerState::new::<State>(&display_handle);
        let commit_timing_state = CommitTimingManagerState::new::<State>(&display_handle);
        let tablet_state = TabletManagerState::new::<State>(&display_handle);
        let cursor_shape_state = CursorShapeManagerState::new::<State>(&display_handle);
        let relative_pointer_state = RelativePointerManagerState::new::<State>(&display_handle);
        let pointer_gestures_state = PointerGesturesState::new::<State>(&display_handle);
        let pointer_warp_state = PointerWarpManager::new::<State>(&display_handle);
        let keyboard_shortcuts_inhibit_state =
            KeyboardShortcutsInhibitState::new::<State>(&display_handle);
        let xdg_activation_state = XdgActivationState::new::<State>(&display_handle);
        let xdg_foreign_state = XdgForeignState::new::<State>(&display_handle);
        let foreign_toplevel_list_state = ForeignToplevelListState::new::<State>(&display_handle);
        let xdg_system_bell_state = XdgSystemBellState::new::<State>(&display_handle);
        let mut xdg_toplevel_icon_state = XdgToplevelIconManager::new::<State>(&display_handle);
        for size in [16, 24, 32, 48, 64, 128] {
            xdg_toplevel_icon_state.add_icon_size(size);
        }
        let xdg_toplevel_tag_state = XdgToplevelTagManager::new::<State>(&display_handle);

        // When `prefer_no_csd` is on, clients see the decoration globals and we
        // answer `ServerSide`: they drop their decorations, Poly draws its own
        // (rounded corners, border, shadow). With it off, the globals are
        // hidden and clients fall back to client-side decorations.
        let prefer_no_csd = config.general.prefer_no_csd;
        let xdg_decoration_state =
            XdgDecorationState::new_with_filter::<State, _>(&display_handle, move |_client| {
                prefer_no_csd
            });
        let kde_decoration_state = KdeDecorationState::new_with_filter::<State, _>(
            &display_handle,
            KdeDefaultMode::Server,
            move |_client| prefer_no_csd,
        );

        let idle_notifier_state = IdleNotifierState::new(&display_handle, event_loop.handle());
        let idle_inhibit_state = IdleInhibitManagerState::new::<State>(&display_handle);
        let session_lock_state =
            SessionLockManagerState::new::<State, _>(&display_handle, |_client| true);
        let text_input_state = TextInputManagerState::new::<State>(&display_handle);
        let input_method_state =
            InputMethodManagerState::new::<State, _>(&display_handle, |_client| true);
        let virtual_keyboard_state =
            VirtualKeyboardManagerState::new::<State, _>(&display_handle, |_client| true);
        let layer_shell_state = WlrLayerShellState::new::<State>(&display_handle);

        // Screenshots (`grim`, `wl-mirror`, ...). The two source managers and
        // the copy manager are one feature: a client that finds only one half
        // refuses to capture.
        let image_capture_source_state = ImageCaptureSourceState::new();
        let output_capture_source_state = OutputCaptureSourceState::new::<State>(&display_handle);
        let image_copy_capture_state = ImageCopyCaptureState::new::<State>(&display_handle);
        // `zwlr_screencopy_manager_v1` is not part of Smithay's protocol set, so
        // its global is created by hand; it shares the rendering path with the
        // `ext` pair above (`handlers/screencopy.rs`).
        crate::core::handlers::screencopy::create_global(&display_handle);

        let mut seat_state = SeatState::new();
        // `new_wl_seat` also announces the `wl_seat` global; `new_seat` would
        // leave clients unable to get any input at all.
        let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
        let keyboard = &config.input.keyboard;
        seat.add_keyboard(
            XkbConfig {
                rules: &keyboard.rules,
                model: &keyboard.model,
                layout: &keyboard.layout,
                variant: &keyboard.variant,
                options: keyboard.options.clone(),
            },
            i32::from(keyboard.repeat_delay),
            i32::from(keyboard.repeat_rate),
        )
        .context("error adding a keyboard to the seat")?;
        seat.add_pointer();

        // Children see the configured environment from their first spawn.
        crate::core::config::apply_child_environment(&config);

        let mod_key = config.general.mod_key.unwrap_or(default_mod_key);
        let mut config = config;
        if config.window_drag.axis_lock == Some(mod_key) {
            warn!("window_drag.axis_lock uses the compositor modifier; axis lock is disabled");
            config.window_drag.axis_lock = None;
        }
        let layout = crate::core::layout::LayoutManager::new(&config, layout_programs);

        // Shader files and the configuration can change under us.
        // Recompiling needs the GL context, which
        // only exists while rendering, so the timer only stats the shader files
        // and sets a flag `Shaders::prepare` picks up on the next frame; the
        // configuration is small enough to reload right here.
        let timer = Timer::from_duration(Duration::from_millis(500));
        // The timer never has to be removed: it lives as long as the event
        // loop, i.e. as long as the compositor.
        event_loop
            .handle()
            .insert_source(timer, |_, _, state: &mut State| {
                state.poll_shader_files();
                crate::core::config::poll_watch(state);
                return TimeoutAction::ToDuration(Duration::from_millis(500));
            })
            .map_err(|err| {
                return anyhow::anyhow!("error arming the housekeeping timer: {err:?}");
            })?;

        // The pointer theme comes from the configuration, which was moved into
        // `Self` below; loading it first keeps `Cursor::load` free of the
        // partially built state.
        let cursor = Cursor::load(&config.general.cursor);

        // Both readings are taken back to back so a `CLOCK_MONOTONIC` timestamp
        // can be mapped onto the `Instant` timeline without a visible offset.
        let monotonic_start: Duration = smithay::utils::Clock::<smithay::utils::Monotonic>::new()
            .now()
            .into();
        let start_time = Instant::now();

        return Ok(Self {
            display_handle,
            stop_signal: event_loop.get_signal(),
            start_time,
            monotonic_start,
            space: Space::default(),
            popups: PopupManager::default(),
            output_state: HashMap::new(),
            unmapped_windows: HashMap::new(),
            seat,
            seat_state,
            config,
            layout,
            config_source,
            config_includes: Vec::new(),
            default_mod_key,
            mod_key,
            suppressed_buttons: HashSet::new(),
            grab_cancelled: false,
            bind_repeat_timer: None,
            event_loop_handle: event_loop.handle(),
            ipc: crate::core::ipc::IpcServer::default(),
            scroll_tracker_horizontal: ScrollTracker::new(120),
            scroll_tracker_vertical: ScrollTracker::new(120),
            cursor,
            cursor_status: CursorImageStatus::default_named(),
            cursor_element: CursorElement::default(),
            pointer_images: Vec::new(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            data_device_state,
            primary_selection_state,
            wlr_data_control_state,
            ext_data_control_state,
            dmabuf_state,
            viewporter_state,
            presentation_state,
            single_pixel_buffer_state,
            fractional_scale_state,
            alpha_modifier_state,
            background_effect_state,
            content_type_state,
            fifo_state,
            commit_timing_state,
            tablet_state,
            cursor_shape_state,
            relative_pointer_state,
            pointer_gestures_state,
            pointer_warp_state,
            keyboard_shortcuts_inhibit_state,
            xdg_activation_state,
            xdg_foreign_state,
            foreign_toplevel_list_state,
            xdg_system_bell_state,
            xdg_toplevel_icon_state,
            xdg_toplevel_tag_state,
            xdg_decoration_state,
            kde_decoration_state,
            idle_notifier_state,
            idle_inhibit_state,
            session_lock_state,
            text_input_state,
            input_method_state,
            virtual_keyboard_state,
            layer_shell_state,
            image_capture_source_state,
            output_capture_source_state,
            image_copy_capture_state,
            image_copy_sessions: Vec::new(),
            cursor_sessions: Vec::new(),
            keyboard_shortcuts_inhibitors: HashMap::new(),
            idle_inhibiting_surfaces: HashSet::new(),
            unmapped_layer_surfaces: HashSet::new(),
            layer_focus: None,
            foreign_toplevels: HashMap::new(),
            popup_grab: None,
            last_window_focus: None,
            preferred_output: None,
            output_config_pending: false,
            config_watcher: crate::utils::watcher::ConfigWatcher::default(),
            satellite: None,
            lock_state: LockState::Unlocked,
            lock_surfaces: Vec::new(),
            lock_rendered_outputs: HashSet::new(),
            lock_backgrounds: HashMap::new(),
            fullscreen_backgrounds: HashMap::new(),
            drag_hint_buffers: None,
            overview_buffers: HashMap::new(),
            shaders: crate::core::render::Shaders::new(),
            decorations: crate::core::render::Decorations::new(),
            toasts: crate::core::ui::Toasts::new(),
            continuous_redraw: false,
            config_generation: 0,
        });
    }

    /// Re-fire a binding at the keyboard repeat rate until a key is released
    /// (ported from niri's `start_key_repeat`).
    pub fn start_bind_repeat(&mut self, bind: &Keybind) {
        if !bind.repeat {
            return;
        }
        self.stop_bind_repeat();

        let keyboard = &self.config.input.keyboard;
        let repeat_rate = keyboard.repeat_rate;
        if repeat_rate == 0 {
            return;
        }
        let repeat_duration = Duration::from_secs_f64(1.0 / f64::from(repeat_rate));
        let timer = Timer::from_duration(Duration::from_millis(u64::from(keyboard.repeat_delay)));

        let action = bind.action.clone();
        let token = self
            .event_loop_handle
            .insert_source(timer, move |_, _, state: &mut State| {
                crate::core::actions::do_action(state, action.clone(), None);
                return TimeoutAction::ToDuration(repeat_duration);
            });
        match token {
            Ok(token) => self.bind_repeat_timer = Some(token),
            Err(err) => warn!("error arming the binding repeat: {err:?}"),
        }
    }

    /// Stop the binding repeat, if one is running.
    pub fn stop_bind_repeat(&mut self) {
        if let Some(token) = self.bind_repeat_timer.take() {
            self.event_loop_handle.remove(token);
        }
    }

    /// Where the pointer is, in compositor coordinates.
    pub fn pointer_location(&self) -> Point<f64, Logical> {
        let Some(pointer) = self.seat.get_pointer() else {
            return Point::from((0.0, 0.0));
        };
        return pointer.current_location();
    }

    /// The surface (and its absolute location) under a compositor-space point.
    ///
    /// The search follows the drawing order — session-lock surfaces first,
    /// then layer surfaces above windows (Overlay, Top), then windows, then
    /// the layers below them (Bottom, Background) — so that what the pointer
    /// hits is what the user sees.
    pub fn surface_under(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.lock_state.is_locked() {
            return self.lock_surface_under(location);
        }
        // The overview is a compositor-owned view: pointer and touch input is
        // interpreted as card clicks and must never reach a client.
        if self.layout.overview_visible(Instant::now()) {
            return None;
        }

        let covers_top = self.fullscreen_covers_top(location);
        if let Some(hit) = self.layers_under(
            location,
            if covers_top {
                &[Layer::Overlay]
            } else {
                &[Layer::Overlay, Layer::Top]
            },
        ) {
            return Some(hit);
        }

        if let Some((window, window_location)) = self.space.element_under(location) {
            let local = location - window_location.to_f64();
            if let Some((surface, surface_location)) =
                window.surface_under(local, WindowSurfaceType::ALL)
            {
                return Some((surface, (surface_location + window_location).to_f64()));
            }
        }

        if covers_top {
            if let Some(hit) = self.layers_under(location, &[Layer::Top]) {
                return Some(hit);
            }
        }

        return self.layers_under(location, &[Layer::Bottom, Layer::Background]);
    }

    /// Whether a compositor-managed fullscreen window covers layer surfaces
    /// on the Top layer at `location` (`fullscreen.cover_top_layer`).
    pub fn fullscreen_covers_top(&self, location: Point<f64, Logical>) -> bool {
        if !self.config.fullscreen.cover_top_layer {
            return false;
        }
        let Some(output) = self.output_at(location) else {
            return false;
        };
        let Some(area) = self.space.output_geometry(&output) else {
            return false;
        };
        return self
            .layout
            .fullscreen_at(&output, area, location, Instant::now());
    }

    /// The output whose geometry contains a compositor-space point.
    pub fn output_at(&self, location: Point<f64, Logical>) -> Option<Output> {
        return self
            .space
            .outputs()
            .find(|output| {
                return self
                    .space
                    .output_geometry(output)
                    .is_some_and(|geometry| return geometry.to_f64().contains(location));
            })
            .cloned();
    }

    /// The layer surface under a point, searching the given layers topmost
    /// first. `layers` is ordered from the front of the screen to the back.
    fn layers_under(
        &self,
        location: Point<f64, Logical>,
        layers: &[Layer],
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.output_at(location)?;
        let output_geometry = self.space.output_geometry(&output)?.to_f64();
        let map = layer_map_for_output(&output);

        for layer in layers {
            for layer_surface in map.layers_on(*layer).rev() {
                let Some(layer_location) = map.layer_geometry(layer_surface) else {
                    continue;
                };
                let layer_origin = output_geometry.loc + layer_location.loc.to_f64();
                let local = location - layer_origin;
                if let Some((surface, surface_location)) =
                    layer_surface.surface_under(local, WindowSurfaceType::ALL)
                {
                    return Some((surface, layer_origin + surface_location.to_f64()));
                }
            }
        }

        return None;
    }

    /// The lock surface under a point; while locked, nothing else takes input.
    fn lock_surface_under(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.output_at(location)?;
        let output_geometry = self.space.output_geometry(&output)?.to_f64();
        let (_, lock_surface) = self
            .lock_surfaces
            .iter()
            .find(|(lock_output, _)| return lock_output == &output)?;

        let local = location - output_geometry.loc;
        let (surface, surface_location) = under_from_surface_tree(
            lock_surface.wl_surface(),
            local,
            (0, 0),
            WindowSurfaceType::ALL,
        )?;
        return Some((surface, output_geometry.loc + surface_location.to_f64()));
    }

    /// Per-output state, created on first use.
    pub fn output_state(&mut self, output: &Output) -> &mut OutputState {
        return self.output_state.entry(output.clone()).or_default();
    }

    /// The `CLOCK_MONOTONIC` reading now.
    ///
    /// `wl_surface.frame` callbacks, `wp_presentation` and the screencopy /
    /// image-copy timestamps are specified on that clock, not since compositor
    /// start.
    pub fn monotonic_now(&self) -> Duration {
        return self.monotonic_start + self.start_time.elapsed();
    }

    /// Map a `CLOCK_MONOTONIC` timestamp onto this process's [`Instant`] timeline.
    pub fn instant_at_monotonic(&self, time: Duration) -> Instant {
        return self.start_time + time.saturating_sub(self.monotonic_start);
    }

    /// Freeze the animation clock for the frame `output` is about to draw.
    ///
    /// The returned time is the VBlank the frame is expected to land on. Every
    /// animation sampled during this render uses it, so consecutive frames step
    /// by exactly one refresh interval even when the event loop is late.
    pub fn begin_frame(&mut self, output: &Output) -> Instant {
        let interval = crate::core::redraw::frame_interval(output);
        let now = Instant::now();
        let clock = &mut self.output_state(output).frame_clock;
        clock.set_refresh_interval(Some(interval));
        return clock.next_presentation_time(now);
    }

    /// Record the presentation of a frame on `output`.
    ///
    /// udev passes the DRM VBlank timestamp; winit and headless pass the host
    /// frame boundary. The frame clock snaps the next animation sample to the
    /// grid this establishes.
    pub fn frame_presented(&mut self, output: &Output, time: Instant) {
        self.output_state(output).frame_clock.presented(time);
    }

    /// The `outputs.<name>.background` colour of `output`, if the file sets one.
    ///
    /// The backends fall back to their own default when this is `None`; the
    /// value is straight (not premultiplied) alpha, ready for a clear colour.
    pub fn output_background(&self, output: &Output) -> Option<poly_config::Color> {
        return self
            .output_state
            .get(output)
            .and_then(|state| return state.background);
    }

    /// Queue a redraw of one output.
    pub fn queue_redraw(&mut self, output: &Output) {
        let state = self.output_state(output);
        state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
    }

    /// Queue a redraw of every output.
    pub fn queue_redraw_all(&mut self) {
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            self.queue_redraw(&output);
        }
    }
}
