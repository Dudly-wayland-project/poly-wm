// Ported from niri (GPL-3.0-or-later): src/niri.rs (lock/unlock), src/handlers/mod.rs
//
//! ext-session-lock: while locked, only the lock client's surfaces are shown
//! and only they receive input (`docs/plan.md` §7).

use std::mem;

use smithay::output::Output;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::utils::{Logical, Size};
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};
use tracing::{debug, error, info, warn};

use crate::core::LockState;
use crate::core::focus::update_keyboard_focus;
use crate::state::State;

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        return &mut self.core.session_lock_state;
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        match &self.core.lock_state {
            LockState::Unlocked => {}
            LockState::WaitingForSurfaces(_) => {
                info!("refusing lock: another client is already locking");
                return;
            }
            LockState::Locked(lock) => {
                if lock.is_alive() {
                    info!("refusing lock: already locked with an active client");
                    return;
                }
                // The previous lock client died; let the new one take over.
                info!("locking session (replacing a dead lock)");
            }
        }

        info!("locking session");
        // Drop surfaces of a previous, now dead lock client so the new client
        // starts from a clean slate.
        self.core
            .lock_surfaces
            .retain(|(_, surface)| return surface.alive());
        self.core.lock_rendered_outputs.clear();
        self.core.lock_state = LockState::WaitingForSurfaces(confirmation);

        // Blank the outputs right away; the lock surfaces follow.
        self.core.queue_redraw_all();
        update_keyboard_focus(self);

        // No outputs: nothing to wait for, confirm immediately.
        if self.core.space.outputs().next().is_none() {
            self.confirm_lock();
        }
    }

    fn unlock(&mut self) {
        info!("unlocking session");
        self.core.lock_state = LockState::Unlocked;
        self.core.lock_surfaces.clear();
        self.core.lock_rendered_outputs.clear();
        update_keyboard_focus(self);
        self.core.queue_redraw_all();
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            warn!("lock surface for an unknown output");
            return;
        };

        // Only the locking client may provide surfaces.
        let lock_client = match &self.core.lock_state {
            LockState::Unlocked => {
                error!("lock surface while the session is not locking");
                return;
            }
            LockState::WaitingForSurfaces(locker) => locker.ext_session_lock().client(),
            LockState::Locked(lock) => lock.client(),
        };
        if surface
            .wl_surface()
            .client()
            .map(|client| return client.id())
            != lock_client.map(|client| return client.id())
        {
            debug!("ignoring a lock surface from an unrelated client");
            return;
        }

        configure_lock_surface(&surface, &output);
        self.core
            .lock_surfaces
            .retain(|(lock_output, _)| return lock_output != &output);
        self.core.lock_surfaces.push((output.clone(), surface));
        debug!(output = %output.name(), "lock surface added");

        // The lock is confirmed by the redraw loop once this surface has a
        // buffer and has been presented (`LockState::WaitingForSurfaces`).
        self.core.queue_redraw(&output);
        update_keyboard_focus(self);
    }

    fn ack_configure(
        &mut self,
        _surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        _configure: smithay::wayland::session_lock::LockSurfaceConfigure,
    ) {
        // The client is expected to attach a buffer next; the next commit
        // repaints the output.
    }
}

impl State {
    /// Confirm the lock to the client that asked for it.
    pub(crate) fn confirm_lock(&mut self) {
        let state = mem::replace(&mut self.core.lock_state, LockState::Unlocked);
        if let LockState::WaitingForSurfaces(locker) = state {
            let lock_object = locker.ext_session_lock().clone();
            locker.lock();
            self.core.lock_state = LockState::Locked(lock_object);
            self.core.lock_rendered_outputs.clear();
            info!("session locked");
            self.core.queue_redraw_all();
        } else {
            self.core.lock_state = state;
        }
    }
}

/// Whether a lock surface has a buffer attached (its client drew the screen).
///
/// The `locked` event must only be sent once the lock client's content is
/// actually visible, so the redraw loop checks this before counting an output
/// as covered. [`on_commit_buffer_handler`] moves the buffer out of
/// `SurfaceAttributes` into the renderer surface state, so the buffer is
/// looked up there.
///
/// [`on_commit_buffer_handler`]: smithay::backend::renderer::utils::on_commit_buffer_handler
pub fn lock_surface_has_buffer(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> bool {
    use smithay::backend::renderer::utils::with_renderer_surface_state;

    return with_renderer_surface_state(surface, |state| return state.buffer().is_some())
        .unwrap_or(false);
}

/// Drop lock objects/ surfaces whose client has gone away.
///
/// A dead lock *client* must not unlock the session (that is the whole point),
/// but a dead pending request must not keep the session blank forever either.
/// Lock surfaces of a dead client are removed so that the next frame shows the
/// lock background instead of stale content.
pub fn refresh_lock_state(state: &mut State) {
    let abort_pending = match &state.core.lock_state {
        LockState::WaitingForSurfaces(locker) => !locker.ext_session_lock().is_alive(),
        _ => false,
    };
    if abort_pending {
        info!("abandoning the lock request: the client went away");
        state.core.lock_state = LockState::Unlocked;
        state.core.lock_surfaces.clear();
        state.core.lock_rendered_outputs.clear();
        state.core.queue_redraw_all();
        return;
    }

    let locked = matches!(state.core.lock_state, LockState::Locked(_));
    if locked {
        let before = state.core.lock_surfaces.len();
        state
            .core
            .lock_surfaces
            .retain(|(_, surface)| return surface.alive());
        if state.core.lock_surfaces.len() != before {
            debug!("lock surfaces removed: the lock client went away");
            // Stay locked: the session content must not reappear, but repaint
            // so that the lock background replaces the dead client's frame.
            state.core.queue_redraw_all();
        }
    }
}

/// Tell a lock surface how big it should be.
fn configure_lock_surface(surface: &LockSurface, output: &Output) {
    let scale = output.current_scale().fractional_scale();
    let size = output
        .current_mode()
        .map(|mode| -> Size<i32, Logical> {
            return mode.size.to_f64().to_logical(scale).to_i32_round();
        })
        .map(|size| {
            return Size::<u32, Logical>::from((size.w.max(0) as u32, size.h.max(0) as u32));
        });

    surface.with_pending_state(|state| {
        state.size = size;
    });
    surface.send_configure();
}

/// Re-send the size of every lock surface.
///
/// A lock surface is configured once, when it is created. If the output changes
/// mode or scale while the session is locked (`poly msg output … scale`), the
/// lock screen would keep the old size until the client is reconfigured.
pub fn reconfigure_lock_surfaces(state: &mut State) {
    let surfaces = state
        .core
        .lock_surfaces
        .iter()
        .map(|(output, surface)| return (output.clone(), surface.clone()))
        .collect::<Vec<_>>();
    for (output, surface) in surfaces {
        configure_lock_surface(&surface, &output);
    }
}
