//! The IPC server.
//!
//! One Unix socket per compositor, JSON objects one per line. Requests are
//! answered one-for-one; a connection that asks for [`poly_ipc::Request::EventStream`]
//! receives the whole compositor state and then one event per change.
//!
//! The wire format and the state diffing live in the `poly-ipc` crate, so the
//! compositor and the clients (`poly msg`, status bars) share one definition of
//! what a workspace or a window looks like.

pub mod events;
mod server;

use poly_ipc::Event;
pub use poly_ipc::socket_path;
pub use server::IpcServer;

use crate::state::State;

/// Send the events that describe what changed since the last call.
///
/// Called from the event loop after the focus bookkeeping, so that a focus change
/// reaches clients in the same frame it happened. Cheap when nobody listens: the
/// snapshot is not even built.
pub fn refresh(state: &mut State) {
    if !state.core.ipc.has_stream() {
        return;
    }
    let events = events::diff(state);
    server::send(state, &events);
}

/// Tell the clients how a configuration load went.
///
/// This is a notification, not a diff: it is sent on every load, even when the
/// outcome is the same as the last one, so that a status bar can show "reloaded".
pub fn config_loaded(state: &mut State, failed: bool) {
    state.core.ipc.events.config.failed = failed;
    server::send(state, &[Event::ConfigLoaded { failed }]);
}
