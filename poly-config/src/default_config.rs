//! The default configuration written on first run.
//!
//! Poly runs without a configuration file (the built-in defaults are niri's),
//! but a user with no file has nothing to edit. When none is found the
//! compositor writes this template to the usual location, so that the first
//! thing the documentation says — "edit `~/.config/poly/config.js`" — actually
//! works, and the file explains itself.
//!
//! The template is a valid configuration: it parses without warnings and only
//! spells out settings whose defaults a user is likely to want to change.

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::locate::ConfigSource;

/// The text written to a fresh `~/.config/poly/config.js`.
pub const DEFAULT_CONFIG: &str = r##"// Poly configuration (`docs/config.md` for every setting).
//
// The file is JavaScript. It runs once and calls the helpers below, which
// collect the configuration; nothing is returned. `config({ … })` may be called
// more than once, objects are merged and arrays are replaced, so a file can be
// split into readable sections or included files:
//
//   include("bindings.js");   // runs another file in this same global scope
//
// `read_file("path")` returns a file as a string, for data rather than code.
//
// A program that generates colours writes its own file; import it here and it
// applies on the next reload, without this file being rewritten:
//
//   include("palette.js");
//
// Saving this file applies it immediately, with no restart. A broken edit keeps
// the previous configuration running and shows the problem in a Toast.

config({
  general: {
    // The modifier that bindings below spell as `Mod`. Defaults to Super on a
    // real session and to Alt when nested in another compositor.
    // mod_key: "Super",
    gap: 8,                      // space between windows and the output edges
    prefer_no_csd: true,         // ask clients to use server-side decorations
    border: {
      width: 2,
      radius: 8,
      active: "#4f8cff",
      inactive: "#3a3a3a",
    },
    animations: {
      duration: 180,             // milliseconds
      window_close: { duration: 180 },
    },
    // The pointer Poly draws. Unset follows `$XCURSOR_THEME`/`$XCURSOR_SIZE`.
    // cursor: { theme: "Adwaita", size: 24 },
  },

  // Input devices. Entries match by glob, first match wins.
  input: {
    keyboard: { layout: "us" },
    // devices: {
    //   "*touchpad*": { natural_scroll: true, tap: true, accel: "adaptive" },
    //   "*Mouse*":    { accel: "flat" },
    // },
  },

  window_drag: {
    tiled: "reorder",            // dragging a tiled window reorders the layout
    threshold: 8,                // pixels before a drag starts
    axis_lock: "shift",          // hold Shift to lock one axis; false disables
    escape_cancels: true,        // Esc restores the geometry and cancels
    resize_pick: "thirds",       // or a pixel width, e.g. 8
    throttle_resize: true,       // one size request per client acknowledgement
  },

  overview: {
    zoom: 0.5,
    backdrop_color: "#262626",
    card_color: "#404040",
  },

  // Problems always appear as on-screen Toasts; set this to also send them to
  // the desktop notification daemon (needs a build with the `dbus` feature).
  errors: { dbus: false },

  // X11 clients (xterm, Steam, ...) run through xwayland-satellite. X11 is
  // only started when an X11 program connects; every child poly spawns from
  // then on gets `DISPLAY` set. Set `enable: false` for a Wayland-only
  // session, or `on_demand: false` to keep the satellite running.
  xwayland: {
    enable: true,
    satellite_path: "xwayland-satellite",
    on_demand: true,
    spawn_at_startup: false,
  },
});

// Display settings, keyed by output name or glob. Every matching entry is
// merged, the most specific one winning field by field, so `"*"` holds the
// defaults for outputs plugged in later.
output("*", {
  // mode: "preferred",       // or "<width>x<height>[@<hz>]"
  // scale: 1.0,              // or "auto"
  // transform: "normal",     // 90 / 180 / 270 / flipped / flipped-90 …
  // position: "auto",        // or { x: 0, y: 0 }
  // vrr: true,               // DRM only
  // focus: true,             // this output is the focused one
  background: "#000000",      // painted behind every window
});

// Every action is spelled as in `poly msg action …`; see `docs/config.md` for
// the full list. `bind(keys, action)` also takes an options object as a third
// argument, e.g. `{ repeat: false }`.
bind("Mod+Return", { spawn: ["foot"] });
bind("Mod+Q", "close-window");
bind("Mod+Space", "toggle-floating");
bind("Mod+F", "toggle-fullscreen");
bind("Mod+Tab", "cycle-layout");
bind("Mod+Left", "focus-left");
bind("Mod+Right", "focus-right");
bind("Mod+Up", "focus-up");
bind("Mod+Down", "focus-down");
bind("Mod+Shift+Left", "move-window-left");
bind("Mod+Shift+Right", "move-window-right");
// Workspaces: one vertical stack per output.
bind("Mod+Page_Up", "focus-workspace-up");
bind("Mod+Page_Down", "focus-workspace-down");
bind("Mod+1", { focus_workspace: 1 });
bind("Mod+2", { focus_workspace: 2 });
bind("Mod+3", { focus_workspace: 3 });
bind("Mod+O", "toggle-overview");
bind("Mod+R", "reload-config");
// Mouse bindings move and resize windows.
bind("Mod+MouseLeft", "move-window");
bind("Mod+MouseRight", "resize-window");
// Media keys are passed through by `xkb`; bind them here when the session does
// not have a separate daemon.
// bind("XF86AudioRaiseVolume",
//      { spawn: ["wpctl", "set-volume", "@DEFAULT_AUDIO_SINK@", "5%+"] });

// Programs started once the compositor is up. A string runs through the shell;
// an object form is `{ exec: [ … ] }` or `{ shell: "…" }`.
// startup("waybar");
// startup({ exec: ["wl-paste", "--watch", "cliphist", "store"] });

// Environment for every child poly spawns.
// env("MOZ_ENABLE_WAYLAND", "1");

// JavaScript layouts: `ctx` carries the output work area, the workspace id and
// the windows in order; the return value maps window ids to geometry. A layout
// definition object can also declare `actions`, per-layout `keybinds` and an
// `insert` hook (`docs/config.md`).
// layout("columns", ctx => {
//   const wa = ctx.output.work_area;
//   const n = ctx.windows.length;
//   const cw = Math.floor((wa.w - (n - 1) * ctx.gap) / Math.max(n, 1));
//   const out = {};
//   ctx.windows.forEach((window, i) => {
//     out[window.id] = { x: wa.x + i * (cw + ctx.gap), y: wa.y, w: cw, h: wa.h };
//   });
//   return out;
// });
// default_layout("columns");
"##;

/// Write the default configuration for `source`, if it has no file yet.
///
/// Returns the location it wrote to, or `None` when a configuration already
/// exists (or there is no place to put one: without `$XDG_CONFIG_HOME` or
/// `$HOME` — and in that case `ConfigSource::Default(None)` says so).
pub fn write_default(source: &ConfigSource) -> io::Result<Option<PathBuf>> {
    if source.path().is_some() {
        return Ok(None);
    }
    let Some(path) = source.watch_path() else {
        return Ok(None);
    };
    // `create_new` rather than `write`: two compositors starting at once, or a
    // user who created the file in the meantime, must not have their file
    // overwritten.
    let created = write_new(path);
    return match created {
        Ok(()) => Ok(Some(path.to_owned())),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(err) => Err(err),
    };
}

/// Create `path` and its parent directories, failing if it exists.
fn write_new(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(DEFAULT_CONFIG.as_bytes())?;
    return file.flush();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::locate::ConfigSource;

    #[test]
    fn the_template_is_a_valid_configuration() {
        let dir = std::env::temp_dir().join(format!("poly-default-parse-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.js");
        fs::write(&path, DEFAULT_CONFIG).unwrap();

        let result = crate::load_file(&path);
        let config = match result.config {
            Ok(config) => config,
            Err(err) => panic!("the default configuration does not load: {err}"),
        };
        // No warnings either: a fresh install must not greet the user with a
        // wall of "unknown setting" Toasts.
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(config.general.gap, 8);
        assert!(config.general.prefer_no_csd);
        assert!(!config.keybinds.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_happens_only_when_the_file_is_missing() {
        let dir = std::env::temp_dir().join(format!("poly-default-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("poly").join("config.js");

        let written = write_default(&ConfigSource::Default(Some(path.clone()))).unwrap();
        assert_eq!(written, Some(path.clone()));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("config({"), "{text}");

        // A second run finds the file and leaves it alone.
        let again = write_default(&ConfigSource::User(path.clone())).unwrap();
        assert_eq!(again, None);

        // Even a `Default` source must not clobber an existing file.
        let again = write_default(&ConfigSource::Default(Some(path.clone()))).unwrap();
        assert_eq!(again, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_place_to_write_is_not_an_error() {
        assert_eq!(
            write_default(&ConfigSource::Default(None)).unwrap(),
            None::<PathBuf>
        );
    }
}
