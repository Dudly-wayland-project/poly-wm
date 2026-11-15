# Poly: build, install and session recipes.
#
# The raw commands are documented in docs/building.md and docs/installing.md;
# these recipes wrap the ones used on a real machine.
#
#   just                  # list the recipes
#   just build            # release build (`just features=dbus build` adds D-Bus)
#   just run              # nested session, for testing inside the current desktop
#   just install          # /usr/local plus the login-screen entry (uses sudo)
#   just install-user     # ~/.local, no root
#   just session          # only the login-screen entry
#   just check            # fmt, clippy and the test suite
#   just uninstall        # remove the installed files

set shell := ["bash", "-euo", "pipefail", "-c"]

# Installation prefix for `just install`. The session launcher starts the
# binary installed next to it, so a custom prefix needs no `PATH` change.
prefix := "/usr/local"

# Where display managers look for Wayland session entries. ly reads
# `waylandsessions` from /etc/ly/config.ini (same default).
sessiondir := "/usr/share/wayland-sessions"

# Privilege escalation for the system-wide recipes.
sudo := "sudo"

# Extra Cargo features, for example `just features=dbus install`.
features := ""

bindir := prefix / "bin"
home := env_var("HOME")
feature_args := if features == "" { "" } else { "--features " + features }

# List the available recipes.
default:
    @just --list

# Build the release binary.
build:
    cargo build --release --locked {{ feature_args }}

# Run the compositor nested in the current Wayland or X11 session.
run:
    cargo run --release -- --backend winit

# Validate the configuration Poly would load, without starting it.
validate: build
    ./target/release/poly validate

# Run the quality gates from AGENTS.md.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace --all-targets

# Install system-wide: binary, session launcher and login-screen entry.
install: build
    {{ sudo }} install -Dm755 target/release/poly {{ bindir }}/poly
    {{ sudo }} install -Dm755 resources/poly-session {{ bindir }}/poly-session
    sed 's|^Exec=.*|Exec={{ bindir }}/poly-session|' resources/poly.desktop \
        | {{ sudo }} install -Dm644 /dev/stdin {{ sessiondir }}/poly.desktop
    @echo 'Installed. Pick "Poly" in the login screen; the first start writes'
    @echo '~/.config/poly/config.js.'

# Install for the current user only; most display managers ignore this entry.
install-user: build
    install -Dm755 target/release/poly {{ home }}/.local/bin/poly
    install -Dm755 resources/poly-session {{ home }}/.local/bin/poly-session
    sed 's|^Exec=.*|Exec={{ home }}/.local/bin/poly-session|' resources/poly.desktop \
        | install -Dm644 /dev/stdin {{ home }}/.local/share/wayland-sessions/poly.desktop

# Install only the login-screen entry, for a binary that is already installed.
session:
    sed 's|^Exec=.*|Exec={{ bindir }}/poly-session|' resources/poly.desktop \
        | {{ sudo }} install -Dm644 /dev/stdin {{ sessiondir }}/poly.desktop

# Remove the system-wide files. Configuration is user data and is kept.
uninstall:
    {{ sudo }} rm -f {{ bindir }}/poly {{ bindir }}/poly-session {{ sessiondir }}/poly.desktop

# Remove the user-local files.
uninstall-user:
    rm -f {{ home }}/.local/bin/poly {{ home }}/.local/bin/poly-session {{ home }}/.local/share/wayland-sessions/poly.desktop
