// Ported from niri (GPL-3.0-or-later): src/handlers/dmabuf.rs

//! dmabuf imports.
//!
//! Rendering imports dmabufs lazily (imports are cached per buffer), so there
//! is nothing to validate when a client announces one.

use smithay::backend::allocator::Buffer as _;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use tracing::trace;

use crate::state::State;

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        return &mut self.core.dmabuf_state;
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Accept the buffer: the render path imports it into the primary
        // renderer when it is first drawn, and a buffer we cannot import simply
        // fails to render.
        trace!(
            "client offered a dmabuf ({:?}, {}x{})",
            dmabuf.format().code,
            dmabuf.width(),
            dmabuf.height()
        );
        let _ = notifier.successful::<State>();
    }
}
