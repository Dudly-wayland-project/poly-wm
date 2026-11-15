//! Poly configuration: JavaScript → Rust types, validation and error
//! collection.
//!
//! The loader executes the configuration script with QuickJS and produces a
//! [`Config`]; problems are collected with field paths instead of failing fast.

// Project convention: every function body ends with an explicit `return`,
// see rust-systems-dev coding-guidelines §2.9.
#![allow(clippy::needless_return)]

mod binds;
mod config;
mod default_config;
mod error;
pub mod glob;
mod js;
mod locate;

pub use binds::{Key, KeyParseError, Keybind, Modifiers, Trigger};
pub use config::{
    AccelProfile, AnimationCurve, AnimationOverride, AnimationSpec, Animations, Blur, BlurShader,
    Border, ClickMethod, Color, Config, Cursor, Device, Errors, Fullscreen, General, Input,
    Keyboard, LayerInteractivity, LayerRule, LayoutInfo, LayoutOptionValue, ModKey, OutputConfig,
    OutputMode, OutputPosition, OutputScale, OutputTransform, ResizePick, ScrollMethod, Shadow,
    StartupSpawn, TiledDrag, WindowDrag, WindowRule, Xwayland, resolved_output,
};
pub use default_config::{DEFAULT_CONFIG, write_default};
pub use error::{ConfigError, ConfigMessage, LocateError};
pub use glob::{device_matches, glob_matches};
pub use js::layout::{
    LayoutContext, LayoutEffects, LayoutError, LayoutOutput, LayoutPrograms, LayoutRect,
    LayoutWindow, LayoutWorkspace,
};
pub use js::{ConfigLoadResult, load, load_file};
pub use locate::{ConfigSource, locate_config};
