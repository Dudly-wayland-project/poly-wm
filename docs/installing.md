# Installing Poly from source

This guide installs a release build of Poly on the local machine. It assumes
that the build prerequisites are installed and that the release binary has been
built as described in [building.md](building.md):

```sh
cargo build --release --locked
```

This is a source installation, not a distribution package. It installs the
executable and, optionally, a configuration file. It does not create a `.deb`
or `.rpm`, install a display-manager session entry or manage system services.
For a quick trial run, the binary in `target/release/` can be used directly
without installing it.

For configuration syntax and runtime control, see [config.md](config.md) and
[ipc.md](ipc.md).

## Choose an install location

| Scope | Binary | Configuration | When to use |
| --- | --- | --- | --- |
| User | `~/.local/bin/poly` | `~/.config/poly/config.js` | One user, no root access. Recommended. |
| System | `/usr/local/bin/poly` | `/etc/poly/config.js` | All users on the machine; requires root. |

Use `/usr/local/bin` rather than `/usr/bin` for a source install. `/usr/bin` is
owned by the distribution package manager. Without `--config` or
`$POLY_CONFIG`, the user configuration takes precedence over the system
configuration.

The binary contains the default configuration, the fallback cursor and the
built-in shaders. There are no data files from `resources/` that need to be
copied.

## User-local installation

This is the recommended option for a single-user machine. It does not require
root and keeps Poly separate from distribution packages.

### Install with Cargo

`cargo install` builds the release binary and copies it to the selected prefix:

```sh
cd /path/to/poly-wm
cargo install --path . --locked --root "$HOME/.local"
```

The executable is installed as `$HOME/.local/bin/poly`. To include the optional
D-Bus notification support, keep the feature flag:

```sh
cargo install --path . --locked --features dbus --root "$HOME/.local"
```

Do not run `cargo install` with `sudo` for a user-local installation.

### Install an existing release build

If `target/release/poly` already exists, copy it directly:

```sh
install -Dm755 target/release/poly "$HOME/.local/bin/poly"
```

`install -D` creates `$HOME/.local/bin` if necessary and sets the executable
mode. Build the release binary first if that file is missing.

### Add the directory to PATH

Some distributions include `~/.local/bin` in `PATH`, but not all do. Check with:

```sh
command -v poly
```

If the command is not found, add the directory to the shell startup file used
for login shells, such as `~/.profile`:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Open a new terminal or source the file, then check again. The compositor is
normally started from a login shell on a TTY, so make sure the login-shell
configuration contains the `PATH` entry, not only an interactive shell's
configuration.

## System-wide installation

Build as your normal user, then install the binary with root privileges:

```sh
cargo build --release --locked
sudo install -Dm755 target/release/poly /usr/local/bin/poly
```

If you want D-Bus notifications, build with the feature before installing:

```sh
cargo build --release --locked --features dbus
sudo install -Dm755 target/release/poly /usr/local/bin/poly
```

Do not run Cargo itself as root. The release build and the installation step are
separate.

A system-wide configuration is optional. Poly uses `/etc/poly/config.js` only
when the user has no configuration file:

```sh
sudo install -Dm644 /path/to/config.js /etc/poly/config.js
```

Most users should keep their configuration in their home directory instead.

## Configuration

On the first compositor start, Poly writes an annotated default configuration to
the user location when no configuration file is found:

```text
~/.config/poly/config.js
```

The file is created with `O_EXCL`, so Poly does not overwrite an existing
configuration. It uses the first file in the lookup order; a system
configuration is used only when no user configuration exists.

To install a prepared configuration:

```sh
install -Dm644 /path/to/config.js "$HOME/.config/poly/config.js"
```

For a system default:

```sh
sudo install -Dm644 /path/to/config.js /etc/poly/config.js
```

The repository does not ship a standalone `config.js`; the default is compiled
into the binary. Validate the resolved file without starting the compositor:

```sh
poly validate
poly validate --config /path/to/config.js
```

See [config.md](config.md) for the lookup order and every setting.

## Verify the installation

Check which binary will run, its version and its configuration:

```sh
command -v poly
poly --version
poly validate
```

If `poly` resolves to an older copy, inspect the search order:

```sh
type -a poly
```

Check that all dynamic libraries are available:

```sh
ldd "$(command -v poly)" | grep 'not found' || true
```

This command should print nothing. The binary needs system libraries such as
Wayland, libinput, libudev, libseat, GBM, libdrm, xkbcommon and libdisplay-info
at runtime; some are loaded directly and some when the compositor starts. The
development packages used for the build normally provide the corresponding
runtime libraries. If you copy the binary to another machine, install the
runtime libraries there or build it on that machine.

## Start Poly

### Test in a nested session

From an existing Wayland or X11 desktop, start Poly in a window:

```sh
poly --backend winit
```

This is the safest way to test the installed binary and the configuration. It
does not replace the current desktop, and the host compositor may keep some
keyboard shortcuts. The default `Mod` key is Alt in a nested session.

### Start a native session

From a free virtual terminal, start the DRM/KMS backend:

```sh
exec poly --backend udev
```

The `exec` replaces the login shell, so the TTY belongs to Poly until it exits.
A normal logind TTY session provides `XDG_RUNTIME_DIR` and a libseat seat. With
seatd, ensure the daemon is running and the user has access to the seat. An SSH
session does not own a seat and cannot start the udev backend. See
[building.md](building.md) for the libseat details.

Poly does not install a display-manager session entry or a systemd service.
Those are desktop integration and packaging concerns. If you need a graphical
login entry, create it according to your display manager's documentation.

## Optional runtime components

- **Toast font.** Poly looks for DejaVu Sans, Liberation Sans or Noto Sans under
  `/usr/share/fonts`. If none is present, on-screen Toasts have no text. Set
  `POLY_TOAST_FONT` to a `.ttf` or `.otf` file in the environment that starts
  Poly to use a different font.
- **XWayland.** X11 applications require `xwayland-satellite`. Install it
  separately and put it in `PATH`, or set `xwayland.satellite_path` in the
  configuration. Poly starts and runs without it; only X11 clients are
  unavailable.
- **Desktop notifications.** Build with `--features dbus`, set
  `errors.dbus = true` in the configuration, and provide a D-Bus session bus.
  Without the feature, notifications are unavailable but Toasts still work.

## Update an installation

Stop Poly before replacing the binary. A running compositor keeps using the
binary it started with until it is restarted.

For a user-local Cargo installation:

```sh
cd /path/to/poly-wm
git pull --ff-only
cargo install --path . --locked --force --root "$HOME/.local"
```

Add `--features dbus` if that was part of the original installation. For a
manual or system-wide installation, rebuild and copy the binary again:

```sh
git pull --ff-only
cargo build --release --locked
install -Dm755 target/release/poly "$HOME/.local/bin/poly"
```

For a system-wide installation, use `sudo install ... /usr/local/bin/poly`.
Check the new version with `poly --version` before starting a session.

## Uninstall

Stop Poly, then remove the binary. For a Cargo installation:

```sh
cargo uninstall --root "$HOME/.local" poly
```

For a manual user installation:

```sh
rm "$HOME/.local/bin/poly"
```

For a system-wide installation:

```sh
sudo rm /usr/local/bin/poly
```

Configuration is user data and is not removed automatically. Remove it only if
you also want to discard the settings and included files:

```sh
rm -rf "$HOME/.config/poly"
sudo rm -rf /etc/poly
```

## Troubleshooting

- **`poly: command not found`.** Add `~/.local/bin` to `PATH` in the login-shell
  startup file, or use the absolute path to the binary. Run `hash -r` in the
  current shell after changing `PATH`.
- **An old version starts.** Run `type -a poly` and check which directory comes
  first. A system installation in `/usr/local/bin` can shadow a user-local
  installation, or the other way around.
- **A shared library is missing.** Run the `ldd` check above and install the
  runtime package for the library named in the error. Building on the target
  machine avoids most copy-related library problems.
- **Toasts have no text.** Install a font from the candidate list or set
  `POLY_TOAST_FONT` to an existing font file before starting Poly.
- **The udev backend cannot open a seat.** Start it from a free TTY or an active
  logind/seatd session; see [building.md](building.md).
- **X11 applications do not start.** Install `xwayland-satellite` and check
  `xwayland.satellite_path` or `poly msg xwayland`.
- **Notifications do not appear.** Confirm the binary was built with
  `--features dbus`, `errors.dbus = true` is set, and a session bus is running.
