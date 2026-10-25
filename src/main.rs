//! Poly — a Wayland compositor with Lua-scriptable layouts.

// Project convention: every function body ends with an explicit `return`,
// see rust-systems-dev coding-guidelines §2.9.
#![allow(clippy::needless_return)]

mod backend;
mod cli;
mod core;
mod state;
mod utils;

use std::process::ExitCode;

fn main() -> ExitCode {
    return cli::main();
}
