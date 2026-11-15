//! Keeping a local copy of the compositor state from the event stream.
//!
//! 1. Connect to the socket and send [`crate::Request::EventStream`].
//! 2. Pass every [`Event`] to [`EventStreamState::apply`].
//! 3. Read the fields of the state whenever the UI needs to be redrawn.
//!
//! The compositor uses the same code: it keeps one [`EventStreamState`] as the
//! baseline, diffs the live state against it, and sends the difference. Using
//! one implementation on both sides is what keeps [`EventStreamStatePart::replicate`]
//! and the diff in sync (`docs/ipc.md`).
//!
//! Applying events never panics: a client and the compositor can disagree
//! about details (a workspace that vanished between two events, an id we never
//! saw), and a status bar must not crash over it.

use std::collections::HashMap;

use crate::{Event, Output, Window, Workspace};

/// One part of the state carried by the event stream.
pub trait EventStreamStatePart {
    /// Events that recreate this state from `Default::default()`.
    ///
    /// Sending all of them, in order, must leave a client that started empty
    /// in exactly this state.
    fn replicate(&self) -> Vec<Event>;

    /// Apply one event.
    ///
    /// Returns `None` when the event belongs to this part of the state, and
    /// `Some(event)` when it does not.
    fn apply(&mut self, event: Event) -> Option<Event>;
}

/// The full state carried by the event stream.
///
/// The parts are not updated atomically: between two events a client can see,
/// for example, the new window focus but the old window list. Use the events
/// that describe one object (`WindowOpenedOrChanged`) when a consistent
/// snapshot of that object matters.
#[derive(Debug, Default, Clone)]
pub struct EventStreamState {
    /// The workspace stack of every output.
    pub workspaces: WorkspacesState,
    /// Every managed window.
    pub windows: WindowsState,
    /// Every connected output, keyed by name.
    pub outputs: OutputsState,
    /// Whether the overview is open.
    pub overview: OverviewState,
    /// How the last configuration load went.
    pub config: ConfigState,
}

impl EventStreamState {
    /// Events that recreate this whole state on a fresh client.
    pub fn replicate(&self) -> Vec<Event> {
        let mut events = Vec::new();
        events.extend(self.workspaces.replicate());
        events.extend(self.windows.replicate());
        events.extend(self.outputs.replicate());
        events.extend(self.overview.replicate());
        events.extend(self.config.replicate());
        return events;
    }

    /// Apply one event to whichever part of the state handles it.
    pub fn apply(&mut self, event: Event) -> Option<Event> {
        let event = self.workspaces.apply(event)?;
        let event = self.windows.apply(event)?;
        let event = self.outputs.apply(event)?;
        let event = self.overview.apply(event)?;
        return self.config.apply(event);
    }

    /// The workspace that owns the focused output's screen.
    pub fn active_workspace(&self) -> Option<&Workspace> {
        return self
            .workspaces
            .workspaces
            .iter()
            .find(|workspace| return workspace.is_active && workspace.is_focused);
    }

    /// One workspace, by output name and 1-based index.
    pub fn workspace(&self, output: &str, index: usize) -> Option<&Workspace> {
        return self.workspaces.workspaces.iter().find(|workspace| {
            return workspace.index == index && workspace.output.as_deref() == Some(output);
        });
    }

    /// The window that currently has the keyboard focus.
    pub fn focused_window(&self) -> Option<&Window> {
        return self.windows.windows.iter().find(|window| {
            return window.is_focused;
        });
    }
}

/// [`EventStreamState::workspaces`].
#[derive(Debug, Default, Clone)]
pub struct WorkspacesState {
    /// Every workspace of every output, in output order.
    pub workspaces: Vec<Workspace>,
}

impl EventStreamStatePart for WorkspacesState {
    fn replicate(&self) -> Vec<Event> {
        return vec![Event::WorkspacesChanged {
            workspaces: self.workspaces.clone(),
        }];
    }

    fn apply(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::WorkspacesChanged { workspaces } => self.workspaces = workspaces,
            Event::WorkspaceActiveWindowChanged {
                output,
                index,
                active_window_id,
            } => {
                if let Some(workspace) = self.workspaces.iter_mut().find(|workspace| {
                    return workspace.index == index
                        && workspace.output.as_deref() == Some(&output);
                }) {
                    workspace.active_window_id = active_window_id;
                }
            }
            Event::LayoutChanged {
                output,
                workspace_index,
                name,
            } => {
                for workspace in &mut self.workspaces {
                    let on_output = match &output {
                        Some(output) => workspace.output.as_deref() == Some(output),
                        None => true,
                    };
                    let at_index = match workspace_index {
                        Some(index) => workspace.index == index,
                        None => true,
                    };
                    if on_output && at_index {
                        workspace.layout = name.clone();
                    }
                }
            }
            other => return Some(other),
        }
        return None;
    }
}

/// [`EventStreamState::windows`].
#[derive(Debug, Default, Clone)]
pub struct WindowsState {
    /// Every managed window, back to front per output.
    pub windows: Vec<Window>,
}

impl WindowsState {
    /// One window, by id.
    pub fn window(&self, id: u64) -> Option<&Window> {
        return self.windows.iter().find(|window| return window.id == id);
    }
}

impl EventStreamStatePart for WindowsState {
    fn replicate(&self) -> Vec<Event> {
        return vec![Event::WindowsChanged {
            windows: self.windows.clone(),
        }];
    }

    fn apply(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::WindowsChanged { windows } => self.windows = windows,
            Event::WindowOpenedOrChanged { window } => match self.window(window.id) {
                Some(existing) => {
                    let index = self
                        .windows
                        .iter()
                        .position(|candidate| return candidate.id == existing.id);
                    if let Some(index) = index {
                        self.windows[index] = window;
                    }
                }
                None => self.windows.push(window),
            },
            Event::WindowClosed { id } => self.windows.retain(|window| return window.id != id),
            Event::WindowFocusChanged { id } => {
                for window in &mut self.windows {
                    window.is_focused = Some(window.id) == id;
                }
            }
            Event::FloatingChanged { id, floating } => {
                if let Some(window) = self
                    .windows
                    .iter_mut()
                    .find(|window| return window.id == id)
                {
                    window.is_floating = floating;
                }
            }
            Event::FullscreenChanged { id, fullscreen } => {
                if let Some(window) = self
                    .windows
                    .iter_mut()
                    .find(|window| return window.id == id)
                {
                    window.fullscreen = fullscreen;
                }
            }
            other => return Some(other),
        }
        return None;
    }
}

/// [`EventStreamState::outputs`].
#[derive(Debug, Default, Clone)]
pub struct OutputsState {
    /// Every connected output, keyed by name.
    pub outputs: HashMap<String, Output>,
}

impl OutputsState {
    /// Every output, in name order (for stable rendering).
    pub fn sorted(&self) -> Vec<&Output> {
        let mut outputs = self.outputs.values().collect::<Vec<_>>();
        outputs.sort_by(|a, b| return a.name.cmp(&b.name));
        return outputs;
    }
}

impl EventStreamStatePart for OutputsState {
    fn replicate(&self) -> Vec<Event> {
        let mut outputs = self.outputs.values().cloned().collect::<Vec<Output>>();
        outputs.sort_by(|a, b| return a.name.cmp(&b.name));
        return vec![Event::OutputsChanged { outputs }];
    }

    fn apply(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::OutputsChanged { outputs } => {
                self.outputs = outputs
                    .into_iter()
                    .map(|output| return (output.name.clone(), output))
                    .collect();
            }
            Event::OutputAdded { output } => {
                self.outputs.insert(output.name.clone(), output);
            }
            Event::OutputChanged { output } => {
                self.outputs.insert(output.name.clone(), output);
            }
            Event::OutputRemoved { name } => {
                self.outputs.remove(&name);
            }
            other => return Some(other),
        }
        return None;
    }
}

/// [`EventStreamState::overview`].
#[derive(Debug, Default, Clone)]
pub struct OverviewState {
    /// Whether the workspace overview is open.
    pub is_open: bool,
}

impl EventStreamStatePart for OverviewState {
    fn replicate(&self) -> Vec<Event> {
        return vec![Event::OverviewOpenedOrClosed {
            is_open: self.is_open,
        }];
    }

    fn apply(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::OverviewOpenedOrClosed { is_open } => self.is_open = is_open,
            other => return Some(other),
        }
        return None;
    }
}

/// [`EventStreamState::config`].
#[derive(Debug, Default, Clone)]
pub struct ConfigState {
    /// Whether the last configuration load failed.
    pub failed: bool,
}

impl EventStreamStatePart for ConfigState {
    fn replicate(&self) -> Vec<Event> {
        return vec![Event::ConfigLoaded {
            failed: self.failed,
        }];
    }

    fn apply(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::ConfigLoaded { failed } => self.failed = failed,
            other => return Some(other),
        }
        return None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FullscreenMode;

    fn workspace(index: usize, output: &str, active: bool) -> Workspace {
        return Workspace {
            index,
            output: Some(output.into()),
            is_active: active,
            is_focused: active,
            active_window_id: None,
            layout: "columns".into(),
        };
    }

    fn window(id: u64) -> Window {
        return Window {
            id,
            title: None,
            app_id: None,
            is_focused: false,
            is_floating: false,
            is_minimized: false,
            fullscreen: None,
            output: Some("winit".into()),
            workspace: Some(1),
        };
    }

    #[test]
    fn replicate_then_apply_is_a_no_op() {
        let mut state = EventStreamState::default();
        state.workspaces.workspaces =
            vec![workspace(1, "winit", true), workspace(2, "winit", false)];
        state.windows.windows = vec![window(1), window(2)];
        state.overview.is_open = true;
        state.config.failed = true;

        let events = state.replicate();
        let mut other = EventStreamState::default();
        for event in events {
            assert_eq!(other.apply(event), None);
        }

        assert_eq!(other.workspaces.workspaces, state.workspaces.workspaces);
        assert_eq!(other.windows.windows, state.windows.windows);
        assert!(other.overview.is_open);
        assert!(other.config.failed);
        assert_eq!(other.active_workspace().map(|ws| return ws.index), Some(1));
    }

    #[test]
    fn applies_window_events() {
        let mut state = EventStreamState::default();
        state.apply(Event::WindowOpenedOrChanged { window: window(1) });
        state.apply(Event::WindowOpenedOrChanged { window: window(2) });

        // Opening an id that already exists replaces it in place.
        let mut changed = window(1);
        changed.is_floating = true;
        state.apply(Event::WindowOpenedOrChanged {
            window: changed.clone(),
        });
        assert_eq!(state.windows.windows.len(), 2);
        assert_eq!(state.windows.windows[0], changed);

        state.apply(Event::WindowFocusChanged { id: Some(2) });
        assert_eq!(
            state.focused_window().map(|window| return window.id),
            Some(2)
        );

        state.apply(Event::FullscreenChanged {
            id: 2,
            fullscreen: Some(FullscreenMode::Windowed),
        });
        assert_eq!(
            state.windows.window(2).unwrap().fullscreen,
            Some(FullscreenMode::Windowed)
        );

        state.apply(Event::WindowClosed { id: 1 });
        assert_eq!(state.windows.windows.len(), 1);
        assert!(state.windows.window(1).is_none());
    }

    #[test]
    fn ignores_events_about_unknown_objects() {
        // A status bar must survive events for things it never saw.
        let mut state = EventStreamState::default();
        state.apply(Event::FloatingChanged {
            id: 42,
            floating: true,
        });
        state.apply(Event::WindowFocusChanged { id: Some(42) });
        state.apply(Event::WindowClosed { id: 42 });
        state.apply(Event::WorkspaceActiveWindowChanged {
            output: "gone".into(),
            index: 7,
            active_window_id: Some(1),
        });
        state.apply(Event::OutputRemoved {
            name: "gone".into(),
        });
        state.apply(Event::LayoutChanged {
            output: Some("gone".into()),
            workspace_index: Some(3),
            name: "dwindle".into(),
        });
        assert!(state.windows.windows.is_empty());
        assert!(state.workspaces.workspaces.is_empty());
    }

    #[test]
    fn applies_workspace_events() {
        let mut state = EventStreamState::default();
        state.apply(Event::WorkspacesChanged {
            workspaces: vec![workspace(1, "winit", true), workspace(2, "winit", false)],
        });
        state.apply(Event::WorkspaceActiveWindowChanged {
            output: "winit".into(),
            index: 2,
            active_window_id: Some(9),
        });
        assert_eq!(
            state.workspace("winit", 2).unwrap().active_window_id,
            Some(9)
        );

        state.apply(Event::LayoutChanged {
            output: Some("winit".into()),
            workspace_index: Some(2),
            name: "dwindle".into(),
        });
        assert_eq!(state.workspace("winit", 1).unwrap().layout, "columns");
        assert_eq!(state.workspace("winit", 2).unwrap().layout, "dwindle");

        // Without an index the whole output switches layout.
        state.apply(Event::LayoutChanged {
            output: Some("winit".into()),
            workspace_index: None,
            name: "grid".into(),
        });
        assert_eq!(state.workspace("winit", 1).unwrap().layout, "grid");
        assert_eq!(state.workspace("winit", 2).unwrap().layout, "grid");
    }

    #[test]
    fn applies_output_events() {
        let mut state = EventStreamState::default();
        let output = Output {
            name: "winit".into(),
            make: "Poly".into(),
            model: "winit".into(),
            serial: None,
            physical_size: None,
            position: (0, 0),
            logical_size: (1280, 800),
            scale: 1.0,
            current_mode: None,
            modes: Vec::new(),
        };
        state.apply(Event::OutputAdded {
            output: output.clone(),
        });
        assert_eq!(state.outputs.outputs.len(), 1);

        let mut changed = output.clone();
        changed.scale = 2.0;
        state.apply(Event::OutputChanged { output: changed });
        assert_eq!(state.outputs.outputs["winit"].scale, 2.0);

        state.apply(Event::OutputsChanged {
            outputs: Vec::new(),
        });
        assert!(state.outputs.outputs.is_empty());

        state.apply(Event::OutputAdded { output });
        state.apply(Event::OutputRemoved {
            name: "winit".into(),
        });
        assert!(state.outputs.outputs.is_empty());
    }
}
