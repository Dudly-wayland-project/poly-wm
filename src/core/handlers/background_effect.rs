//! `ext-background-effect-manager-v1`.
//!
//! A client that draws translucent content (a terminal with a translucent
//! background, a launcher overlay) asks the compositor to blur what is *behind*
//! the window through this protocol. The protocol layer lives in Smithay
//! (`smithay::wayland::background_effect`): it creates the manager and surface
//! objects, validates that a surface has at most one effect object, and stores
//! the requested region in a double-buffered
//! [`BackgroundEffectSurfaceCachedState`] on the surface.
//!
//! Nothing is rendered from here. The state is read at frame time by
//! [`crate::core::render::push_window`] and
//! [`crate::core::render::push_layers`], which turn the region into the geometry
//! of a [`crate::core::render::blur`] element. That keeps the layout/render
//! split intact: a protocol handler never reaches into the element list.
//!
//! `general.blur.accept_client_regions` decides whether the region is honoured.
//! The global stays advertised either way, because removing a global mid-session
//! breaks clients that cached it (AGENTS.md §2.10).
//!
//! # "No blur" versus "never asked"
//!
//! The protocol has two ways to say *no blur*: never creating an effect object,
//! and calling `set_blur_region` with a null region. Smithay stores both as
//! `blur_region == None`, which loses the difference. Shells rely on the second
//! form — Noctalia sends a null region whenever a panel goes fully opaque — so
//! treating `None` as "the client never asked" would blur a panel that had
//! explicitly opted out. [`BlurRequested`] records that a request happened at
//! all, which is all the renderer needs to tell the two apart.
//!
//! [`BackgroundEffectSurfaceCachedState`]: smithay::wayland::background_effect::BackgroundEffectSurfaceCachedState

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::background_effect::{Capability, ExtBackgroundEffectHandler};
use smithay::wayland::compositor::{RegionAttributes, with_states};

use crate::state::State;

/// Whether a surface ever carried a blur request.
///
/// Monotone by design: a client that clears its region and later sets it again
/// is "asked" either way, and the region itself lives in the protocol's own
/// cached state. Stored in the surface's data map, so it disappears with the
/// surface.
#[derive(Debug, Default)]
pub struct BlurRequested(std::sync::Mutex<bool>);

impl BlurRequested {
    fn set(&self) {
        // A panic while holding this lock would only ever be a bug in this
        // module, and the flag is advisory: recover the guard instead of
        // poisoning every later frame (AGENTS.md §2.11).
        let mut requested = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *requested = true;
    }

    fn get(&self) -> bool {
        let requested = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        return *requested;
    }
}

/// Record on `surface` that its client said something about blur.
fn stamp(surface: &WlSurface) {
    with_states(surface, |states| {
        states
            .data_map
            .get_or_insert_threadsafe(BlurRequested::default)
            .set();
    });
}

/// Whether the client ever set or cleared a blur region on `surface`.
///
/// `false` means the surface never mentioned blur, so the compositor's own
/// whole-window backdrop applies. `true` means the protocol's cached
/// `blur_region` is the client's answer, including `None` for "blur nothing".
pub fn blur_requested(surface: &WlSurface) -> bool {
    return with_states(surface, |states| {
        return states
            .data_map
            .get::<BlurRequested>()
            .is_some_and(BlurRequested::get);
    });
}

impl ExtBackgroundEffectHandler for State {
    fn capabilities(&self) -> Capability {
        return Capability::Blur;
    }

    // Both hooks only stamp the surface. The region itself is committed through
    // the protocol's own cached state, so acting on the pending value here would
    // read a region the client may still change before committing; the commit
    // hook in `handlers/compositor.rs` already queues the redraw that makes the
    // change visible.
    fn set_blur_region(&mut self, wl_surface: WlSurface, region: RegionAttributes) {
        let _ = region;
        stamp(&wl_surface);
    }

    fn unset_blur_region(&mut self, wl_surface: WlSurface) {
        stamp(&wl_surface);
    }
}
