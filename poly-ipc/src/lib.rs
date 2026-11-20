//! Poly IPC: protocol types, event-stream state and a blocking client.
//!
//! Transport is a Unix socket (`$POLY_SOCKET`), one JSON message per line
//! (`docs/ipc.md`). A request is answered with exactly one [`Reply`];
//! [`Request::EventStream`] then turns the connection into a stream of
//! [`Event`]s.
//!
//! The compositor side of the socket lives in the compositor binary
//! (`src/core/ipc`), but the wire format and the state diffing are defined
//! here so that both sides share one definition.

// Project convention: every function body ends with an explicit `return`,
// see rust-systems-dev coding-guidelines §2.9.
#![allow(clippy::needless_return)]

mod action;
mod protocol;
mod socket;
mod state;

pub use action::{Action, InvalidAction, argless_action_names};
pub use protocol::{
    CaptureMetrics, Event, FullscreenMode, Metrics, Mode, Output, OutputAction,
    OutputConfigChanged, OutputMetrics, Reply, Request, Response, Window, Workspace,
    XwaylandStatus,
};
pub use socket::{IpcError, SOCKET_ENV, Socket, socket_path};
pub use state::{
    ConfigState, EventStreamState, EventStreamStatePart, OutputsState, WindowsState,
    WorkspacesState,
};
