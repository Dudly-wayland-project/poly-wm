# AGENTS.md

This guide is for people and coding agents working on Poly. It describes how the
repository is put together, which conventions are load-bearing, and how to
validate a change. The section numbers are referenced from source comments, so
keep them stable when editing this file.

## 1. Source of truth

- Read the surrounding module before changing it. Most subsystems have a module
  comment explaining the invariants and the niri/Smithay code they were adapted
  from.
- `poly-ipc/src/action.rs` is the only action vocabulary. Lua keybindings, IPC
  and `poly msg action` all derive from it.
- `poly-ipc/src/protocol.rs` is the wire format. `docs/ipc.md` must describe
  what that file actually serializes.
- `poly-config/src/config.rs` defines the configuration data model;
  `poly-config/src/lua.rs` and `poly-config/src/layout.rs` parse it.
  `docs/config.md` must stay in sync with both.
- Some old source comments cite `docs/plan.md` for historical design notes.
  That file is not part of the current checkout. The maintained documentation
  is `docs/config.md`, `docs/ipc.md`, `docs/building.md`, `docs/installing.md`
  and this guide.

## 2. Engineering conventions

### 2.1 Preserve the architecture

Poly has one compositor process and one event loop. `State` owns the backend,
`Core` and capture state. `Core` owns the Smithay `Space`, protocol globals,
layout, input seat, renderer caches, IPC server and configuration. Backends
translate native events into calls on `Core` and present the `Space`; they do not
own window policy.

Put behavior in the layer that already owns the relevant state. Do not add a
second window list, workspace model, action dispatcher or configuration parser.

### 2.2 Dependencies and reproducible builds

- Smithay and `smithay-drm-extras` are pinned to a full Git revision in the
  workspace `Cargo.toml`. Update both together, refresh `Cargo.lock`, and run
  the full validation suite.
- Prefer workspace dependencies. Add a crate only when the standard library or
  an existing dependency cannot do the job cleanly.
- Keep optional integrations behind a Cargo feature (`dbus` is the current
  example) and make the featureless build still compile and test.
- Do not vendor or patch dependencies just to make a local build pass.

### 2.3 One `State`, one dispatch

Wayland protocol handlers are implemented on `State`. The single
`smithay::delegate_dispatch2!(State)` registration in `src/core/handlers.rs`
routes every global to that type. Add new handler implementations as modules
next to the existing ones and register their traits there.

Avoid constructing parallel handler types or per-protocol "context" objects
that duplicate access to `State`. If a handler needs data, add it to `Core` or
pass it through the existing backend/core API.

### 2.4 Threads and the event loop

The compositor state, Smithay renderer, libinput devices and Lua VM are not
`Send`. Keep compositor work on the main thread inside the calloop event loop.
Background threads are limited to self-contained helpers that return owned data;
do not hold a `&mut State` across a thread or block the event loop on I/O.

The event loop order in `state.rs` is deliberate: handle pending output config,
refresh capture sessions, clean up popups and locks, update keyboard focus, send
IPC diffs, then render and flush clients. If you add periodic work, put it in the
right place rather than adding a second timer with fuzzy ordering.

### 2.5 Lua is thread-bound

The Lua state lives in `LayoutPrograms`, uses `Rc`, and must only be touched
from the compositor thread. `poly-config` exposes parsed functions through
`LayoutPrograms`; do not move Lua values into a thread pool or a `Send` future.
Configuration setup, layout calls and `poly.include` all run through the same
VM.

Layouts are called synchronously during arrangement. They must not block, spawn
threads or retain compositor-owned references outside the VM.

### 2.6 One action vocabulary

Every action lives in `poly-ipc::Action`. When adding or changing one:

1. Add the variant and its doc comment in `poly-ipc/src/action.rs`.
2. Update `Action::name` (kebab-case for configuration and CLI).
3. Update `FromStr`/`from_argv` if the action takes arguments.
4. Check `ARGLESS_ACTIONS` so the bare spelling keeps working.
5. Let serde derive the wire spelling; do not write a second JSON mapping.
6. Add parser and round-trip tests.
7. Update `docs/config.md` and `docs/ipc.md`.

Never add a Lua-only action or a CLI-only alias that bypasses this enum. The
JSON variant names and the kebab-case names are intentionally different; keep
both stable.

### 2.7 Treat Lua as untrusted

Configuration and layout scripts can contain accidental infinite loops or huge
allocations. They are bounded by instruction budgets:

- 20,000,000 instructions for configuration loading;
- 200,000 instructions per `arrange` call;
- a maximum include depth of 32.

Do not remove or bypass these limits, add blocking host functions, or expose
arbitrary filesystem or process APIs to Lua. The only file helpers are
`poly.include` and `poly.read_file`; process launching goes through the action
vocabulary. A timeout or budget error must leave the compositor responsive and
fall back to the previous configuration or the built-in column layout.

### 2.8 Report errors at the right level

- Library crates use `thiserror` error types; the binary may use `anyhow` at
  its boundaries.
- Configuration parsing collects errors and warnings instead of panicking.
  Invalid values are errors; unknown keys are warnings. An error rejects the
  file and leaves the previous configuration active.
- User-visible failures (config errors, shader compile failures, XWayland setup
  failures) should reach the log and, where the user may not see a terminal, a
  Toast. Do not silently swallow them.
- Per-frame rendering errors should be logged at `debug`/`warn`, not turned into
  a panic.
- Preserve error context when wrapping: include the file path, output name,
  action or protocol request that failed.

### 2.9 Explicit return

The project style is an explicit `return` at the end of every function body,
including closures where it reads naturally. Crates enable
`#![allow(clippy::needless_return)]` for this reason. Match the surrounding
style rather than rewriting a file to expression tails.

### 2.10 Protocol globals and decorations

Window decorations are negotiated, not assumed. The decoration globals are only
exposed while `general.prefer_no_csd` is on; with it off, clients keep their own
decorations. A per-window `no_csd = true` can only request server-side
decorations while those globals are exposed.

When adding a global or changing its visibility, consider:

- clients that cache globals and never re-bind;
- the xdg-decoration and KDE decoration negotiation paths;
- fullscreen and layer-shell stacking;
- the no-CSD configuration default and per-window rules.

Keep global creation in `Core::new` and keep the `State` alive for the lifetime
of every global. Do not remove a global mid-session unless the protocol requires
it and clients are known to tolerate the removal.

### 2.11 Panics, locks and event-loop resilience

A panic must not leave the DRM master, libseat session or input devices held.
`main.rs` installs a panic hook and runs the compositor under `catch_unwind`;
the `State` is dropped during unwinding before the process exits.

In normal operation, never panic on malformed client input, a poisoned lock,
a missing output, a broken configuration or a failed GPU allocation. Handle
poisoned locks explicitly (the session-lock code does this), log the failure,
and keep the event loop alive. A bad client connection must only close that
connection.

`unwrap`/`expect` are acceptable only where the invariant is truly local and
provable, or in tests. Prefer `match`, `if let` or an error return. If you add an
`expect`, explain the invariant in the message or a nearby comment.

### 2.12 Unsafe code

Keep `unsafe` to the smallest possible block. Every `unsafe` block needs a
`SAFETY` comment naming the invariant and why the surrounding code upholds it.
Do not introduce unsafe for convenience; use the safe Smithay or standard
library API first. Run tests and clippy after any unsafe change.

### 2.13 Documentation and comments

- Public items have doc comments describing behavior, units and ownership.
- Module comments explain why the module exists and any non-obvious invariants.
- Code comments should explain decisions, not restate statements.
- Keep `docs/config.md` and `docs/ipc.md` current when changing configuration
  fields, defaults, wire types, event ordering or CLI spellings.
- Do not put non-English prose in `docs/`; the documentation is English-only.
- Preserve copyright and porting headers. If code is adapted from niri, smallvil,
  Hyprland or another project, record the source and license.

### 2.14 Tests

- Add unit tests next to the code they cover, following the existing `#[cfg(test)]`
  modules.
- Parser changes need valid, invalid and boundary-value cases.
- Action changes need both `FromStr` and serde/`from_argv` coverage.
- IPC changes need JSON round-trip tests and, for event changes, state-diff tests.
- Layout changes need a test with a tiny scripted layout where practical.
- Before finishing, run the workspace test suite, clippy with warnings denied,
  and a headless IPC smoke test for changes touching startup, config or IPC.

## 3. Architecture

```text
src/main.rs, cli.rs
  -> state::run
       -> Wayland display and socket
       -> locate/load/watch config
       -> IPC socket bind
       -> backend init (winit / udev / headless)
       -> XWayland satellite setup
       -> startup commands
       -> calloop event loop
```

- `src/state.rs` assembles the process and owns `State`.
- `src/core.rs` owns backend-independent state: `Space`, protocol globals,
  layout, focus, input, renderer caches, Toasts and IPC.
- `src/backend/` presents the space and feeds input. All three backends share
  the same `Core`; behavior belongs in `Core` unless it is truly backend-specific.
- `src/core/handlers/` implements Wayland protocol handlers; `handlers.rs`
  registers them with `delegate_dispatch2!(State)`.
- `src/core/layout/` owns window placement, transactions, overview and the
  column fallback. Lua layouts return rectangles through `LayoutPrograms`;
  they do not mutate state directly.
- `src/core/render/` builds and draws the scene: window visuals, decorations,
  blur, shadows and custom shaders.
- `src/core/ipc/` serves the socket and diffs state into events. The wire types
  and client live in `poly-ipc`.
- `poly-config/` locates, parses and validates Lua configuration; it also owns
  the embedded default config and layout execution.
- `poly-ipc/` contains `Action`, `Request`/`Response`/`Event`, the event-stream
  state model and the blocking client.
- `tools/capture-bench/` is a separate benchmark tool, not part of the
  compositor binary.

## 4. Build, run and validate

Rust 1.87 or newer and the Linux development libraries required by Smithay are
needed; [docs/building.md](docs/building.md) has the full prerequisite list and
backend notes, and [docs/installing.md](docs/installing.md) covers user-local
and system-wide installation. From the repository root:

```sh
cargo build
cargo run -- --backend winit          # nested inside Wayland/X11
cargo run -- --backend udev           # native TTY session
cargo run -- --backend headless       # no display, for tests
cargo run -- validate                 # validate the located config
cargo build --features dbus           # build with desktop notifications
```

The `--backend` value may also be `auto` (the default), which chooses winit when
a display is available and udev otherwise. A command after `--` runs once the
compositor is up:

```sh
cargo run -- --backend winit -- foot
```

Quality gates:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

For a headless IPC smoke test, start `target/debug/poly --backend headless` with
a private `XDG_RUNTIME_DIR`, find `poly.*.sock` beneath it, and send lines such
as `"Version"` or `"EventStream"`. `poly msg` requires `POLY_SOCKET`; child
processes started by Poly inherit the correct value.

Logging is controlled by `RUST_LOG`:

```sh
RUST_LOG=poly=debug,smithay=warn poly --backend winit
```

## 5. Changing configuration or IPC

For a new configuration field:

1. Add the field and default to the matching type in
   `poly-config/src/config.rs`.
2. Parse it in `lua.rs` or `layout.rs`, with an unknown-key list and range
   validation.
3. Wire it into the consumer (`src/core/...`) rather than leaving a parsed but
   unused field.
4. Update `poly-config/src/default_config.rs` if the annotated default file
   should mention it.
5. Add parser tests, including invalid values.
6. Document it in `docs/config.md`.

For a wire change:

1. Change the serde type in `poly-ipc/src/protocol.rs` or `action.rs`.
2. Keep compatibility: add optional/defaulted fields, append variants, never
   rename existing variants.
3. Update the event diff/state application if needed.
4. Add round-trip and state tests.
5. Update `docs/ipc.md` and the relevant `poly msg` command.

## 6. Licensing and ported code

Poly is GPL-3.0-or-later. New source files must be compatible with that license.
If code is ported from niri (GPL-3.0-or-later), smallvil (MIT), Hyprland
(BSD-3-Clause) or another project:

- keep the original copyright notice;
- add the source repository and file path in the header;
- update `NOTICE` with the new source;
- do not copy code whose license is incompatible.

## 7. Before finishing

- [ ] The behavior is implemented in the owning module, not duplicated.
- [ ] Configuration, IPC, build and install docs match the code.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo test --workspace --all-targets` passes.
- [ ] A headless run or IPC smoke test was done for startup/config/protocol work.
- [ ] Ported code headers and `NOTICE` are updated when applicable.
- [ ] No non-English prose was added to `docs/`.
