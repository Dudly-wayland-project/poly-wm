//! Output handler: M0 only needs the global bookkeeping that smithay provides.

use smithay::wayland::output::OutputHandler;

use crate::state::State;

impl OutputHandler for State {}
