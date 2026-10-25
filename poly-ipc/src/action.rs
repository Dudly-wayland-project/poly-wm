//! The compositor action vocabulary.
//!
//! Keybindings (Lua), IPC requests and `poly msg action` all speak this single
//! set of actions (`AGENTS.md` §2.6). `FromStr` accepts the kebab-case spelling
//! used in configuration files; serde (used by the IPC protocol) serialises the
//! variants as they are named in Rust.

use serde::{Deserialize, Serialize};

use std::str::FromStr;

/// Something the compositor can be asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Exit the compositor.
    Quit,
    /// Run a command directly, without a shell: argv.
    Spawn {
        /// The program and its arguments.
        command: Vec<String>,
    },
    /// Run a command through `sh -c`.
    SpawnShell {
        /// The shell command line.
        command: String,
    },
    /// Ask the focused window to close.
    CloseWindow,
    /// Reload the configuration from disk.
    ReloadConfig,

    /// Cycle the focus to the next window.
    FocusNext,
    /// Cycle the focus to the previous window.
    FocusPrev,
    /// Focus the window to the left of the focused one.
    FocusLeft,
    /// Focus the window to the right of the focused one.
    FocusRight,
    /// Focus the window above the focused one.
    FocusUp,
    /// Focus the window below the focused one.
    FocusDown,
    /// Focus the window with this id, switching workspace and output as needed.
    ///
    /// Ids are the ones [`crate::Window::id`] reports; they only mean something
    /// while the compositor that handed them out is running, which is why this
    /// is a script action (`poly msg action focus-window 3`) rather than
    /// something worth writing into a configuration file.
    FocusWindow {
        /// Stable window id, as printed by [`Request::Windows`](crate::Request::Windows).
        id: u64,
    },

    /// Start an interactive move of the window under the pointer.
    MoveWindow,
    /// Start an interactive resize of the window under the pointer.
    ResizeWindow,
    /// Move the focused window towards a direction.
    MoveWindowLeft,
    /// Move the focused window towards a direction.
    MoveWindowRight,
    /// Move the focused window towards a direction.
    MoveWindowUp,
    /// Move the focused window towards a direction.
    MoveWindowDown,

    /// Toggle the focused window between tiled and floating.
    ToggleFloating,
    /// Toggle the focused window between windowed and fullscreen.
    ToggleFullscreen,
    /// Toggle the focused window between windowed and windowed-fullscreen.
    ToggleWindowedFullscreen,

    /// Switch the current output to a named layout.
    SetLayout {
        /// Layout name from the configuration.
        name: String,
    },
    /// Switch the current output to the next configured layout.
    CycleLayout,

    /// Focus the workspace above the active one (`docs/plan.md` §9.6).
    FocusWorkspaceUp,
    /// Focus the workspace below the active one. Creates one at the bottom.
    FocusWorkspaceDown,
    /// Focus the workspace with this 1-based index, creating it when needed.
    FocusWorkspace {
        /// Workspace number as written in the configuration (1 = first).
        index: usize,
    },
    /// Move the focused window to the workspace above and follow it.
    MoveWindowToWorkspaceUp,
    /// Move the focused window to the workspace below and follow it.
    MoveWindowToWorkspaceDown,
    /// Move the focused window to the workspace with this 1-based index.
    MoveWindowToWorkspace {
        /// Workspace number as written in the configuration (1 = first).
        index: usize,
    },
    /// Swap the active workspace with the one above it.
    MoveWorkspaceUp,
    /// Swap the active workspace with the one below it.
    MoveWorkspaceDown,
    /// Show or hide the workspace overview.
    ToggleOverview,
}

impl Action {
    /// The kebab-case name of this action, as spelled in configuration files.
    pub fn name(&self) -> &'static str {
        return match self {
            Action::Quit => "quit",
            Action::Spawn { .. } => "spawn",
            Action::SpawnShell { .. } => "spawn-shell",
            Action::CloseWindow => "close-window",
            Action::ReloadConfig => "reload-config",
            Action::FocusNext => "focus-next",
            Action::FocusPrev => "focus-prev",
            Action::FocusLeft => "focus-left",
            Action::FocusRight => "focus-right",
            Action::FocusUp => "focus-up",
            Action::FocusDown => "focus-down",
            Action::FocusWindow { .. } => "focus-window",
            Action::MoveWindow => "move-window",
            Action::ResizeWindow => "resize-window",
            Action::MoveWindowLeft => "move-window-left",
            Action::MoveWindowRight => "move-window-right",
            Action::MoveWindowUp => "move-window-up",
            Action::MoveWindowDown => "move-window-down",
            Action::ToggleFloating => "toggle-floating",
            Action::ToggleFullscreen => "toggle-fullscreen",
            Action::ToggleWindowedFullscreen => "toggle-windowed-fullscreen",
            Action::SetLayout { .. } => "set-layout",
            Action::CycleLayout => "cycle-layout",
            Action::FocusWorkspaceUp => "focus-workspace-up",
            Action::FocusWorkspaceDown => "focus-workspace-down",
            Action::FocusWorkspace { .. } => "focus-workspace",
            Action::MoveWindowToWorkspaceUp => "move-window-to-workspace-up",
            Action::MoveWindowToWorkspaceDown => "move-window-to-workspace-down",
            Action::MoveWindowToWorkspace { .. } => "move-window-to-workspace",
            Action::MoveWorkspaceUp => "move-workspace-up",
            Action::MoveWorkspaceDown => "move-workspace-down",
            Action::ToggleOverview => "toggle-overview",
        };
    }

    /// Parse the argument list of `poly msg action`: a name, then its
    /// arguments.
    ///
    /// ```
    /// # use poly_ipc::Action;
    /// let argv = ["spawn", "foot", "-e", "htop"].map(str::to_owned);
    /// assert_eq!(
    ///     Action::from_argv(&argv).unwrap(),
    ///     Action::Spawn {
    ///         command: vec!["foot".into(), "-e".into(), "htop".into()],
    ///     },
    /// );
    /// let argv = ["focus-workspace", "2"].map(str::to_owned);
    /// assert_eq!(
    ///     Action::from_argv(&argv).unwrap(),
    ///     Action::FocusWorkspace { index: 2 },
    /// );
    /// ```
    pub fn from_argv(argv: &[String]) -> Result<Self, InvalidAction> {
        let Some((name, rest)) = argv.split_first() else {
            return Err(InvalidAction {
                name: String::new(),
                hint: Some("expected an action name"),
            });
        };

        let action = match name.as_str() {
            "spawn" => Action::Spawn {
                command: match rest {
                    [] => {
                        return Err(InvalidAction {
                            name: name.clone(),
                            hint: Some("expected a command: action spawn foot -e htop"),
                        });
                    }
                    command => command.to_vec(),
                },
            },
            "spawn-shell" => Action::SpawnShell {
                command: match rest {
                    [] => {
                        return Err(InvalidAction {
                            name: name.clone(),
                            hint: Some(
                                "expected a command line: action spawn-shell \"foot -e htop\"",
                            ),
                        });
                    }
                    command => command.join(" "),
                },
            },
            "set-layout" => Action::SetLayout {
                name: one_arg(
                    name,
                    rest,
                    "expected a layout name: action set-layout dwindle",
                )?,
            },
            "focus-workspace" => Action::FocusWorkspace {
                index: index_arg(name, rest)?,
            },
            "move-window-to-workspace" => Action::MoveWindowToWorkspace {
                index: index_arg(name, rest)?,
            },
            "focus-window" => Action::FocusWindow {
                id: window_id_arg(name, rest)?,
            },
            _ => {
                if !rest.is_empty() {
                    return Err(InvalidAction {
                        name: name.clone(),
                        hint: Some("this action takes no arguments"),
                    });
                }
                return name.parse::<Action>();
            }
        };
        return Ok(action);
    }
}

/// The actions that can be spelled as a bare name, without arguments.
const ARGLESS_ACTIONS: &[Action] = &[
    Action::Quit,
    Action::CloseWindow,
    Action::ReloadConfig,
    Action::FocusNext,
    Action::FocusPrev,
    Action::FocusLeft,
    Action::FocusRight,
    Action::FocusUp,
    Action::FocusDown,
    Action::MoveWindow,
    Action::ResizeWindow,
    Action::MoveWindowLeft,
    Action::MoveWindowRight,
    Action::MoveWindowUp,
    Action::MoveWindowDown,
    Action::ToggleFloating,
    Action::ToggleFullscreen,
    Action::ToggleWindowedFullscreen,
    Action::CycleLayout,
    Action::FocusWorkspaceUp,
    Action::FocusWorkspaceDown,
    Action::MoveWindowToWorkspaceUp,
    Action::MoveWindowToWorkspaceDown,
    Action::MoveWorkspaceUp,
    Action::MoveWorkspaceDown,
    Action::ToggleOverview,
];

/// Exactly one argument, or an error hinting at the right spelling.
fn one_arg(name: &str, rest: &[String], hint: &'static str) -> Result<String, InvalidAction> {
    return match rest {
        [arg] => Ok(arg.clone()),
        _ => Err(InvalidAction {
            name: name.to_owned(),
            hint: Some(hint),
        }),
    };
}

/// A 1-based workspace number.
fn index_arg(name: &str, rest: &[String]) -> Result<usize, InvalidAction> {
    let arg = one_arg(
        name,
        rest,
        "expected a workspace number: action focus-workspace 2",
    )?;
    return match arg.parse::<usize>() {
        Ok(index) if index >= 1 => Ok(index),
        _ => Err(InvalidAction {
            name: name.to_owned(),
            hint: Some("workspaces are numbered from 1"),
        }),
    };
}

/// A window id, as printed by `poly msg windows`.
fn window_id_arg(name: &str, rest: &[String]) -> Result<u64, InvalidAction> {
    let arg = one_arg(
        name,
        rest,
        "expected a window id: action focus-window 3 (see `poly msg windows`)",
    )?;
    return match arg.parse::<u64>() {
        Ok(id) => Ok(id),
        Err(_) => Err(InvalidAction {
            name: name.to_owned(),
            hint: Some("window ids are numbers: action focus-window 3"),
        }),
    };
}

impl FromStr for Action {
    type Err = InvalidAction;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for action in ARGLESS_ACTIONS {
            if s == action.name() {
                return Ok(action.clone());
            }
        }

        // Actions that carry an argument cannot come from a bare string; the
        // Lua loader accepts tables for those (`{ spawn = { "foot" } }`).
        let err = match s {
            "spawn" => Some("use { spawn = { \"command\", \"arg\" } }"),
            "spawn-shell" => Some("use { spawn_shell = \"command line\" }"),
            "set-layout" => Some("use { set_layout = \"name\" }"),
            "focus-workspace" => Some("use { focus_workspace = 2 }"),
            "focus-window" => Some("use { focus_window = 3 }, or `poly msg action focus-window 3`"),
            "move-window-to-workspace" => Some("use { move_window_to_workspace = 2 }"),
            _ => None,
        };
        return Err(InvalidAction {
            name: s.to_owned(),
            hint: err,
        });
    }
}

/// The string is not a known action name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid action: {name}")]
pub struct InvalidAction {
    /// The offending string.
    pub name: String,
    /// A hint about the right spelling, when the action needs arguments.
    pub hint: Option<&'static str>,
}

/// The kebab-case names of every action that can be written as a bare string.
pub fn argless_action_names() -> Vec<&'static str> {
    return ARGLESS_ACTIONS
        .iter()
        .map(|action| return action.name())
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_argless_actions() {
        assert_eq!("quit".parse::<Action>().unwrap(), Action::Quit);
        assert_eq!(
            "toggle-floating".parse::<Action>().unwrap(),
            Action::ToggleFloating
        );
        assert_eq!("move-window".parse::<Action>().unwrap(), Action::MoveWindow);
    }

    #[test]
    fn rejects_unknown_and_argument_actions() {
        let err = "spawn".parse::<Action>().unwrap_err();
        assert!(err.hint.is_some(), "spawn should hint at the table form");
        let err = "frobnicate".parse::<Action>().unwrap_err();
        assert_eq!(err.name, "frobnicate");
        assert!(err.hint.is_none());
    }

    #[test]
    fn serde_roundtrip() {
        let action = Action::Spawn {
            command: vec!["foot".into(), "-e".into(), "htop".into()],
        };
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#"{"Spawn":{"command":["foot","-e","htop"]}}"#);
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);

        let json = serde_json::to_string(&Action::CloseWindow).unwrap();
        assert_eq!(json, r#""CloseWindow""#);
    }

    #[test]
    fn parses_the_workspace_actions() {
        assert_eq!(
            "focus-workspace-up".parse::<Action>().unwrap(),
            Action::FocusWorkspaceUp
        );
        assert_eq!(
            "move-window-to-workspace-down".parse::<Action>().unwrap(),
            Action::MoveWindowToWorkspaceDown
        );
        assert_eq!(
            "move-workspace-up".parse::<Action>().unwrap(),
            Action::MoveWorkspaceUp
        );
        assert_eq!(
            "toggle-overview".parse::<Action>().unwrap(),
            Action::ToggleOverview
        );
        let err = "focus-workspace".parse::<Action>().unwrap_err();
        assert!(err.hint.is_some(), "focus-workspace needs an index");
    }

    #[test]
    fn names_are_unique() {
        let mut names = argless_action_names();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate action names");
    }

    #[test]
    fn parses_argv() {
        let parse = |argv: &[&str]| {
            return Action::from_argv(
                &argv
                    .iter()
                    .map(|arg| return arg.to_string())
                    .collect::<Vec<_>>(),
            );
        };
        assert_eq!(parse(&["close-window"]).unwrap(), Action::CloseWindow);
        assert_eq!(
            parse(&["spawn", "foot", "-e", "htop"]).unwrap(),
            Action::Spawn {
                command: vec!["foot".into(), "-e".into(), "htop".into()],
            }
        );
        assert_eq!(
            parse(&["spawn-shell", "foot", "&&", "true"]).unwrap(),
            Action::SpawnShell {
                command: "foot && true".into(),
            }
        );
        assert_eq!(
            parse(&["set-layout", "dwindle"]).unwrap(),
            Action::SetLayout {
                name: "dwindle".into(),
            }
        );
        assert_eq!(
            parse(&["focus-workspace", "3"]).unwrap(),
            Action::FocusWorkspace { index: 3 }
        );
        assert_eq!(
            parse(&["move-window-to-workspace", "2"]).unwrap(),
            Action::MoveWindowToWorkspace { index: 2 }
        );
        assert_eq!(
            parse(&["focus-window", "3"]).unwrap(),
            Action::FocusWindow { id: 3 }
        );

        // Argument problems come back as hints, not silent no-ops.
        assert!(parse(&["spawn"]).unwrap_err().hint.is_some());
        assert!(parse(&["set-layout"]).unwrap_err().hint.is_some());
        assert!(parse(&["set-layout", "a", "b"]).unwrap_err().hint.is_some());
        assert!(parse(&["focus-workspace", "0"]).unwrap_err().hint.is_some());
        assert!(parse(&["focus-workspace", "x"]).unwrap_err().hint.is_some());
        assert!(parse(&["focus-window", "x"]).unwrap_err().hint.is_some());
        assert!(parse(&["focus-window"]).unwrap_err().hint.is_some());
        assert!(
            parse(&["close-window", "extra"])
                .unwrap_err()
                .hint
                .is_some()
        );
        assert!(parse(&["frobnicate"]).unwrap_err().hint.is_none());
        assert!(parse(&[]).unwrap_err().hint.is_some());
    }
}
