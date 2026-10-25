// Ported from niri (GPL-3.0-or-later): src/handlers/compositor.rs

use std::sync::OnceLock;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::PopupKind;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_surface::WlSurface};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, add_destruction_hook,
    is_sync_subsurface,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use tracing::{error, warn};

use crate::core::display::ClientState;
use crate::core::window::{
    is_mapped, on_root_commit, on_surface_destroyed, root_surface, window_for_root,
};
use crate::state::State;

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        return &mut self.core.compositor_state;
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Every client gets a ClientState when we accept its connection.
        let Some(client_state) = client.get_data::<ClientState>() else {
            error!("client without ClientState; using a fallback");
            return fallback_compositor_state();
        };
        return &client_state.compositor_state;
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        // Installed before the first commit, so it runs before Smithay's
        // renderer-state reset hook during destruction. See
        // `on_surface_destroyed`.
        add_destruction_hook::<Self, _>(surface, |state, surface| {
            on_surface_destroyed(state, surface);
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // Track the root while the parent link is still valid. Destruction
        // hooks run after Smithay has orphaned a subsurface, so the table is
        // the only reliable way to find the window there.
        let root = root_surface(surface);
        self.core.layout.remember_surface_root(surface, &root);

        // Popups must be tracked for every committing surface, subsurfaces included.
        self.core.popups.commit(surface);

        // A new xdg_popup cannot map before it has seen its initial configure:
        // the client may not attach a buffer until it acknowledged one. Smithay
        // only computes the geometry (`unconstrain_popup` sets it); sending the
        // configure is the compositor's job. Without this every menu, tooltip
        // and combobox popup stays invisible forever.
        if let Some(PopupKind::Xdg(popup)) = self.core.popups.find_popup(surface) {
            if !popup.is_initial_configure_sent() {
                if let Err(err) = popup.send_configure() {
                    warn!("failed to send the initial popup configure: {err:?}");
                }
            }
        }

        if is_sync_subsurface(surface) {
            return;
        }

        if &root != surface {
            // A subsurface commit only refreshes the contents of its window.
            if let Some(window) = window_for_root(self, surface) {
                window.on_commit();
            }
        } else {
            on_root_commit(self, &root);
            // Layer surfaces have their own initial-configure handshake and
            // map/unmap lifecycle (`wlr-layer-shell`).
            crate::core::handlers::layer_shell::on_layer_commit(self, &root);
        }

        // Any commit while the window is still mapped makes a snapshot taken
        // from an earlier surface destruction stale. A null-buffer commit has
        // already unmapped the root and consumed its snapshot.
        if is_mapped(&root) {
            self.core.layout.clear_pending_unmap_closing(&root);
        }

        // A commit can change what is on screen (a new buffer, damage, a cursor
        // image, a toplevel state). Repaint; outputs whose damage tracker finds
        // that nothing changed are not presented.
        self.core.queue_redraw_all();
    }
}

fn fallback_compositor_state() -> &'static CompositorClientState {
    static FALLBACK: OnceLock<CompositorClientState> = OnceLock::new();
    return FALLBACK.get_or_init(CompositorClientState::default);
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        return &self.core.shm_state;
    }
}
