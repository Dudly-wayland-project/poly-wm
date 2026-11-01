//! Snapshots and event-stream diffing (`docs/ipc.md`).
//!
//! The compositor keeps one [`EventStreamState`] as the baseline of what its
//! clients know, and compares the live state against it. Every difference
//! becomes an [`Event`], and the baseline is then replaced by the live state, so
//! the next comparison starts from what the clients have.
//!
//! Diffing the state instead of emitting an event from every place that mutates
//! it has two advantages: nothing can be forgotten (a code path that changes
//! windows without telling anyone still produces the right event), and outputs
//! are covered without a hook for every backend (`winit` resizes, DRM hotplugs
//! and headless mode changes all show up in the same snapshot).

use std::collections::HashSet;

use poly_ipc::{Event, FullscreenMode, Mode, Output, Window, Workspace};

use crate::core::layout::LayoutManager;
use crate::state::State;

/// The connected outputs, in the order the space holds them.
pub fn outputs(state: &State) -> Vec<Output> {
    let mut outputs = Vec::new();
    for output in state.core.space.outputs() {
        let physical = output.physical_properties();
        let name = output.name();
        let location = output.current_location();
        // The geometry the layout works with: the mode size divided by the
        // scale, i.e. what a fullscreen window would get.
        let logical_size = state
            .core
            .space
            .output_geometry(output)
            .map(|geometry| return (geometry.size.w, geometry.size.h))
            .unwrap_or_default();
        let serial = match physical.serial_number.is_empty() {
            true => None,
            false => Some(physical.serial_number.clone()),
        };
        let physical_size = match physical.size.w > 0 && physical.size.h > 0 {
            true => Some((physical.size.w, physical.size.h)),
            false => None,
        };
        outputs.push(Output {
            name,
            make: physical.make.clone(),
            model: physical.model.clone(),
            serial,
            physical_size,
            position: (location.x, location.y),
            logical_size,
            scale: output.current_scale().fractional_scale(),
            current_mode: output.current_mode().map(mode_of),
            modes: output.modes().into_iter().map(mode_of).collect(),
        });
    }
    return outputs;
}

/// The workspace stack of every output, in output order.
pub fn workspaces(state: &State) -> Vec<Workspace> {
    let focused_output = focused_output(state);
    let focused_window = crate::core::layout::focused_id(state);
    let mut workspaces = Vec::new();
    for output in state.core.space.outputs() {
        let name = output.name();
        let is_focused_output = focused_output.as_deref() == Some(name.as_str());
        for snapshot in state.core.layout.workspaces_of(output) {
            let is_focused = is_focused_output && snapshot.active;
            workspaces.push(Workspace {
                // Workspaces have no ids: the IPC layer exposes 1-based numbers
                // inside the output's stack.
                index: snapshot.index + 1,
                output: Some(name.clone()),
                is_active: snapshot.active,
                is_focused,
                active_window_id: active_window(
                    &snapshot,
                    if is_focused { focused_window } else { None },
                ),
                layout: snapshot.layout,
            });
        }
    }
    return workspaces;
}

/// The window that `workspace` would focus when it is entered.
///
/// On the focused workspace that is the real keyboard focus; elsewhere it is the
/// window the workspace remembers, falling back to the topmost one so that a
/// status bar shows something sensible.
fn active_window(
    workspace: &crate::core::layout::WorkspaceSnapshot,
    focused: Option<u64>,
) -> Option<u64> {
    let contains =
        |id: u64| return workspace.tiled.contains(&id) || workspace.floating.contains(&id);
    if let Some(id) = focused.filter(|id| return contains(*id)) {
        return Some(id);
    }
    if let Some(id) = workspace.focus.filter(|id| return contains(*id)) {
        return Some(id);
    }
    return workspace
        .floating
        .last()
        .or_else(|| return workspace.tiled.last())
        .copied();
}

/// The managed windows, back to front per output.
pub fn windows(state: &State) -> Vec<Window> {
    let focused = crate::core::layout::focused_id(state);
    let mut windows = Vec::new();
    let mut seen = HashSet::new();
    for output in state.core.space.outputs() {
        for workspace in state.core.layout.workspaces_of(output) {
            for id in workspace.tiled.iter().chain(workspace.floating.iter()) {
                if !seen.insert(*id) {
                    continue;
                }
                if let Some(window) = window(state, *id, focused) {
                    windows.push(window);
                }
            }
        }
    }
    // Windows that no workspace holds yet: mapped but not placed, or pulled out
    // of the layout by an interactive drag.
    for id in state.core.layout.ids() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(window) = window(state, id, focused) {
            windows.push(window);
        }
    }
    return windows;
}

/// One window, or `None` when it disappeared while we were looking.
fn window(state: &State, id: u64, focused: Option<u64>) -> Option<Window> {
    let snapshot = state.core.layout.window_snapshot(id)?;
    return Some(Window {
        id: snapshot.id,
        title: snapshot.title,
        app_id: snapshot.app_id,
        is_focused: focused == Some(id),
        is_floating: !snapshot.tiled,
        fullscreen: snapshot.fullscreen.map(fullscreen_of),
        output: snapshot.output.map(|output| return output.name()),
        // 1-based on the wire.
        workspace: snapshot.workspace.map(|index| return index + 1),
    });
}

/// The name of the output that has keyboard focus.
pub fn focused_output(state: &State) -> Option<String> {
    return LayoutManager::active_output(state).map(|output| return output.name());
}

/// The window that has keyboard focus.
pub fn focused_window(state: &State) -> Option<Window> {
    let id = crate::core::layout::focused_id(state)?;
    return window(state, id, Some(id));
}

/// Replace the baseline with the live state, without emitting anything.
///
/// Called when a client starts streaming: it gets the whole state instead of
/// the difference against a state it never saw.
pub fn resync(state: &mut State) {
    let workspaces = workspaces(state);
    let windows = windows(state);
    let outputs = outputs(state);
    let overview = state.core.layout.overview_open();
    let events = &mut state.core.ipc.events;

    events.workspaces.workspaces = workspaces;
    events.windows.windows = windows;
    events.outputs.outputs = outputs
        .into_iter()
        .map(|output| return (output.name.clone(), output))
        .collect();
    events.overview.is_open = overview;
}

/// The events that describe the difference between the live state and the
/// baseline, and the baseline updated to the live state.
pub fn diff(state: &mut State) -> Vec<Event> {
    let mut events = Vec::new();

    let live_workspaces = workspaces(state);
    diff_workspaces(
        &state.core.ipc.events.workspaces.workspaces,
        &live_workspaces,
        &mut events,
    );
    state.core.ipc.events.workspaces.workspaces = live_workspaces;

    let live_windows = windows(state);
    diff_windows(
        &state.core.ipc.events.windows.windows,
        &live_windows,
        &mut events,
    );
    state.core.ipc.events.windows.windows = live_windows;

    let live_outputs = outputs(state);
    let known_outputs = &mut state.core.ipc.events.outputs.outputs;
    for output in &live_outputs {
        match known_outputs.get(&output.name) {
            Some(previous) if previous == output => (),
            // A new name is an output that just appeared; a known name with a
            // different mode, scale or position changed.
            Some(_) => events.push(Event::OutputChanged {
                output: output.clone(),
            }),
            None => events.push(Event::OutputAdded {
                output: output.clone(),
            }),
        }
    }
    let gone = known_outputs
        .keys()
        .filter(|name| {
            return !live_outputs
                .iter()
                .any(|output| return &output.name == *name);
        })
        .cloned()
        .collect::<Vec<_>>();
    for name in gone {
        events.push(Event::OutputRemoved { name });
    }
    *known_outputs = live_outputs
        .into_iter()
        .map(|output| return (output.name.clone(), output))
        .collect();

    let overview = state.core.layout.overview_open();
    if state.core.ipc.events.overview.is_open != overview {
        state.core.ipc.events.overview.is_open = overview;
        events.push(Event::OverviewOpenedOrClosed { is_open: overview });
    }

    return events;
}

/// Workspace differences.
///
/// A change of which workspace owns the screen, or of the shape of the stack,
/// is sent as a full snapshot: [`Event`] has no narrower event for it, and a
/// status bar must not keep drawing an outdated active workspace. Layout and
/// remembered-focus changes are sent as narrow events.
fn diff_workspaces(known: &[Workspace], live: &[Workspace], events: &mut Vec<Event>) {
    let same_shape = known.len() == live.len()
        && known.iter().zip(live).all(|(a, b)| {
            return a.index == b.index && a.output == b.output;
        });
    let needs_snapshot = !same_shape
        || known
            .iter()
            .zip(live)
            .any(|(a, b)| return a.is_active != b.is_active || a.is_focused != b.is_focused);

    if needs_snapshot {
        events.push(Event::WorkspacesChanged {
            workspaces: live.to_vec(),
        });
        return;
    }

    for (previous, workspace) in known.iter().zip(live) {
        let Some(output) = workspace.output.clone() else {
            continue;
        };
        if previous.layout != workspace.layout {
            events.push(Event::LayoutChanged {
                output: Some(output.clone()),
                workspace_index: Some(workspace.index),
                name: workspace.layout.clone(),
            });
        }
        if previous.active_window_id != workspace.active_window_id {
            events.push(Event::WorkspaceActiveWindowChanged {
                output,
                index: workspace.index,
                active_window_id: workspace.active_window_id,
            });
        }
    }
}

/// Window differences.
///
/// `is_focused` is left out of the per-window comparison: a focus change is one
/// [`Event::WindowFocusChanged`] instead of a rewrite of every window that lost
/// or gained the flag. Floating and fullscreen changes additionally get their
/// own narrow event, for scripts that do not want to inspect whole windows.
fn diff_windows(known: &[Window], live: &[Window], events: &mut Vec<Event>) {
    for previous in known {
        if !live.iter().any(|window| return window.id == previous.id) {
            events.push(Event::WindowClosed { id: previous.id });
        }
    }

    for window in live {
        let previous = known.iter().find(|known| return known.id == window.id);
        match previous {
            Some(previous) if same_window(previous, window) => (),
            _ => events.push(Event::WindowOpenedOrChanged {
                window: window.clone(),
            }),
        }
    }

    let focused_before = known.iter().find(|window| return window.is_focused);
    let focused_now = live.iter().find(|window| return window.is_focused);
    if focused_before.map(|window| return window.id) != focused_now.map(|window| return window.id) {
        events.push(Event::WindowFocusChanged {
            id: focused_now.map(|window| return window.id),
        });
    }

    for window in live {
        let Some(previous) = known.iter().find(|known| return known.id == window.id) else {
            continue;
        };
        if previous.is_floating != window.is_floating {
            events.push(Event::FloatingChanged {
                id: window.id,
                floating: window.is_floating,
            });
        }
        if previous.fullscreen != window.fullscreen {
            events.push(Event::FullscreenChanged {
                id: window.id,
                fullscreen: window.fullscreen,
            });
        }
    }
}

/// Every field of a window except the focus flag.
fn same_window(a: &Window, b: &Window) -> bool {
    return a.id == b.id
        && a.title == b.title
        && a.app_id == b.app_id
        && a.is_floating == b.is_floating
        && a.fullscreen == b.fullscreen
        && a.output == b.output
        && a.workspace == b.workspace;
}

fn mode_of(mode: smithay::output::Mode) -> Mode {
    return Mode {
        width: mode.size.w.max(0) as u16,
        height: mode.size.h.max(0) as u16,
        refresh: mode.refresh.max(0) as u32,
    };
}

fn fullscreen_of(mode: crate::core::layout::FullscreenMode) -> FullscreenMode {
    return match mode {
        crate::core::layout::FullscreenMode::Fullscreen => FullscreenMode::Fullscreen,
        crate::core::layout::FullscreenMode::Windowed => FullscreenMode::Windowed,
    };
}
