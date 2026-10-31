# Configuration

Poly is configured with a Lua file. The file is executed, and the table it
returns (or passes to `poly.setup`) becomes the compositor configuration.

```lua
return {
    general = {
        gap = 8,
    },
    keybinds = {
        ["Mod+Return"] = { spawn = { "foot" } },
    },
}
```

Configuration problems are collected instead of failing fast. Unknown settings
are reported as warnings, invalid values as errors. A file with errors is not
applied; on reload the previous configuration stays in effect. Errors are shown
as on-screen Toasts and written to the log. For the runtime control socket, see
[ipc.md](ipc.md).

## Locating the file

The configuration is looked up in this order:

1. `--config <path>` on the command line
2. `$POLY_CONFIG`
3. `$XDG_CONFIG_HOME/poly/config.lua` (or `~/.config/poly/config.lua`)
4. `/etc/poly/config.lua`

If none exists, Poly starts with built-in defaults and writes an annotated
`config.lua` to the user location, so there is a file to edit. Use
`poly validate` to locate and validate the file without starting the compositor:

```sh
poly validate
poly validate --config ./config.lua
```

The root file and every file loaded with `poly.include` are watched. Saving any
of them reloads the configuration immediately. A broken edit never takes down
the running session.

## Lua environment

The following helpers are available while the configuration is evaluated:

| Helper | Description |
| --- | --- |
| `poly.setup(table)` | Use `table` as the configuration instead of returning it. |
| `poly.include(path)` | Execute another Lua file and return its value. Paths are relative to the file that calls `poly.include`; `~` is expanded. Includes are watched and may nest up to 32 levels. |
| `poly.read_file(path)` | Return the contents of a data file, or `nil, error`. Paths resolve like `poly.include`; data files are not watched. |
| `poly.spawn(argv)` | Return `{ spawn = argv }`, for use as a binding action. |
| `poly.spawn_shell(command)` | Return `{ spawn_shell = command }`, for use as a binding action. |

Inside layout functions, Poly additionally exposes `poly.gap`, `poly.state` and
`poly.layout.relayout()`; see [layouts](#layouts).

The Lua VM has a budget of 20,000,000 instructions, so an accidental infinite
loop in the configuration cannot hang the compositor. Includes and the root
file share the same budget.

## Top-level keys

| Key | Type | Purpose |
| --- | --- | --- |
| `general` | table | Gaps, decorations, animations, opacity and shaders. |
| `input` | table | Keyboard layout and libinput device settings. |
| `keybinds` | table | Bindings from key, mouse button or scroll trigger to actions. |
| `window_drag` | table | Interactive move and resize behaviour. |
| `spawn_at_startup` | array | Commands started once the compositor is up. |
| `environment` | table | Extra environment variables for child processes. |
| `layer_rules` | array | Rules for layer-shell surfaces (bars, launchers, lock screens). |
| `layouts` | table | Named Lua layout functions. |
| `default_layout` | string | Layout used by new workspaces. |
| `window_rules` | array | Per-window placement and appearance rules. |
| `outputs` | table | Per-output mode, scale, transform, position and background. |
| `fullscreen` | table | Fullscreen behaviour. |
| `overview` | table | Workspace overview appearance. |
| `errors` | table | Additional error reporting channels. |
| `xwayland` | table | X11 support through `xwayland-satellite`. |

## `general`

```lua
general = {
    mod_key = "Super",
    prefer_no_csd = true,
    gap = 8,
    opacity = 1.0,
    window_shader = "shaders/window.frag",
    animations = { ... },
    border = { ... },
    shadow = { ... },
    blur = { ... },
}
```

| Key | Default | Description |
| --- | --- | --- |
| `mod_key` | backend-dependent | Modifier that `Mod` refers to in bindings. Defaults to Super on the DRM/headless backend and Alt in a nested winit session. Values: `"ctrl"`/`"control"`, `"shift"`, `"alt"`, `"super"`/`"win"`, `"iso-level3-shift"`/`"mod5"`, `"iso-level5-shift"`/`"mod3"`. |
| `prefer_no_csd` | `true` | Advertise server-side decorations to clients. With `false` the decoration globals are hidden and clients keep their own decorations. |
| `gap` | `8` | Gap in logical pixels. Layer-shell exclusive zones and this inset define the layout work area; layouts normally use `poly.gap` between windows. Range: 0-512. |
| `opacity` | `1.0` | Default window opacity. Range: 0.0-1.0. |
| `window_shader` | unset | Path to a GLSL fragment shader used for every window. Relative paths resolve against the root `config.lua`, not an included file. |

### `general.animations`

Animations are enabled by default. `duration` is the fallback length in
milliseconds for every category; each category can override it and its easing
curve.

```lua
animations = {
    enable = true,
    duration = 180,
    slowdown = 1.0,
    window_open = { duration = 220, curve = "ease-out-expo" },
    window_close = { duration = 180, curve = "ease-out-quad" },
    window_movement = { curve = "ease-out-cubic" },
    window_resize = { curve = "ease-out-cubic" },
    workspace_switch = { curve = "ease-out-cubic" },
    overview = { curve = "ease-out-cubic" },
}
```

| Key | Default | Description |
| --- | --- | --- |
| `enable` | `true` | Master switch. When false every animation is instant. |
| `duration` | `180` | Fallback duration in milliseconds. Range: 0-10000. |
| `slowdown` | `1.0` | Multiplier applied to every duration. Range: 0.0-100.0. |
| `window_open` | `ease-out-expo` | Window open animation (fade and scale). |
| `window_close` | `ease-out-quad` | Window close animation (fade and shrink). |
| `window_movement` | `ease-out-cubic` | Moving a window without resizing it. |
| `window_resize` | `ease-out-cubic` | Resizing a window. |
| `workspace_switch` | `ease-out-cubic` | Sliding between workspaces. |
| `overview` | `ease-out-cubic` | Opening and closing the workspace overview. |

Each category accepts `duration` (milliseconds, 0-10000) and `curve`. Supported
curves are `"linear"`, `"ease-out-quad"`, `"ease-out-cubic"` and
`"ease-out-expo"`. A duration of 0 disables that category.

### `general.border`

```lua
border = {
    width = 2,
    radius = 8,
    active = "#4f8cff",
    inactive = "#3a3a3a",
}
```

| Key | Default | Description |
| --- | --- | --- |
| `width` | `0` | Border thickness in logical pixels. `0` disables the border. Range: 0-256. |
| `radius` | `0` | Corner radius in logical pixels. Range: 0-1024. |
| `active` | `#4f8cff` | Border colour of the focused window. |
| `inactive` | `#3a3a3a` | Border colour of every other window. |

### `general.shadow`

```lua
shadow = {
    enable = true,
    blur = 24,
    spread = 0,
    offset = { 0, 6 },
    color = "#000000a0",
}
```

| Key | Default | Description |
| --- | --- | --- |
| `enable` | `false` | Draw a drop shadow behind windows. |
| `blur` | `24` | Shadow blur (standard deviation) in logical pixels. Range: 0-512. |
| `spread` | `0` | Grow (or shrink, when negative) the shadow silhouette. Range: -1024 to 1024. |
| `offset` | `{ 0, 6 }` | Horizontal and vertical offset in logical pixels. Exactly two numbers. |
| `color` | `#000000a0` | Shadow colour, including alpha. |

### `general.blur`

```lua
blur = {
    enable = true,
    passes = 3,
    offset = 3.0,
    noise = 0.0,
    accept_client_regions = true,
    layer_surfaces = true,
    shader = {
        down = "shaders/blur_down.frag",
        up = "shaders/blur_up.frag",
        finish = "shaders/blur_finish.frag",
    },
}
```

| Key | Default | Description |
| --- | --- | --- |
| `enable` | `false` | Default blur state for windows. A window rule can override it per window. |
| `passes` | `3` | Number of Kawase down/up passes. Range: 0-8. |
| `offset` | `3.0` | Sampling offset in pixels. Range: 0-64. |
| `noise` | `0.0` | Dithering noise that hides banding. Range: 0.0-1.0. |
| `accept_client_regions` | `true` | Honour the blur regions clients request through `ext-background-effect-v1`. |
| `layer_surfaces` | `true` | Blur the backdrop of bars, panels and docks that request it. |
| `shader` | built in | Optional replacement shader files. Paths resolve against the root `config.lua`. |

`enable` is the master switch: a client cannot blur anything by itself. With it
on, `accept_client_regions` decides what happens when a client announces a blur
region on one of its surfaces (foot, kitty and other terminals do this to blur
their own background). With it on, that region replaces the whole-window
backdrop: only the part of the window the client asked for is blurred, and the
client can leave the rest opaque. With it off, the request is ignored and the
whole window is blurred, which keeps the compositor in charge of the look.

Either way the blur uses the parameters above and is clipped to the window and
its rounded corners. The region is only read from the window's own surface; a
client that sets one on a subsurface gets the whole-window backdrop instead.

A client that empties its region gets no backdrop at all. That is how a client
switches the blur off without destroying its background-effect object, and both
wire forms count: a null region (`set_blur_region(NULL)`, what Noctalia and most
toolkits send) and a committed region with no rectangles. The difference between
"never asked" (the configured whole-window blur) and "asked for nothing" (no
blur) matters, and panels rely on it.

Layer surfaces (the bar, panels, the dock, notification daemons) are handled
separately because there is no window rule to give them a look. They are only
blurred where the client asked: `layer_surfaces` enables the feature, but a
surface that never calls `set_blur_region` stays sharp, so a wallpaper client
cannot end up with a blurred screen. `enable` still has to be on, and
`accept_client_regions` still applies.

Shader keys are `down` (downsample), `up` (upsample) and `finish` (final pass).
A shader that fails to compile keeps the previous program and reports the error
on screen.

### Colours

Any colour setting accepts `#rgb`, `#rrggbb` or `#rrggbbaa`. The leading `#` is
required and hex digits are case-insensitive. `#rrggbbaa` carries straight
(non-premultiplied) alpha.

## `input`

```lua
input = {
    keyboard = {
        rules = "",
        model = "",
        layout = "us",
        variant = "",
        options = "grp:alt_shift_toggle",
        repeat_rate = 25,
        repeat_delay = 600,
    },
    devices = {
        ["*touchpad*"] = {
            tap = true,
            natural_scroll = true,
            accel = "adaptive",
        },
        ["*Mouse*"] = { accel = "flat" },
    },
}
```

### `input.keyboard`

| Key | Default | Description |
| --- | --- | --- |
| `rules` | `""` | xkb rules file; empty uses the xkb default. |
| `model` | `""` | xkb model. |
| `layout` | `"us"` | xkb layout, for example `"us,cn"`. |
| `variant` | `""` | xkb variant. |
| `options` | unset | xkb options, for example `"grp:alt_shift_toggle"`. |
| `repeat_rate` | `25` | Key repeat rate in events per second. `0` disables repeating. Range: 0-255. |
| `repeat_delay` | `600` | Delay before repeating starts, in milliseconds. Range: 0-10000. |

### `input.devices`

Device entries are matched against the libinput device name. Globs (`*`, `?`)
are supported, and the first matching entry wins. Unset fields keep the libinput
default. These settings only apply to the udev/libinput backend.

| Key | Type | Description |
| --- | --- | --- |
| `tap` | bool | Tap-to-click. |
| `drag` | bool | Tap-and-drag. |
| `drag_lock` | bool | Keep dragging after the finger is lifted, until the next tap. |
| `dwt` / `disable_while_typing` | bool | Disable the touchpad while typing. Both spellings set the same option. |
| `natural_scroll` | bool | Inverted scrolling. |
| `left_handed` | bool | Swap left and right buttons. |
| `click_method` | string | `"button-areas"` or `"clickfinger"`. |
| `scroll_method` | string | `"no-scroll"`, `"two-finger"`, `"edge"` or `"on-button-down"`. |
| `accel` / `accel_profile` | string | `"adaptive"` or `"flat"`. `accel_profile` wins when both are set. |
| `accel_speed` | number | Pointer acceleration speed, -1.0 to 1.0. |
| `calibration` | array | Six numbers for the affine 2x3 calibration matrix. |
| `map_to_output` | string | Parsed but currently ignored; Poly logs a warning. |

## `keybinds`

A binding maps a trigger string to an action. The trigger is a keysym name with
optional modifiers:

```lua
keybinds = {
    ["Mod+Return"] = { spawn = { "foot" } },
    ["Mod+Q"] = "close-window",
    ["Mod+Shift+Left"] = "move-window-left",
    ["XF86AudioRaiseVolume"] = { spawn = { "wpctl", "set-volume", "@DEFAULT_AUDIO_SINK@", "5%+" } },
    ["Mod+MouseLeft"] = "move-window",
    ["Mod+WheelScrollDown"] = "focus-workspace-down",
}
```

### Triggers

Modifiers are separated by `+` and are matched case-insensitively:

| Name | Meaning |
| --- | --- |
| `Mod` | `general.mod_key` |
| `Ctrl` / `Control` | Control |
| `Shift` | Shift |
| `Alt` | Alt |
| `Super` / `Win` | Super/Windows |
| `ISO_Level3_Shift` / `Mod5` | AltGr |
| `ISO_Level5_Shift` / `Mod3` | ISO level 5 shift |

The final segment is a trigger:

- Any xkb keysym name, matched case-insensitively: `Return`, `F11`, `q`,
  `XF86AudioRaiseVolume`.
- Mouse buttons: `MouseLeft`, `MouseRight`, `MouseMiddle`, `MouseBack`,
  `MouseForward`.
- Discrete wheel scroll: `WheelScrollUp`, `WheelScrollDown`,
  `WheelScrollLeft`, `WheelScrollRight`.
- Continuous touchpad scroll: `TouchpadScrollUp`, `TouchpadScrollDown`,
  `TouchpadScrollLeft`, `TouchpadScrollRight`.

Bindings are matched against the unmodified keysym, so `Mod+Shift+Q` fires on
the same physical key as `Mod+q`.

### Binding values

A binding value can be:

1. A bare action name: `"close-window"`.
2. A table with an `action` field:

   ```lua
   ["Mod+1"] = { action = { focus_workspace = 1 }, key_repeat = false },
   ```

3. A table with exactly one inline action field:

   ```lua
   ["Mod+Return"] = { spawn = { "foot" } },
   ["Mod+V"] = { spawn_shell = "foot | cat" },
   ["Mod+Tab"] = { set_layout = "dwindle" },
   ["Mod+2"] = { focus_workspace = 2 },
   ["Mod+3"] = { focus_window = 3 },
   ["Mod+4"] = { move_window_to_workspace = 4 },
   ```

Holding a binding repeats its action by default. Disable that with
`key_repeat = false` (or `["repeat"] = false`, since `repeat` is a Lua keyword).
A binding must contain exactly one action.

### Actions

Actions are shared by Lua bindings, the IPC protocol and `poly msg action`.
The kebab-case spelling below is used in configuration files.

Actions without arguments:

| Name | Effect |
| --- | --- |
| `quit` | Exit the compositor. |
| `close-window` | Ask the focused window to close. |
| `reload-config` | Reload the configuration from disk. |
| `focus-next`, `focus-prev` | Cycle through windows. |
| `focus-left`, `focus-right`, `focus-up`, `focus-down` | Focus a neighbouring window. |
| `move-window` | Start an interactive move of the window under the pointer. |
| `resize-window` | Start an interactive resize of the window under the pointer. |
| `move-window-left`, `move-window-right`, `move-window-up`, `move-window-down` | Move the focused window in the layout. |
| `toggle-floating` | Toggle the focused window between tiled and floating. |
| `toggle-fullscreen` | Toggle compositor-managed fullscreen. |
| `toggle-windowed-fullscreen` | Toggle the client's fullscreen state while the layout still places it. |
| `cycle-layout` | Switch the active workspace to the next configured layout. |
| `focus-workspace-up`, `focus-workspace-down` | Move through the output's workspace stack. |
| `move-window-to-workspace-up`, `move-window-to-workspace-down` | Move the focused window and follow it. |
| `move-workspace-up`, `move-workspace-down` | Swap the active workspace with its neighbour. |
| `toggle-overview` | Open or close the workspace overview. |

Closing or unmapping the focused window hands the keyboard to its neighbour in
the layout order: the window before it in `ctx.windows`, or the one after it
when it was the first. Floating windows hand focus to the floating neighbour
first and fall back to the topmost window of the other list. Focus therefore
stays where the window was instead of jumping to whichever window mapped last.

Actions with arguments are written as tables in Lua:

| Lua form | Effect |
| --- | --- |
| `{ spawn = { "program", "arg" } }` | Run an argv directly, without a shell. The list must not be empty. |
| `{ spawn_shell = "command line" }` | Run a line through `sh -c`. |
| `{ set_layout = "name" }` | Switch the active workspace to a named layout. |
| `{ focus_workspace = 2 }` | Focus workspace 2 (1-based, 1-512), creating it when needed. |
| `{ focus_window = 3 }` | Focus a window by its runtime id. Mostly useful from scripts. |
| `{ move_window_to_workspace = 2 }` | Move the focused window to workspace 2 and follow it. |

## `window_drag`

```lua
window_drag = {
    tiled = "reorder",
    threshold = 8,
    axis_lock = "shift",
    escape_cancels = true,
    resize_pick = "thirds",
}
```

| Key | Default | Description |
| --- | --- | --- |
| `tiled` | `"reorder"` | What dragging a tiled window does: `"reorder"` (move it in the layout), `"float"` (pull it out into a floating window) or `"ignore"`. |
| `threshold` | `8` | Pixels of pointer movement before a press becomes a drag. Must not be negative. |
| `axis_lock` | `"shift"` | Modifier that locks a drag to one axis, or `false` to disable axis locking. |
| `escape_cancels` | `true` | Let Escape cancel a drag and restore the original geometry. |
| `resize_pick` | `"thirds"` | Resize edge picker: `"thirds"` reserves the middle third of an edge as non-resizable, or a non-negative pixel distance from the border. |

## `spawn_at_startup`

A list of commands started once the compositor is up. A bare string is a shell
command; a table can run an argv directly or use a shell.

```lua
spawn_at_startup = {
    "wl-paste --watch cliphist store",
    { exec = { "waybar" } },
    { shell = "systemctl --user import-environment WAYLAND_DISPLAY" },
    { spawn = { "foot", "--server" } },
    { spawn_shell = "mako &" },
}
```

`exec` and `spawn` run the argv directly. `shell` and `spawn_shell` run through
`sh -c`. Child processes inherit `WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`,
`POLY_SOCKET`, and (once X11 is set up) `DISPLAY`.

## `environment`

Extra environment variables for every process Poly spawns:

```lua
environment = {
    MOZ_ENABLE_WAYLAND = "1",
    ELECTRON_OZONE_PLATFORM_HINT = "wayland",
}
```

Keys and values must both be strings. Use `tostring(...)` for numbers.

## `layer_rules`

Layer-shell surfaces (status bars, notification daemons, launchers) can ask for
keyboard focus. A rule can override that, which is useful for bars that would
otherwise steal focus on every click.

```lua
layer_rules = {
    {
        match = { namespace = "waybar" },
        keyboard_interactivity = "none",
    },
    {
        match = { namespace = "fuzzel" },
        keyboard_interactivity = "on-demand",
    },
}
```

| Key | Description |
| --- | --- |
| `match.namespace` | Glob matched against the layer surface namespace. Omit to match every namespace. |
| `keyboard_interactivity` | `"none"`, `"on-demand"` or `"exclusive"`. |

Rules are checked in order; the first matching rule that specifies
`keyboard_interactivity` wins. A rule must set `match.namespace`,
`keyboard_interactivity`, or both.

## `window_rules`

Rules apply placement and appearance when a window maps. They are checked in
order and the first matching rule wins for every field. `match.app_id` and
`match.title` are globs.

```lua
window_rules = {
    {
        match = { app_id = "org.gnome.Nautilus" },
        float = true,
        radius = 10,
    },
    {
        match = { app_id = "mpv" },
        float = true,
        focus = true,
        no_csd = true,
    },
    {
        match = { title = "Picture-in-Picture" },
        float = true,
        open_on_output = "DP-*",
        shadow = false,
    },
}
```

| Key | Type | Description |
| --- | --- | --- |
| `match.app_id` | string | Glob matched against the window's `app_id`. |
| `match.title` | string | Glob matched against the window title. |
| `float` | bool | Open the window floating. |
| `focus` | bool | Whether the window takes keyboard focus when it maps. The default is `true`. |
| `open_on_output` | string | Open on the first output whose name matches this glob. |
| `blur` | bool | Enable or disable blur behind the window. |
| `radius` | number | Corner radius in logical pixels. |
| `shadow` | bool | Enable or disable the window shadow. |
| `opacity` | number | Window opacity, 0.0-1.0. |
| `no_csd` | bool | Ask for server-side decorations (`true`) or let the client keep its own (`false`). Only effective while `general.prefer_no_csd` exposes the decoration globals. |
| `window_shader` | string | Per-window GLSL fragment shader, overriding `general.window_shader`. Relative to the root `config.lua`. |

A rule with no matcher and no settings is rejected because it would match every
window while doing nothing.

## `outputs`

Output settings are keyed by output name or glob. Every matching entry is
merged; the most specific entry wins field by field. Exact names are considered
more specific than globs, then patterns with more literal characters, then
alphabetical order (earlier names win ties). This lets `["*"]` provide defaults
that `["eDP-1"]` overrides.

```lua
outputs = {
    ["*"] = {
        background = "#101010",
        scale = 1.0,
    },
    ["eDP-1"] = {
        mode = "1920x1080@60",
        scale = 1.5,
        transform = "normal",
        position = { x = 0, y = 0 },
        vrr = true,
        focus = true,
        background = "#000000",
        layout_options = {
            master_width = 0.6,
            stack_on_right = true,
        },
    },
    ["DP-*"] = {
        mode = "preferred",
    },
}
```

| Key | Description |
| --- | --- |
| `mode` | `"preferred"` or `"<width>x<height>[@<hz>]"`, for example `"2560x1440@144"` and `"1920x1080"`. Refresh rates in configuration are in Hz. |
| `scale` | `"auto"` or a fractional scale from 0.1 to 4.0. `"auto"` leaves it to the backend. |
| `transform` | `"normal"`, `"90"`, `"180"`, `"270"`, `"flipped"`, `"flipped-90"`, `"flipped-180"` or `"flipped-270"`. `rotate90`/`rotate-90` style aliases are also accepted. |
| `position` | `"auto"`, `{ x = 0, y = 0 }`, or `{ 0, 0 }`. |
| `vrr` | Enable variable refresh rate. Only the DRM/KMS backend can apply it; other backends log a warning. |
| `focus` | Mark this output as the preferred focus output. The first matching output with `focus = true` wins. |
| `background` | Solid colour painted behind every window on this output. |
| `layout_options` | Primitive values (boolean, integer, number or string) exposed to Lua layouts as `ctx.options`. Entries merge across matching output patterns. |

Values the backend cannot apply are logged and skipped; the output keeps its
previous setting for that field.

## `fullscreen`

```lua
fullscreen = {
    policy = "auto",
    cover_top_layer = true,
}
```

`cover_top_layer` (default `true`) makes a compositor-managed fullscreen window
cover Top layer-shell surfaces. `policy` currently accepts only `"auto"`; other
values produce a warning and behave like `"auto"`.

## `overview`

The workspace overview is the zoomed-out stack of workspaces.

```lua
overview = {
    zoom = 0.5,
    backdrop_color = "#262626",
    card_color = "#404040",
    shadow = {
        enable = true,
        blur = 40,
        spread = 10,
        offset = { 0, 10 },
        color = "#00000050",
    },
}
```

| Key | Default | Description |
| --- | --- | --- |
| `zoom` | `0.5` | Card scale at full overview. Range: 0.0001-0.75. |
| `backdrop_color` | `#262626` | Colour behind the stack of cards. |
| `card_color` | `#404040` | Background colour of each workspace card. |
| `shadow` | see above | Card shadow; accepts the same fields as `general.shadow`. |

## `errors`

```lua
errors = {
    dbus = false,
}
```

Poly always draws configuration and runtime problems as on-screen Toasts. With
`dbus = true` it also sends them to `org.freedesktop.Notifications`. This
requires a binary built with the `dbus` feature:

```sh
cargo build --features dbus
```

Without that feature the setting has no effect; `poly validate` prints a
warning.

## `xwayland`

X11 clients are supported through an external `xwayland-satellite` process.

```lua
xwayland = {
    enable = true,
    satellite_path = "xwayland-satellite",
    on_demand = true,
    spawn_at_startup = false,
}
```

| Key | Default | Description |
| --- | --- | --- |
| `enable` | `true` | Set up X11 sockets and `DISPLAY`. When false, X11-only programs cannot start. |
| `satellite_path` | `"xwayland-satellite"` | Program name in `$PATH` or a path; `~` is expanded. Must not be empty. |
| `on_demand` | `true` | Start the satellite when the first X11 client connects, and again after it exits. When false, keep it running and restart it if it exits. |
| `spawn_at_startup` | `false` | Also start the satellite at compositor startup while `on_demand` is enabled. |

`poly msg xwayland` reports whether X11 is enabled, the current `DISPLAY`, and
whether the satellite is running.

## `layouts`

A layout is a Lua function that receives the usable layout area and the windows
in layout order and returns a rectangle for each window id.

```lua
layouts = {
    columns = function(ctx)
        local area = ctx.output.work_area
        local windows = ctx.windows
        local count = math.max(#windows, 1)
        local width = math.floor((area.w - (count - 1) * poly.gap) / count)
        local result = {}

        for index, window in ipairs(windows) do
            result[window.id] = {
                x = area.x + (index - 1) * (width + poly.gap),
                y = area.y,
                w = width,
                h = area.h,
            }
        end

        return result
    end,
}
default_layout = "columns"
```

The built-in layout is called `columns` and is always available. If no layouts
are configured, or `default_layout` is omitted, the built-in column layout is
used.

### Layout definition

A layout entry can be a function or a table:

```lua
layouts = {
    dwindle = {
        setup = function(poly)
            poly.state.order = {}
        end,
        handles_fullscreen = false,
        arrange = function(ctx)
            -- ...
        end,
    },
}
```

| Field | Description |
| --- | --- |
| `arrange` | Required. The function called whenever geometry is needed. |
| `setup` | Optional. Called once when the configuration loads, with the `poly` table. |
| `handles_fullscreen` | When true, the layout also positions compositor-managed fullscreen windows. Otherwise fullscreen windows fill their output. |
| `actions` | Optional. Table of `name = function(ctx, args) … end`, described below. |
| `keybinds` | Optional. Bindings that only apply while a workspace uses this layout. |
| `insert` | Optional. Hook deciding where a newly mapped tiled window goes. |

Layouts are per workspace. `set-layout` and `cycle-layout` change the active
workspace of the focused output. A layout function:

- runs on the compositor thread, so it must not block;
- has a budget of 200,000 VM instructions per call;
- must return a table keyed by window id, with `{ x, y, w, h }` values;
- must return positive widths and heights.

A tiled window missing from the result is arranged by the built-in column
fallback (floating windows keep their floating geometry), and ids that do not
belong to the workspace are ignored. A runtime layout error is logged and the
built-in column layout is used for that arrangement. A layout can request
another arrangement by calling `poly.layout.relayout()`.

### Layout context

`ctx` contains:

| Field | Description |
| --- | --- |
| `ctx.output.name` | Output name, for example `"eDP-1"`. |
| `ctx.output.width`, `ctx.output.height` | Usable layout area in logical pixels; equal to `work_area.w` and `work_area.h`. |
| `ctx.output.scale` | Fractional scale. |
| `ctx.output.work_area` | `{ x, y, w, h }` after layer-shell exclusive zones and `general.gap`. |
| `ctx.workspace.id` | Layout-private workspace id. It follows the workspace when the stack is reordered; like window ids, it is only valid until the compositor restarts. |
| `ctx.workspace.index` | 1-based position in the output's vertical workspace stack. |
| `ctx.windows` | Tiled windows in layout order, excluding compositor-managed fullscreen windows; described below. |
| `ctx.floating` | Array of floating window ids, bottom to top, excluding fullscreen windows. |
| `ctx.fullscreen` | Array of fullscreen window ids. |
| `ctx.focused` | Focused window id, or nil. |
| `ctx.options` | The merged `layout_options` for this output. |

### Layout actions and keybindings

A layout can expose its own action vocabulary. The handler receives the same
context as `arrange` plus a 1-based array of string arguments, and returns a
table of effects. It must not change window order or focus directly; returning
effects keeps the Rust window model authoritative.

```lua
layouts = {
    scrolling = {
        arrange = function(ctx)
            -- use poly.state[ctx.workspace.id] for per-workspace state
        end,
        actions = {
            ["focus-left"] = function(ctx)
                -- Pick the window to focus (layout-specific policy).
                local target = ctx.windows[1] and ctx.windows[1].id
                return { focus = target }
            end,
            ["move-column-left"] = function(ctx, args)
                -- Build the complete new tiled order.
                local order = {}
                for index, window in ipairs(ctx.windows) do
                    order[index] = window.id
                end
                return { order = order, relayout = true }
            end,
        },
        keybinds = {
            ["Mod+H"] = "focus-left",
            ["Mod+Shift+H"] = { "move-column-left" },
            ["Mod+Comma"] = { action = "focus-left", key_repeat = false },
        },
    },
}
default_layout = "scrolling"
```

The supported effects are:

| Field | Description |
| --- | --- |
| `focus` | Window id to focus. It must belong to the action's workspace. |
| `order` | Complete new order of the workspace's tiled window ids. It must be a permutation of the current ids; membership is owned by the compositor. |
| `switch_workspace` | 1-based workspace number on the action's output to switch to. |
| `relayout` | `true` asks for another `arrange` call even when no other effect changed. |

Unknown fields are ignored, so a layout can keep bookkeeping in the same table.
Returning nothing is allowed.

`keybinds` uses the same key spellings as the top-level `keybinds` table.
Bindings from the layout of the active workspace are checked before the global
bindings, so a layout can override `Mod+H` without touching the rest of the
configuration. A layout keybinding may only invoke an action declared in that
same layout's `actions` table; a missing name is a configuration error. Global
bindings and IPC can invoke one explicitly:

```lua
keybinds = {
    ["Mod+E"] = { layout_action = "focus-left" },
    ["Mod+Shift+E"] = { layout_action = { "move-column-left" } },
}
```

`insert` is called when a tiled window maps, before it joins the workspace. It
receives the workspace context without the new window and a window table with
the same fields as `ctx.windows` entries; its `x`/`y`/`w`/`h` are the client's
requested size clipped and centred on the work area, because no layout has
placed it yet. Return a 1-based position in the tiled order, or nothing to
append:

```lua
insert = function(ctx, window)
    return #ctx.windows + 1
end
```

The hook only runs for named Lua layouts; the built-in `columns` layout always
appends.

After the window joins the workspace, `arrange` runs once before the window is
mapped, and the rectangle it returns is where the window opens. A mapping
window therefore fades in at its final slot and does not flash at a centered
position first.

[`docs/examples/scrolling.lua`](examples/scrolling.lua) is a complete
one-window-per-column scrolling layout built from these pieces: per-workspace
width, viewport and maximized state, focused-column following, column
reordering, resizing and niri-style `maximize-column`. It is the reference for
extending the same state to multi-window columns and consume/expel.

Each entry in `ctx.windows` has:

| Field | Description |
| --- | --- |
| `id` | Stable window id, shared with IPC. |
| `app_id`, `title` | Client metadata, when set. |
| `focused` | Whether this window has keyboard focus. |
| `x`, `y`, `w`, `h` | Current geometry. |
| `geometry` | The same geometry as `{ x, y, w, h }`. |
| `min_size`, `max_size` | Client size constraints as `{ w, h }`; `0` means unconstrained. |

### Layout helpers

| Helper | Description |
| --- | --- |
| `poly.gap` | The configured `general.gap`, for spacing between windows. |
| `poly.state` | A table private to the current layout. It persists across setup and later calls. |
| `poly.layout.relayout()` | Request another arrangement after state changed. |

## Complete example

```lua
-- ~/.config/poly/config.lua
return {
    general = {
        mod_key = "Super",
        gap = 8,
        prefer_no_csd = true,
        border = {
            width = 2,
            radius = 8,
            active = "#4f8cff",
            inactive = "#3a3a3a",
        },
        shadow = { enable = true, blur = 24, offset = { 0, 6 } },
        animations = { duration = 180, slowdown = 1.0 },
    },

    input = {
        keyboard = { layout = "us" },
        devices = {
            ["*touchpad*"] = { natural_scroll = true, tap = true, accel = "adaptive" },
            ["*Mouse*"] = { accel = "flat" },
        },
    },

    keybinds = {
        ["Mod+Return"] = { spawn = { "foot" } },
        ["Mod+Q"] = "close-window",
        ["Mod+Space"] = "toggle-floating",
        ["Mod+F"] = "toggle-fullscreen",
        ["Mod+Tab"] = "cycle-layout",
        ["Mod+Left"] = "focus-left",
        ["Mod+Right"] = "focus-right",
        ["Mod+Shift+Left"] = "move-window-left",
        ["Mod+Shift+Right"] = "move-window-right",
        ["Mod+1"] = { focus_workspace = 1 },
        ["Mod+2"] = { focus_workspace = 2 },
        ["Mod+O"] = "toggle-overview",
        ["Mod+R"] = "reload-config",
        ["Mod+MouseLeft"] = "move-window",
        ["Mod+MouseRight"] = "resize-window",
    },

    window_drag = {
        tiled = "reorder",
        threshold = 8,
        axis_lock = "shift",
        escape_cancels = true,
    },

    window_rules = {
        { match = { app_id = "mpv" }, float = true, focus = true },
        { match = { app_id = "org.gnome.Nautilus" }, float = true, radius = 10 },
    },

    layer_rules = {
        { match = { namespace = "waybar" }, keyboard_interactivity = "none" },
    },

    outputs = {
        ["*"] = { background = "#101010" },
        ["eDP-1"] = { mode = "preferred", scale = "auto", focus = true },
    },

    overview = { zoom = 0.5 },

    spawn_at_startup = {
        { exec = { "waybar" } },
    },

    environment = {
        MOZ_ENABLE_WAYLAND = "1",
    },

    layouts = {
        columns = function(ctx)
            local area = ctx.output.work_area
            local count = math.max(#ctx.windows, 1)
            local width = math.floor((area.w - (count - 1) * poly.gap) / count)
            local result = {}
            for index, window in ipairs(ctx.windows) do
                result[window.id] = {
                    x = area.x + (index - 1) * (width + poly.gap),
                    y = area.y,
                    w = width,
                    h = area.h,
                }
            end
            return result
        end,
    },
    default_layout = "columns",

    xwayland = { enable = true, on_demand = true },
}
```

## Troubleshooting

- Run `poly validate` after editing; it prints the resolved path, warnings and
  errors.
- Unknown keys are warnings. Check the log if a setting seems to have no
  effect.
- If the screen is covered by Toasts after a reload, the new file was rejected
  and the previous configuration is still active.
- Output mode and scale values the backend cannot apply are logged; the output
  keeps its previous value.
- `poly msg action reload-config` reloads the configuration the same way as
  saving the file.
