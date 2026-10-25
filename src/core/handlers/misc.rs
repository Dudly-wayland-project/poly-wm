// Ported from niri (GPL-3.0-or-later): src/handlers/mod.rs, src/handlers/xdg_shell.rs
//
//! Handlers for the protocol globals that need little or no Poly-specific
//! logic: scale metadata, decorations, activation, taskbar toplevels, idle
//! management, pointer warp and shortcut inhibits (`docs/plan.md` §7).

use smithay::backend::input::{InputTime, TabletToolDescriptor};
use smithay::input::pointer::MotionEvent;
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Resource, WEnum};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::foreign_toplevel_list::{
    ForeignToplevelListHandler, ForeignToplevelListState,
};
use smithay::wayland::fractional_scale::FractionalScaleHandler;
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
};
use smithay::wayland::pointer_warp::PointerWarpHandler;
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgToplevelSurfaceData};
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};
use smithay::wayland::xdg_system_bell::XdgSystemBellHandler;
use smithay::wayland::xdg_toplevel_icon::XdgToplevelIconHandler;
use smithay::wayland::xdg_toplevel_tag::XdgToplevelTagHandler;
use tracing::{debug, info, trace};

use crate::core::window::{focus_window, root_surface, window_for_root};
use crate::state::State;

impl FractionalScaleHandler for State {}

impl TabletSeatHandler for State {
    type ToolFocus = WlSurface;

    fn tablet_tool_image(&mut self, _tool: &TabletToolDescriptor, image: CursorImageStatus) {
        // Tablet tools share the pointer cursor for now (niri has the same
        // FIXME: per-tool cursors).
        self.core.cursor_status = image;
        self.core.queue_redraw_all();
    }
}

/// Poly stores no icon cache yet; taskbars (M5) read icons from
/// `ext-foreign-toplevel-list` metadata instead.
impl XdgToplevelIconHandler for State {}

/// Tags are kept client-side; window rules match them from M3 on.
impl XdgToplevelTagHandler for State {}

impl XdgForeignHandler for State {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        return &mut self.core.xdg_foreign_state;
    }
}

impl ForeignToplevelListHandler for State {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        return &mut self.core.foreign_toplevel_list_state;
    }
}

impl XdgSystemBellHandler for State {
    fn ring(&mut self, surface: Option<WlSurface>) {
        // No audible bell in the compositor; terminals do their own BEL.
        trace!(?surface, "system bell");
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // Clients only see this global while `prefer_no_csd` is on, in which
        // case Poly decorates: answer `ServerSide` so the client drops its own
        // decoration (Poly's border, corners and shadow follow in M4). A rule
        // with `no_csd = false` instead asks this window to keep its own.
        let mode = if toplevel_no_csd(self, &toplevel) {
            Mode::ServerSide
        } else {
            Mode::ClientSide
        };
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: Mode) {
        // A rule may pin the window to client-side decorations; otherwise
        // honor what the client asks for instead of forcing our preferred mode
        // (niri does the same: SDL2 used to leave windows hidden otherwise).
        let mode = if toplevel_no_csd(self, &toplevel) {
            mode
        } else {
            Mode::ClientSide
        };
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });

        // The protocol requires a configure in response; the initial one may
        // still be pending, in which case it will carry the mode.
        if toplevel.is_initial_configure_sent() {
            if window_for_root(self, toplevel.wl_surface()).is_some() {
                toplevel.send_pending_configure();
            } else {
                toplevel.send_configure();
            }
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        // Unsetting means "the compositor decides": keep the rule's answer
        // rather than dropping back to the protocol default.
        let mode = if toplevel_no_csd(self, &toplevel) {
            Mode::ServerSide
        } else {
            Mode::ClientSide
        };
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }
}

/// The effective `no_csd` setting for a toplevel that may not have mapped yet.
///
/// A rule matches on app id and title, which clients normally set before
/// creating their decoration object; without them no rule matches and
/// `general.prefer_no_csd` applies.
fn toplevel_no_csd(state: &State, toplevel: &ToplevelSurface) -> bool {
    let identity = WindowIdentity::from_toplevel(toplevel);
    let (app_id, title) = identity.as_pair();
    return crate::core::render::visual::WindowVisuals::resolve(&state.core.config, app_id, title)
        .no_csd;
}

impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        return &self.core.kde_decoration_state;
    }

    fn request_mode(
        &mut self,
        _surface: &WlSurface,
        decoration: &smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
        mode: WEnum<
            smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::Mode,
        >,
    ) {
        // Echo the requested mode back (the default behavior): the client
        // decides whether to draw decorations itself.
        if let WEnum::Value(mode) = mode {
            decoration.mode(mode);
        }
    }
}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        return &mut self.core.xdg_activation_state;
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Any client holding a token may ask for focus; that is what tokens
        // are for. M5 will add rate limiting for misbehaving clients.
        let root = root_surface(&surface);
        let Some(window) = window_for_root(self, &root) else {
            debug!("activation request for an unknown surface");
            return;
        };
        info!(?root, "activating a window on request");
        self.core.space.raise_element(&window, true);
        let toplevel_surface = window
            .toplevel()
            .map(|toplevel| return toplevel.wl_surface().clone());
        focus_window(
            self,
            &window,
            toplevel_surface,
            SERIAL_COUNTER.next_serial(),
        );
        self.core.queue_redraw_all();
    }
}

impl PointerWarpHandler for State {
    fn warp_pointer(
        &mut self,
        surface: WlSurface,
        _pointer: smithay::reexports::wayland_server::protocol::wl_pointer::WlPointer,
        pos: Point<f64, Logical>,
        serial: smithay::utils::Serial,
    ) {
        // Move the pointer to `pos` inside `surface`. We can translate the
        // position for toplevels and layer surfaces; anything else (popups,
        // subsurfaces, X11 windows) is left alone, like other compositors do.
        let Some(origin) = surface_origin(self, &surface) else {
            return;
        };
        let Some(pointer) = self.core.seat.get_pointer() else {
            return;
        };

        let location = origin + pos;
        let under = self.core.surface_under(location);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial,
                time: InputTime::now(),
            },
        );
        pointer.frame(self);
        self.core.queue_redraw_all();
    }
}

impl KeyboardShortcutsInhibitHandler for State {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        return &mut self.core.keyboard_shortcuts_inhibit_state;
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        // Like niri, we take the client's word for it: an X11-style app that
        // grabs the keyboard (games, remote desktops) gets to keep it.
        inhibitor.activate();
        self.core
            .keyboard_shortcuts_inhibitors
            .insert(inhibitor.wl_surface().clone(), inhibitor);
    }

    fn inhibitor_destroyed(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        self.core
            .keyboard_shortcuts_inhibitors
            .remove(&inhibitor.wl_surface().clone());
    }
}

impl IdleNotifierHandler for State {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        return &mut self.core.idle_notifier_state;
    }
}

impl IdleInhibitHandler for State {
    fn inhibit(&mut self, surface: WlSurface) {
        self.core.idle_inhibiting_surfaces.insert(surface);
        self.refresh_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.core.idle_inhibiting_surfaces.remove(&surface);
        self.refresh_idle_inhibit();
    }
}

/// The global position of a surface we know how to place (toplevel or layer).
fn surface_origin(state: &State, surface: &WlSurface) -> Option<Point<f64, Logical>> {
    for window in state.core.space.elements() {
        if window
            .toplevel()
            .is_some_and(|toplevel| return toplevel.wl_surface() == surface)
        {
            let location = state.core.space.element_location(window)?;
            return Some((location + window.geometry().loc).to_f64());
        }
    }

    for output in state.core.space.outputs() {
        let Some(output_geometry) = state.core.space.output_geometry(output) else {
            continue;
        };
        let map = smithay::desktop::layer_map_for_output(output);
        for layer in map.layers() {
            if layer.wl_surface() != surface {
                continue;
            }
            let Some(layer_geometry) = map.layer_geometry(layer) else {
                continue;
            };
            return Some(output_geometry.loc.to_f64() + layer_geometry.loc.to_f64());
        }
    }

    return None;
}

impl State {
    /// Push the current inhibitor set into the idle notifier.
    ///
    /// Inhibitors whose surface died (client crashed) are dropped here.
    pub fn refresh_idle_inhibit(&mut self) {
        self.core
            .idle_inhibiting_surfaces
            .retain(|surface| return surface.is_alive());

        let inhibited = !self.core.idle_inhibiting_surfaces.is_empty();
        self.core.idle_notifier_state.set_is_inhibited(inhibited);
    }

    /// Tell the idle notifier that the user did something (input, usually).
    pub fn notify_activity(&mut self) {
        self.core
            .idle_notifier_state
            .notify_activity(&self.core.seat);
    }

    /// Drop inhibitors of dead surfaces; called when clients go away.
    pub fn idle_inhibit_cleanup(&mut self) {
        if self.core.idle_inhibiting_surfaces.is_empty() {
            return;
        }
        let before = self.core.idle_inhibiting_surfaces.len();
        self.refresh_idle_inhibit();
        if self.core.idle_inhibiting_surfaces.len() != before {
            debug!("dropped idle inhibitors of dead surfaces");
        }
    }

    /// Whether bindings must step aside for the surface that holds focus.
    pub fn shortcuts_inhibited_for(&self, surface: &WlSurface) -> bool {
        let root = root_surface(surface);
        return self
            .core
            .keyboard_shortcuts_inhibitors
            .iter()
            .any(|(inhibited, inhibitor)| {
                return inhibitor.is_active()
                    && (inhibited == surface || root_surface(inhibited) == root);
            });
    }
}

/// The app id and title a client committed for a toplevel.
///
/// A named struct rather than a tuple: window rules match on both, and swapped
/// fields silently match the wrong thing (`docs/plan.md` §10.2).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WindowIdentity {
    /// `xdg_toplevel.set_app_id`.
    pub app_id: Option<String>,
    /// `xdg_toplevel.set_title`.
    pub title: Option<String>,
}

impl WindowIdentity {
    /// Read the identity straight from a toplevel surface.
    pub fn from_toplevel(toplevel: &ToplevelSurface) -> WindowIdentity {
        return smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
            let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
                return WindowIdentity::default();
            };
            // Role data always exists for a mapped toplevel, but a poisoned
            // lock must not take down the event loop (AGENTS.md §2.11).
            let data = data.lock().unwrap_or_else(|err| return err.into_inner());
            return WindowIdentity {
                app_id: data.app_id.clone(),
                title: data.title.clone(),
            };
        });
    }

    /// `(app_id, title)` as the borrowed pair window rules expect.
    pub fn as_pair(&self) -> (Option<&str>, Option<&str>) {
        return (self.app_id.as_deref(), self.title.as_deref());
    }
}

/// The app id and title a client committed, for taskbars and window rules.
pub fn window_title_and_app_id(window: &smithay::desktop::Window) -> WindowIdentity {
    let Some(toplevel) = window.toplevel() else {
        return WindowIdentity::default();
    };
    return WindowIdentity::from_toplevel(toplevel);
}
