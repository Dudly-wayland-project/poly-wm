//! The default configuration written on first run (`docs/plan.md` §11).
//!
//! Poly runs without a configuration file (the built-in defaults are niri's),
//! but a user with no file has nothing to edit. When none is found the
//! compositor writes this template to the usual location, so that the first
//! thing the documentation says — "edit `~/.config/poly/config.lua`" — actually
//! works, and the file explains itself.
//!
//! The template is a valid configuration: it parses without warnings and only
//! spells out settings whose defaults a user is likely to want to change.

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::locate::ConfigSource;

/// The text written to a fresh `~/.config/poly/config.lua`.
pub const DEFAULT_CONFIG: &str = r##"-- Poly configuration (`docs/config.md` for every setting).
--
-- The file is Lua: it is executed, and its return value is the configuration.
-- `poly.setup{ … }` works as well. `poly.include("bindings.lua")` runs another
-- file and returns its value, which is how a big configuration stays readable;
-- included files are reloaded too when they change.
--
-- A `theme.lua` next to this file is merged over it when it exists, which is
-- where a palette generator writes its colours; it needs no change here.
--
-- Saving this file applies it immediately, with no restart. A broken edit keeps
-- the previous configuration running and shows the problem in a Toast.

return {
  general = {
    -- The modifier that bindings below spell as `Mod`. Defaults to Super on a
    -- real session and to Alt when nested in another compositor.
    -- mod_key = "Super",
    gap = 8,                      -- space between windows and the output edges
    prefer_no_csd = true,         -- ask clients to use server-side decorations
    border = {
      width = 2,
      radius = 8,
      active = "#4f8cff",
      inactive = "#3a3a3a",
    },
    animations = {
      duration = 180,             -- milliseconds
      window_close = { duration = 180 },
    },
    -- The pointer Poly draws. Unset follows `$XCURSOR_THEME`/`$XCURSOR_SIZE`.
    -- cursor = { theme = "Adwaita", size = 24 },
  },

  -- Input devices. Entries match by glob, first match wins.
  input = {
    keyboard = { layout = "us" },
    -- devices = {
    --   ["*touchpad*"] = { natural_scroll = true, tap = true, accel = "adaptive" },
    --   ["*Mouse*"]    = { accel = "flat" },
    -- },
  },

  -- Every action is spelled as in `poly msg action …`; see `docs/config.md`
  -- for the full list.
  keybinds = {
    ["Mod+Return"]  = { spawn = { "foot" } },
    ["Mod+Q"]       = "close-window",
    ["Mod+Space"]   = "toggle-floating",
    ["Mod+F"]       = "toggle-fullscreen",
    ["Mod+Tab"]     = "cycle-layout",
    ["Mod+Left"]    = "focus-left",
    ["Mod+Right"]   = "focus-right",
    ["Mod+Up"]      = "focus-up",
    ["Mod+Down"]    = "focus-down",
    ["Mod+Shift+Left"] = "move-window-left",
    ["Mod+Shift+Right"] = "move-window-right",
    -- Workspaces: the vertical stack of `docs/plan.md` §9.6.
    ["Mod+Page_Up"]   = "focus-workspace-up",
    ["Mod+Page_Down"] = "focus-workspace-down",
    ["Mod+1"]         = { focus_workspace = 1 },
    ["Mod+2"]         = { focus_workspace = 2 },
    ["Mod+3"]         = { focus_workspace = 3 },
    ["Mod+O"]         = "toggle-overview",
    ["Mod+R"]         = "reload-config",
    -- Mouse bindings move and resize windows (`docs/plan.md` §9.5).
    ["Mod+MouseLeft"]  = "move-window",
    ["Mod+MouseRight"] = "resize-window",
    -- Media keys are passed through by `xkb`; bind them here when the session
    -- does not have a separate daemon.
    -- ["XF86AudioRaiseVolume"] = { spawn = { "wpctl", "set-volume", "@DEFAULT_AUDIO_SINK@", "5%+" } },
  },

  -- Display settings, keyed by output name or glob. Every matching entry is
  -- merged, the most specific one winning field by field, so `["*"]` holds the
  -- defaults for outputs plugged in later.
  outputs = {
    ["*"] = {
      -- mode = "preferred",       -- or "<width>x<height>[@<hz>]"
      -- scale = 1.0,              -- or "auto"
      -- transform = "normal",     -- 90 / 180 / 270 / flipped / flipped-90 …
      -- position = "auto",        -- or { x = 0, y = 0 }
      -- vrr = true,               -- DRM only
      -- focus = true,             -- this output is the focused one
      background = "#000000",      -- painted behind every window
    },
  },

  window_drag = {
    tiled = "reorder",            -- dragging a tiled window reorders the layout
    threshold = 8,                -- pixels before a drag starts
    axis_lock = "shift",          -- hold Shift to lock one axis; false disables
    escape_cancels = true,        -- Esc restores the geometry and cancels
    resize_pick = "thirds",       -- or a pixel width, e.g. 8
  },

  overview = {
    zoom = 0.5,
    backdrop_color = "#262626",
    card_color = "#404040",
  },

  spawn_at_startup = {
    -- { exec = { "waybar" } },
    -- { shell = "wl-paste --watch cliphist store" },
  },

  -- Environment for every child poly spawns.
  environment = {
    -- MOZ_ENABLE_WAYLAND = "1",
  },

  -- Problems always appear as on-screen Toasts; set this to also send them to
  -- the desktop notification daemon (needs a build with the `dbus` feature).
  errors = { dbus = false },

  -- X11 clients (xterm, Steam, ...) run through xwayland-satellite. X11 is
  -- only started when an X11 program connects; every child poly spawns from
  -- then on gets `DISPLAY` set. Set `enable = false` for a Wayland-only
  -- session, or `on_demand = false` to keep the satellite running.
  xwayland = {
    enable = true,
    satellite_path = "xwayland-satellite",
    on_demand = true,
    spawn_at_startup = false,
  },

  -- Lua layouts: `ctx` carries the output work area, the workspace id and the
  -- windows in order; the return value maps window ids to geometry. A layout
  -- table can also declare `actions`, per-layout `keybinds` and an `insert`
  -- hook (`docs/config.md`).
  -- layouts = {
  --   columns = function(ctx)
  --     local wa, n, out = ctx.output.work_area, #ctx.windows, {}
  --     local cw = math.floor((wa.w - (n - 1) * poly.gap) / math.max(n, 1))
  --     for i, w in ipairs(ctx.windows) do
  --       out[w.id] = { x = wa.x + (i - 1) * (cw + poly.gap), y = wa.y, w = cw, h = wa.h }
  --     end
  --     return out
  --   end,
  -- },
  -- default_layout = "columns",
}
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
        let path = dir.join("config.lua");
        fs::write(&path, DEFAULT_CONFIG).unwrap();

        let result = crate::lua::load_file(&path);
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
        let path = dir.join("poly").join("config.lua");

        let written = write_default(&ConfigSource::Default(Some(path.clone()))).unwrap();
        assert_eq!(written, Some(path.clone()));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("return {"), "{text}");

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
