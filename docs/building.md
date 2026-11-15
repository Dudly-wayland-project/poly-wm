# Building Poly

Poly is a Linux Wayland compositor. This guide covers building and running it
from a source checkout. To install the built binary for regular use, see
[installing.md](installing.md). Distribution packaging, service units and
desktop integration are intentionally out of scope.

For configuration, see [config.md](config.md). For the runtime control socket,
see [ipc.md](ipc.md).

## Requirements

- Linux. The compositor uses DRM/KMS, libinput, udev and libseat. Other
  operating systems are not supported.
- Rust 1.87 or newer with Cargo. The repository's `rust-toolchain.toml` selects
  the stable toolchain and requests `rustfmt` and `clippy` when using rustup.
- A C compiler and `pkg-config`. The QuickJS runtime is built from source, and
  the native libraries are found through pkg-config.
- Development headers for the libraries listed below.
- A working EGL/GLES driver for the `winit` and `udev` backends. The `headless`
  backend does not need a GPU.

Install Rust with [rustup](https://rustup.rs/) if it is not already available:

```sh
rustup toolchain install stable --component rustfmt --component clippy
rustup default stable
rustc --version
```

The minimum supported Rust version is also recorded as `rust-version` in
`Cargo.toml`.

## Native dependencies

These pkg-config modules are required at build time:

| pkg-config module | Used for |
| --- | --- |
| `wayland-client`, `wayland-server`, `wayland-egl`, `wayland-cursor` | Wayland protocol support and the nested `winit` backend |
| `libinput` | Input devices on the `udev` backend |
| `libudev` | Device enumeration |
| `libseat` | Session and DRM master handling |
| `gbm` | GPU buffer allocation |
| `libdrm` | DRM/KMS output handling |
| `xkbcommon` | Keyboard layouts and key handling |
| `libdisplay-info` | EDID and display mode information |

### Debian and Ubuntu

```sh
sudo apt update
sudo apt install build-essential pkg-config \
    libwayland-dev libinput-dev libudev-dev libseat-dev \
    libgbm-dev libdrm-dev libxkbcommon-dev libdisplay-info-dev
```

For Mesa-based graphics, the runtime packages are usually `libegl1` and
`libgles2`; NVIDIA and other vendors provide their own EGL/GLES drivers.

### Fedora

```sh
sudo dnf install gcc make pkgconf-pkg-config \
    wayland-devel libinput-devel systemd-devel libseat-devel \
    mesa-libgbm-devel libdrm-devel libxkbcommon-devel libdisplay-info-devel
```

The Mesa runtime packages are usually `mesa-libEGL` and `mesa-libGLES`.

### Arch Linux

```sh
sudo pacman -S --needed base-devel rustup pkgconf \
    wayland libinput systemd-libs seatd mesa libdrm libxkbcommon libdisplay-info
rustup default stable
```

On Arch, `seatd` provides libseat, `systemd-libs` provides libudev, and `mesa`
provides GBM, EGL and GLES.

### Other distributions

Install the packages that provide the pkg-config modules in the table above.
The development package is often named with a `-dev` or `-devel` suffix. You
can check a module with:

```sh
for module in wayland-client wayland-server wayland-egl wayland-cursor \
    libinput libudev libseat gbm libdrm xkbcommon libdisplay-info; do
    pkg-config --exists "$module" || echo "missing: $module"
done
```

If a module is missing, install the corresponding development package before
building.

## Build

From the repository root:

```sh
cargo build --locked
```

The debug binary is written to `target/debug/poly`. For normal use, build an
optimized binary:

```sh
cargo build --release --locked
```

The release binary is written to `target/release/poly`. The release profile uses
thin LTO and keeps line tables for backtraces, so the first release build can
take longer than a debug build. To copy the binary to a user or system location,
see [installing.md](installing.md).

`--locked` makes Cargo use the committed `Cargo.lock`. This matters because
Smithay and `smithay-drm-extras` are pinned to a fixed Git revision. The first
build downloads dependencies from crates.io and that Git revision, so it needs
network access; later builds use Cargo's cache.

All three backends are compiled by default. There are no backend feature flags;
choose a backend at runtime with `--backend`.

### Optional D-Bus support

The `dbus` feature adds desktop notifications through `org.freedesktop.Notifications`:

```sh
cargo build --release --locked --features dbus
```

This feature uses the pure-Rust `zbus` client and does not need libdbus
development headers. To use it, set `errors.dbus = true` in the configuration
and run Poly with access to a D-Bus session bus. Without the feature, the
setting has no effect and `poly validate` prints a warning.

## Run

### Nested inside Wayland or X11 (`winit`)

The `winit` backend opens a window in the current graphical session. It is the
easiest backend for development:

```sh
cargo run --release --locked -- --backend winit
```

or, after building:

```sh
target/release/poly --backend winit
```

This requires `WAYLAND_DISPLAY`, `WAYLAND_SOCKET` or `DISPLAY` to be set. The
host compositor keeps keyboard shortcuts such as the session's application
launcher, so this backend is not a substitute for a native session.

### Native TTY (`udev`)

The `udev` backend takes over DRM/KMS outputs and input devices:

```sh
target/release/poly --backend udev
```

Start it from a free virtual terminal, or from a session unit that gives it an
active libseat seat. It cannot run inside another compositor window; use
`winit` for that.

libseat talks to systemd-logind or seatd. With seatd, make sure the daemon is
running and your user has access to the seat (usually through the `seat` group).
With logind, start Poly from an active local session; an SSH session normally
does not own a seat.

### Headless

The headless backend has a virtual 1920x1080 output and does not draw. It is
intended for tests, CI and IPC work:

```sh
runtime_dir=$(mktemp -d)
chmod 700 "$runtime_dir"
XDG_RUNTIME_DIR="$runtime_dir" target/release/poly --backend headless
```

`XDG_RUNTIME_DIR` must be set for every backend; Poly places its Wayland and IPC
sockets there. Most desktop sessions already set it. Stop a foreground compositor
with Ctrl-C.

### Automatic backend selection

`--backend auto` is the default. It chooses `winit` when `WAYLAND_DISPLAY`,
`WAYLAND_SOCKET` or `DISPLAY` is set, and `udev` otherwise:

```sh
target/release/poly --backend auto
```

### Startup command

A command after `--` is passed to `sh -c` once the compositor is up:

```sh
target/release/poly --backend winit -- foot
```

With Cargo, the first `--` separates Cargo options from Poly's arguments:

```sh
cargo run --release --locked -- --backend winit -- foot
```

The command inherits `WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`, `POLY_SOCKET` and,
once X11 is available, `DISPLAY`.

## First run and configuration

Poly looks for `config.js` in the usual locations and creates an annotated
default file when none exists. Check the resolved file without starting the
compositor:

```sh
target/release/poly validate
target/release/poly validate --config ./config.js
```

The lookup order and every setting are documented in [config.md](config.md).
Pass `--config` to the compositor to use a specific file:

```sh
target/release/poly --config ./config.js --backend winit
```

Configuration errors are collected, logged and shown as Toasts. A failed reload
keeps the previous configuration active. Use `RUST_LOG` for more detail:

```sh
RUST_LOG=poly=debug,smithay=warn target/release/poly --backend winit
```

## Optional runtime components

- **XWayland.** X11 applications need `xwayland-satellite` in `PATH`, or a path
  set in `xwayland.satellite_path`. The compositor builds and runs without it;
  only X11 clients are unavailable. Check the current state with
  `poly msg xwayland`.
- **Notifications.** Build with the `dbus` feature, set `errors.dbus = true`,
  and provide a session bus if you want errors delivered to a notification
  daemon. Toasts are always drawn by Poly itself.
- **Portal.** The `poly portal` command routes a desktop portal at the running
  session. It is a runtime integration and does not add build dependencies.

## Verify the build

Run the same checks used by contributors:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features dbus
```

For a quick smoke test without a display:

```sh
target/release/poly validate
target/release/poly --version
```

A full headless IPC test is described in [AGENTS.md](../AGENTS.md), section 4.
It starts the headless backend with a private `XDG_RUNTIME_DIR`, waits for
`poly.*.sock`, and sends a request such as `"Version"`.

## Troubleshooting

- **`pkg-config` cannot find a library.** Install the `-dev`/`-devel` package
  from the table above. The error usually names the missing module.
- **`libdisplay-info` is missing.** It is required by `smithay-drm-extras`; this
  is a build-time dependency even if you only plan to use the nested backend.
- **The Rust version is too old.** Run `rustup update stable` and check
  `rustc --version`. Poly requires 1.87 or newer.
- **The first build cannot fetch Smithay.** Check network access and any proxy
  configuration. Smithay is fetched from GitHub at the revision pinned in
  `Cargo.lock`; do not replace it with a local path.
- **`XDG_RUNTIME_DIR is not set`.** Export a private directory with mode 0700
  before starting Poly, as in the headless example.
- **libseat cannot open a session.** Run the `udev` backend from a free TTY or
  an active session managed by logind/seatd. A nested terminal inside another
  compositor does not own a seat.
- **The renderer cannot create an EGL display.** Install a working Mesa or
  vendor EGL/GLES driver. The `headless` backend is a useful fallback for
  checking the build and IPC without a GPU.
- **X11 applications do not start.** Install `xwayland-satellite`, check its
  path in the configuration, and inspect `poly msg xwayland`.
- **Desktop notifications do not appear.** Confirm that Poly was built with
  `--features dbus`, `errors.dbus` is true, and the session bus is available.
- **Keyboard input is captured by the host.** This is expected with `winit`;
  test native input on a TTY with the `udev` backend.
