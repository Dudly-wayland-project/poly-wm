//! Output handler: Smithay provides the global bookkeeping.

use smithay::wayland::output::OutputHandler;

use crate::state::State;

impl OutputHandler for State {}
