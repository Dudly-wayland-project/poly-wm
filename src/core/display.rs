// Ported from smallvil (MIT) / niri (GPL-3.0-or-later): wayland display setup

//! The wayland display: listening socket, client dispatch and per-client data.

use std::ffi::OsString;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::wayland::compositor::CompositorClientState;
use smithay::wayland::socket::ListeningSocketSource;
use tracing::{debug, error, warn};

use crate::state::State;

/// Data attached to every connected wayland client.
#[derive(Default)]
pub struct ClientState {
    /// Per-client compositor state (buffer bookkeeping).
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        debug!(?client_id, "client connected");
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        debug!(?client_id, ?reason, "client disconnected");
    }
}

/// Create the wayland socket and hook the display into the event loop.
///
/// Returns the socket name clients should connect to.
pub fn init(
    event_loop: &EventLoop<'static, State>,
    display: Display<State>,
    display_handle: &DisplayHandle,
) -> anyhow::Result<OsString> {
    let listening_socket =
        ListeningSocketSource::new_auto().context("error creating the wayland socket")?;
    let socket_name = listening_socket.socket_name().to_os_string();
    debug!("listening on wayland socket {:?}", socket_name);

    event_loop
        .handle()
        .insert_source(listening_socket, |stream, _, state| {
            let client_state = Arc::new(ClientState::default());
            if let Err(err) = state
                .core
                .display_handle
                .insert_client(stream, client_state)
            {
                warn!("error inserting wayland client: {err}");
            }
        })
        .map_err(|err| return anyhow!("error inserting socket source: {}", err.error))?;

    let _ = display_handle;
    event_loop
        .handle()
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // SAFETY: this source owns the display and is dropped with the
                // event loop, so the reference stays valid for its lifetime.
                let display = unsafe { display.get_mut() };
                match display.dispatch_clients(state) {
                    Ok(_) => (),
                    Err(err) => error!("error dispatching wayland clients: {err}"),
                }
                return Ok(PostAction::Continue);
            },
        )
        .map_err(|err| return anyhow!("error inserting display source: {}", err.error))?;

    return Ok(socket_name);
}
