// Ported from niri (GPL-3.0-or-later): src/input/mod.rs (`Action` handling)

//! Action dispatch: the one implementation of the `poly-ipc` action vocabulary
//! (`AGENTS.md` §2.6).
//!
//! Key bindings, the IPC `Action` request and `poly msg action` all end up
//! here. Layout actions are forwarded to the active workspace's scripted
//! layout, when it has one; every other action acts on the compositor state
//! directly.

use poly_ipc::Action;
use smithay::desktop::Window;
use smithay::input::pointer::{CursorIcon, CursorImageStatus, Focus, GrabStartData};
use smithay::utils::SERIAL_COUNTER;
use tracing::{debug, info, warn};

use crate::core::input::move_grab::{self, MoveGrab};
use crate::core::input::resize_grab::{self, ResizeGrab};
use crate::state::State;
use crate::utils::spawn;

/// The button an IPC-started drag pretends to hold.
///
/// `poly msg action move-window` has no button behind it. The grabs only record
/// the value (they end when no button is pressed any more), so a synthetic one
/// is enough.
const SYNTHETIC_BUTTON: u32 = 0x110; // BTN_LEFT

/// Run an action.
///
/// `button` is the pointer button that triggered it, when the action came from
/// a mouse binding: starting a drag needs to know which button is held.
pub fn do_action(state: &mut State, action: Action, button: Option<u32>) {
    match action {
        Action::Quit => {
            info!("quitting on request");
            state.core.stop_signal.stop();
        }
        Action::Spawn { command } => {
            spawn::spawn(command);
        }
        Action::SpawnShell { command } => {
            spawn::spawn_sh(command);
        }
        Action::CloseWindow => close_window(state),
        Action::ReloadConfig => crate::core::config::reload(state),

        Action::FocusNext | Action::FocusRight | Action::FocusDown => {
            crate::core::layout::LayoutManager::focus_relative(state, 1);
        }
        Action::FocusPrev | Action::FocusLeft | Action::FocusUp => {
            crate::core::layout::LayoutManager::focus_relative(state, -1);
        }
        Action::FocusWindow { id } => {
            if !crate::core::layout::LayoutManager::focus_window_id(state, id) {
                debug!("focus-window: there is no window with id {id}");
            }
        }
        Action::MinimizeWindow { id } => match id {
            Some(id) => {
                if !crate::core::layout::LayoutManager::minimize_window(state, id) {
                    debug!("minimize-window: window {id} cannot be minimized");
                }
            }
            None => crate::core::layout::LayoutManager::minimize_focused(state),
        },
        Action::RestoreWindow { id } => match id {
            Some(id) => {
                if !crate::core::layout::LayoutManager::restore_window(state, id) {
                    debug!("restore-window: window {id} is not minimized");
                }
            }
            None => crate::core::layout::LayoutManager::restore_last_minimized(state),
        },

        Action::MoveWindow => match button {
            Some(button) => start_move(state, button, None),
            None => warn!("move-window must be bound to a mouse button, e.g. [\"Mod+MouseLeft\"]"),
        },
        Action::ResizeWindow => match button {
            Some(button) => start_resize(state, button, None),
            None => {
                warn!("resize-window must be bound to a mouse button, e.g. [\"Mod+MouseRight\"]")
            }
        },

        Action::MoveWindowLeft | Action::MoveWindowUp => {
            crate::core::layout::LayoutManager::move_window_relative(state, -1);
        }
        Action::MoveWindowRight | Action::MoveWindowDown => {
            crate::core::layout::LayoutManager::move_window_relative(state, 1);
        }
        Action::ToggleFloating => crate::core::layout::LayoutManager::toggle_floating(state),
        Action::ToggleFullscreen => {
            crate::core::layout::LayoutManager::toggle_fullscreen(state);
        }
        Action::ToggleWindowedFullscreen => {
            crate::core::layout::LayoutManager::toggle_windowed_fullscreen(state);
        }
        Action::SetLayout { name } => {
            crate::core::layout::LayoutManager::set_layout(state, &name);
        }
        Action::CycleLayout => crate::core::layout::LayoutManager::cycle_layout(state),

        Action::FocusWorkspaceUp => {
            crate::core::layout::LayoutManager::focus_workspace_relative(state, -1);
        }
        Action::FocusWorkspaceDown => {
            crate::core::layout::LayoutManager::focus_workspace_relative(state, 1);
        }
        Action::MoveWindowToWorkspaceUp => {
            crate::core::layout::LayoutManager::move_window_to_workspace_relative(state, -1);
        }
        Action::MoveWindowToWorkspaceDown => {
            crate::core::layout::LayoutManager::move_window_to_workspace_relative(state, 1);
        }
        Action::MoveWorkspaceUp => {
            crate::core::layout::LayoutManager::move_workspace(state, -1);
        }
        Action::MoveWorkspaceDown => {
            crate::core::layout::LayoutManager::move_workspace(state, 1);
        }
        Action::FocusWorkspace { index } => {
            crate::core::layout::LayoutManager::focus_workspace(state, index);
        }
        Action::MoveWindowToWorkspace { index } => {
            crate::core::layout::LayoutManager::move_window_to_workspace(state, index);
        }
        Action::LayoutAction { name, args } => {
            crate::core::layout::LayoutManager::run_layout_action(state, &name, &args);
        }
    }
}

/// Run an action that came from IPC rather than from a binding.
///
/// The vocabulary is the same; only a drag action differs, since there is no
/// button press and no pointer grab behind the request.
pub fn do_ipc_action(state: &mut State, action: Action) {
    match action {
        Action::MoveWindow => match ipc_drag_target(state) {
            Some(window) => start_move(state, SYNTHETIC_BUTTON, Some(window)),
            None => debug!("move-window: no window under the pointer and nothing focused"),
        },
        Action::ResizeWindow => match ipc_drag_target(state) {
            Some(window) => start_resize(state, SYNTHETIC_BUTTON, Some(window)),
            None => debug!("resize-window: no window under the pointer and nothing focused"),
        },
        other => do_action(state, other, None),
    }
}

/// The window an IPC drag acts on: the one under the pointer, else the focused
/// one.
fn ipc_drag_target(state: &mut State) -> Option<Window> {
    let pointer = state.core.seat.get_pointer()?;
    let location = pointer.current_location();
    let under = state
        .core
        .space
        .element_under(location)
        .map(|(window, _)| return window.clone());
    match under {
        Some(window) => return Some(window),
        None => return focused_window(state),
    }
}

/// The window that currently has the keyboard focus, if any.
pub fn focused_window(state: &State) -> Option<Window> {
    let keyboard = state.core.seat.get_keyboard()?;
    let surface = keyboard.current_focus()?;
    let root = crate::core::window::root_surface(&surface);
    return crate::core::window::window_for_root(state, &root);
}

/// Ask the focused window to close.
fn close_window(state: &mut State) {
    let Some(window) = focused_window(state) else {
        debug!("close-window: nothing is focused");
        return;
    };
    let Some(toplevel) = window.toplevel() else {
        return;
    };
    debug!("asking the focused window to close");
    toplevel.send_close();
}

/// `move-window`: drag the window under the pointer around.
///
/// `window` overrides the window under the pointer; IPC requests use it because
/// the pointer may be somewhere else entirely.
fn start_move(state: &mut State, button: u32, target: Option<Window>) {
    let Some(pointer) = state.core.seat.get_pointer() else {
        return;
    };
    let location = pointer.current_location();
    let window = match target {
        Some(window) => Some(window),
        None => state
            .core
            .space
            .element_under(location)
            .map(|(window, _)| return window.clone()),
    };
    let Some(window) = window else {
        return;
    };

    // Grabbing a window also activates it: the binding consumed the press, so
    // the regular click-to-focus path never ran.
    crate::core::window::focus_window(
        state,
        &window,
        window
            .toplevel()
            .map(|toplevel| return toplevel.wl_surface().clone()),
        SERIAL_COUNTER.next_serial(),
    );

    // No focus in the start data: the grab owns the pointer, the client must
    // not see the drag (`Focus::Clear`).
    let start_data = GrabStartData {
        focus: None,
        button,
        location,
    };
    let Some(grab) = MoveGrab::new(state, &window, start_data) else {
        debug!("move-window: the window cannot be dragged right now");
        return;
    };
    debug!("starting an interactive move");
    let serial = SERIAL_COUNTER.next_serial();
    pointer.set_grab(state, grab, serial, Focus::Clear);
    state.core.cursor_status = CursorImageStatus::Named(CursorIcon::Grabbing);
    state.core.queue_redraw_all();
}

/// `resize-window`: resize by the edges the pointer is closest to.
///
/// `target` works like in [`start_move`]; a drag that was started without the
/// pointer over the window resizes from the bottom-right corner, since there is
/// no "closest edge" to read.
fn start_resize(state: &mut State, button: u32, target: Option<Window>) {
    let from_ipc = target.is_some();
    let Some(pointer) = state.core.seat.get_pointer() else {
        return;
    };
    let location = pointer.current_location();
    let window = match target {
        Some(window) => Some(window),
        None => state
            .core
            .space
            .element_under(location)
            .map(|(window, _)| return window.clone()),
    };
    let Some(window) = window else {
        return;
    };

    crate::core::window::focus_window(
        state,
        &window,
        window
            .toplevel()
            .map(|toplevel| return toplevel.wl_surface().clone()),
        SERIAL_COUNTER.next_serial(),
    );

    let Some(rect) = move_grab::window_rect(state, &window) else {
        return;
    };

    let pick = state.core.config.window_drag.resize_pick;
    let mut edges = resize_grab::pick_edges(rect, location, pick);
    if edges.is_empty() {
        if !from_ipc {
            debug!("the pointer is in the middle of the window; nothing to resize");
            return;
        }
        // An IPC drag has no pointer position to read: grow towards the
        // bottom-right, like a window manager's keyboard resize.
        edges = resize_grab::ResizeEdges::RIGHT | resize_grab::ResizeEdges::BOTTOM;
    }

    let start_data = GrabStartData {
        focus: None,
        button,
        location,
    };
    let Some(grab) = ResizeGrab::new(state, &window, edges, start_data) else {
        debug!("resize-window: the window cannot be resized right now");
        return;
    };
    debug!("starting an interactive resize");
    let serial = SERIAL_COUNTER.next_serial();
    pointer.set_grab(state, grab, serial, Focus::Clear);
    state.core.cursor_status = CursorImageStatus::Named(edges.cursor_icon());
    state.core.queue_redraw_all();
}
