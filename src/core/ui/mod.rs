//! Compositor-drawn UI overlays.
//!
//! The only overlay so far is the Toast stack that reports configuration,
//! layout and shader problems to the user.

pub mod toast;

pub use toast::Toasts;
