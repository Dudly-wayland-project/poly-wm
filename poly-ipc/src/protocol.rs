//! The wire protocol: requests, replies and events (`docs/ipc.md`).
//!
//! Every message is one JSON object on one line. A request is answered with
//! exactly one [`Reply`]; a connection that asked for [`Request::EventStream`]
//! then receives [`Event`]s, one per line, until it goes away.
//!
//! # Backwards compatibility
//!
//! Requests, responses and events may gain new variants and new fields in a
//! patch release. Clients must ignore what they do not know: fields are only
//! ever added as optional (or with `#[serde(default)]`), and variants are only
//! ever appended.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::Action;

/// A request from a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Ask for the compositor version.
    Version,
    /// Ask for the connected outputs.
    Outputs,
    /// Ask for the whole workspace stack of every output.
    Workspaces,
    /// Ask for the managed windows.
    Windows,
    /// Ask for the window that currently holds keyboard focus.
    FocusedWindow,
    /// Ask for the output that currently has keyboard focus.
    FocusedOutput,
    /// Ask for the state of the XWayland integration (`xwayland-satellite`).
    XwaylandStatus,
    /// Run an action from the shared vocabulary (`poly-ipc::Action`).
    Action(Action),
    /// Re-read the configuration from disk.
    ReloadConfig,
    /// Change an output at runtime (scale, mode, position).
    ///
    /// The change is temporary: it is not written back to the configuration
    /// file, and a later `reload-config` may replace it.
    OutputConfig {
        /// Output name, as reported by [`Request::Outputs`].
        output: String,
        /// What to change.
        action: OutputAction,
    },
    /// Switch a workspace to a named layout.
    LayoutSwitch {
        /// Output name; the focused output when omitted.
        output: Option<String>,
        /// Layout name from the configuration.
        name: String,
    },
    /// Start receiving events.
    ///
    /// The reply is [`Response::Handled`], followed by a full snapshot of the
    /// current state (one [`Event::WorkspacesChanged`], [`Event::WindowsChanged`],
    /// [`Event::OutputsChanged`], …), and then by one event per change. No
    /// further requests are read from this connection.
    EventStream,
}

/// What [`Request::OutputConfig`] changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OutputAction {
    /// Set the fractional scale (`1.0`, `1.5`, `2.0`, …).
    Scale(f64),
    /// Switch to one of the modes the output reports in [`Output::modes`].
    ///
    /// `refresh` disambiguates modes that share a size; when omitted, the
    /// output's preferred mode of that size is used.
    Mode {
        /// Pixels.
        width: u16,
        /// Pixels.
        height: u16,
        /// Refresh rate in millihertz; any mode of that size when omitted.
        refresh: Option<u32>,
    },
    /// Move the output to a compositor-space position.
    Position {
        /// Horizontal position in logical pixels.
        x: i32,
        /// Vertical position in logical pixels.
        y: i32,
    },
}

/// A successful reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    /// A request that needs no data was handled.
    Handled,
    /// Compositor version string.
    Version(String),
    /// Connected outputs, keyed by output name.
    Outputs(HashMap<String, Output>),
    /// The workspace stack of every output, in output order.
    Workspaces(Vec<Workspace>),
    /// The managed windows, back to front per output.
    Windows(Vec<Window>),
    /// The focused window, or `None` when no window has the keyboard.
    FocusedWindow(Option<Window>),
    /// The name of the focused output, or `None` when there is no output.
    FocusedOutput(Option<String>),
    /// Outcome of [`Request::OutputConfig`].
    OutputConfigChanged(OutputConfigChanged),
    /// The XWayland integration, for [`Request::XwaylandStatus`].
    XwaylandStatus(XwaylandStatus),
}

/// The state of the XWayland integration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XwaylandStatus {
    /// Whether X11 is set up: the sockets exist and children get `DISPLAY`.
    ///
    /// `false` also when the configuration asks for it but the setup failed
    /// (`poly` reports the reason in its log and on screen).
    pub enabled: bool,
    /// Whether the configuration asked for X11 at all, i.e. whether `enabled`
    /// being `false` means "switched off" or "failed to start".
    #[serde(default)]
    pub configured: bool,
    /// The `DISPLAY` value children are spawned with, when X11 is set up.
    #[serde(default)]
    pub display: Option<String>,
    /// Whether an `xwayland-satellite` process is running right now.
    pub running: bool,
    /// Whether the satellite is started by the first X11 client (instead of
    /// running from startup on).
    ///
    /// The effective value: a satellite configured to keep running that keeps
    /// failing to start falls back to on-demand activation and reports `true`.
    pub on_demand: bool,
    /// The program that is started.
    pub satellite_path: String,
}

/// What happened to a runtime output change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputConfigChanged {
    /// The change was applied.
    Applied,
    /// No output with that name exists.
    OutputWasMissing,
    /// The output exists but cannot do that (unknown mode, held by the DRM
    /// backend, …); the details are in the log.
    Rejected,
}

/// What the compositor sends back for each request: `{"Ok": …}` or `{"Err": "…"}`.
pub type Reply = Result<Response, String>;

/// One connected output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Output {
    /// Output name (`winit`, `headless`, `eDP-1`, …).
    pub name: String,
    /// Monitor vendor, as reported by the backend.
    pub make: String,
    /// Monitor model, as reported by the backend.
    pub model: String,
    /// Serial number, when the backend reports one.
    #[serde(default)]
    pub serial: Option<String>,
    /// Physical size in millimetres, when known.
    #[serde(default)]
    pub physical_size: Option<(i32, i32)>,
    /// Position in compositor coordinates.
    pub position: (i32, i32),
    /// Logical size in compositor coordinates (mode size ÷ scale).
    pub logical_size: (i32, i32),
    /// Fractional scale.
    pub scale: f64,
    /// The mode currently driving the output.
    pub current_mode: Option<Mode>,
    /// Every mode the output supports, preferred first.
    pub modes: Vec<Mode>,
}

/// One display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mode {
    /// Pixels.
    pub width: u16,
    /// Pixels.
    pub height: u16,
    /// Refresh rate in millihertz (`60000` = 60 Hz).
    pub refresh: u32,
}

/// One managed window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Stable id, unique for the lifetime of the compositor process.
    pub id: u64,
    /// `xdg_toplevel.set_title`.
    #[serde(default)]
    pub title: Option<String>,
    /// `xdg_toplevel.set_app_id`.
    #[serde(default)]
    pub app_id: Option<String>,
    /// Whether the window holds the keyboard focus.
    pub is_focused: bool,
    /// Whether the window floats above the layout.
    pub is_floating: bool,
    /// Fullscreen mode, when the window is fullscreen.
    #[serde(default)]
    pub fullscreen: Option<FullscreenMode>,
    /// Output the window is on.
    #[serde(default)]
    pub output: Option<String>,
    /// 1-based workspace number inside that output's stack.
    #[serde(default)]
    pub workspace: Option<usize>,
}

/// How a window is fullscreen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FullscreenMode {
    /// The compositor gave the window the whole output.
    Fullscreen,
    /// The client thinks it is fullscreen; the layout still places it.
    Windowed,
}

/// One workspace of one output.
///
/// Workspaces have no stable ids: they are identified by the output they live
/// on plus their 1-based index in that output's vertical stack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// 1-based index in the output's stack.
    pub index: usize,
    /// Output name.
    #[serde(default)]
    pub output: Option<String>,
    /// Whether this workspace owns its output's screen.
    pub is_active: bool,
    /// Whether this workspace also has the keyboard focus (it is the active
    /// workspace of the focused output).
    pub is_focused: bool,
    /// The window the workspace would focus when entered.
    #[serde(default)]
    pub active_window_id: Option<u64>,
    /// Name of the layout arranging this workspace.
    pub layout: String,
}

/// Something that happened in the compositor, for [`Request::EventStream`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// The full workspace state; the first event of a stream, and after any
    /// change that cannot be expressed with a narrower event.
    WorkspacesChanged {
        /// Every workspace of every output.
        workspaces: Vec<Workspace>,
    },
    /// A workspace changed which window it would focus.
    WorkspaceActiveWindowChanged {
        /// Output name.
        output: String,
        /// 1-based index.
        index: usize,
        /// The new window, if any.
        active_window_id: Option<u64>,
    },
    /// The full window state; the first window event of a stream.
    WindowsChanged {
        /// Every managed window.
        windows: Vec<Window>,
    },
    /// A window was mapped, or one of its properties changed.
    WindowOpenedOrChanged {
        /// The window.
        window: Window,
    },
    /// A window is gone.
    WindowClosed {
        /// Its id.
        id: u64,
    },
    /// The keyboard focus moved.
    WindowFocusChanged {
        /// The newly focused window, or `None`.
        id: Option<u64>,
    },
    /// A window started or stopped floating.
    FloatingChanged {
        /// The window.
        id: u64,
        /// Whether it floats now.
        floating: bool,
    },
    /// A window changed fullscreen mode.
    FullscreenChanged {
        /// The window.
        id: u64,
        /// The new mode, or `None` when it is windowed again.
        fullscreen: Option<FullscreenMode>,
    },
    /// A workspace switched to another layout.
    LayoutChanged {
        /// Output name.
        output: Option<String>,
        /// 1-based workspace index, when the switch was per workspace.
        #[serde(default)]
        workspace_index: Option<usize>,
        /// The layout now in use.
        name: String,
    },
    /// The full output state; the first output event of a stream.
    OutputsChanged {
        /// Every connected output.
        outputs: Vec<Output>,
    },
    /// An output was enabled.
    OutputAdded {
        /// The output.
        output: Output,
    },
    /// An output went away.
    OutputRemoved {
        /// Its name.
        name: String,
    },
    /// An output changed scale, mode or position.
    OutputChanged {
        /// The output.
        output: Output,
    },
    /// The workspace overview opened or closed.
    OverviewOpenedOrClosed {
        /// Whether it is open.
        is_open: bool,
    },
    /// A configuration load finished, at startup or on `reload-config`.
    ConfigLoaded {
        /// Whether the file had errors (the previous configuration then stays
        /// in effect).
        failed: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip() {
        let line = serde_json::to_string(&Request::Version).unwrap();
        assert_eq!(line, r#""Version""#);
        let reply: Reply = Ok(Response::Version("0.1.0".into()));
        let line = serde_json::to_string(&reply).unwrap();
        assert_eq!(line, r#"{"Ok":{"Version":"0.1.0"}}"#);
        let back: Reply = serde_json::from_str(&line).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn requests_roundtrip() {
        for request in [
            Request::Outputs,
            Request::Windows,
            Request::FocusedWindow,
            Request::FocusedOutput,
            Request::EventStream,
            Request::ReloadConfig,
            Request::XwaylandStatus,
            Request::Action(Action::ToggleOverview),
            Request::Action(Action::Spawn {
                command: vec!["foot".into()],
            }),
            Request::LayoutSwitch {
                output: Some("eDP-1".into()),
                name: "dwindle".into(),
            },
            Request::OutputConfig {
                output: "eDP-1".into(),
                action: OutputAction::Scale(1.5),
            },
            Request::OutputConfig {
                output: "eDP-1".into(),
                action: OutputAction::Mode {
                    width: 1920,
                    height: 1080,
                    refresh: Some(60_000),
                },
            },
            Request::OutputConfig {
                output: "DP-2".into(),
                action: OutputAction::Position { x: 1920, y: 0 },
            },
        ] {
            let line = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        }
    }

    #[test]
    fn xwayland_status_roundtrips() {
        let reply: Reply = Ok(Response::XwaylandStatus(XwaylandStatus {
            enabled: true,
            configured: true,
            display: Some(":0".into()),
            running: false,
            on_demand: true,
            satellite_path: "xwayland-satellite".into(),
        }));
        let line = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Reply>(&line).unwrap(), reply);

        // An older compositor knows neither the field nor the variant; a newer
        // one must not insist on the field either.
        let line = r#"{"enabled":false,"running":false,"on_demand":true,"satellite_path":"x"}"#;
        let status = serde_json::from_str::<XwaylandStatus>(line).unwrap();
        assert_eq!(status.display, None);
    }

    #[test]
    fn events_roundtrip() {
        let workspace = Workspace {
            index: 1,
            output: Some("winit".into()),
            is_active: true,
            is_focused: true,
            active_window_id: Some(3),
            layout: "columns".into(),
        };
        let window = Window {
            id: 3,
            title: Some("foot".into()),
            app_id: Some("foot".into()),
            is_focused: true,
            is_floating: false,
            fullscreen: None,
            output: Some("winit".into()),
            workspace: Some(1),
        };
        for event in [
            Event::WorkspacesChanged {
                workspaces: vec![workspace.clone()],
            },
            Event::WindowsChanged {
                windows: vec![window.clone()],
            },
            Event::WindowOpenedOrChanged {
                window: window.clone(),
            },
            Event::WindowFocusChanged { id: Some(3) },
            Event::FloatingChanged {
                id: 3,
                floating: true,
            },
            Event::FullscreenChanged {
                id: 3,
                fullscreen: Some(FullscreenMode::Windowed),
            },
            Event::LayoutChanged {
                output: Some("winit".into()),
                workspace_index: Some(1),
                name: "columns".into(),
            },
            Event::OverviewOpenedOrClosed { is_open: true },
            Event::ConfigLoaded { failed: false },
        ] {
            let line = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), event);
        }
    }

    #[test]
    fn optional_fields_stay_omittable() {
        // An older compositor sends a window without the newer fields.
        let line = r#"{"id":1,"is_focused":false,"is_floating":true}"#;
        let window = serde_json::from_str::<Window>(line).unwrap();
        assert_eq!(window.title, None);
        assert_eq!(window.workspace, None);
        let line = r#"{"WindowOpenedOrChanged":{"window":{"id":1,"is_focused":false,"is_floating":false}}}"#;
        assert!(serde_json::from_str::<Event>(line).is_ok());
    }
}
