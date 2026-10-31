//! Protocol handler implementations.
//!
//! Every handler is implemented on [`State`], the single type registered with
//! wayland-server through [`smithay::delegate_dispatch2!`] (AGENTS.md §2.3).

pub(crate) mod background_effect;
mod compositor;
mod dmabuf;
mod image_copy_capture;
pub mod ime;
pub mod layer_shell;
pub(crate) mod misc;
mod output;
pub(crate) mod screencopy;
mod seat;
pub mod session_lock;
mod xdg_shell;

pub(crate) use image_copy_capture::{refresh_cursor_sessions, refresh_sessions};

use smithay::delegate_dispatch2;

use crate::state::State;

delegate_dispatch2!(State);
