# Configuration

Poly is configured with a JavaScript file. The file is executed once and calls
flat helper functions that collect the configuration; nothing is returned.

```js
config({
  general: {
    gap: 8,
  },
});

bind("Mod+Return", { spawn: ["foot"] });
```

The helpers may be called more than once and from included files: `config()`
merges objects, `bind()` replaces a binding with the same trigger, and
`window_rule()`, `layer_rule()` and `startup()` append.

Configuration problems are collected instead of failing fast. Unknown settings
are reported as warnings, invalid values as errors. A file with errors is not
applied; on reload the previous configuration stays in effect. Errors are shown
as on-screen Toasts and written to the log. For the runtime control socket, see
[ipc.md](ipc.md).

## Locating the file

The configuration is looked up in this order:

1. `--config <path>` on the command line
2. `$POLY_CONFIG`
3. `$XDG_CONFIG_HOME/poly/config.js` (or `~/.config/poly/config.js`)
4. `/etc/poly/config.js`

If none exists, Poly starts with built-in defaults and writes an annotated
`config.js` to the user location, so there is a file to edit. Use
`poly validate` to locate and validate the file without starting the compositor:

```sh
poly validate
poly validate --config ./config.js
```

The root file and every file loaded with `include()` are watched. Saving any of
them reloads the configuration immediately. A broken edit never takes down the
running session.

There is no Lua loader: a file with a `.lua` extension, or one passed to
`--config`, is evaluated as JavaScript and fails with a syntax error if it was
written for the old syntax. The script may use the standard JavaScript built-ins
(`Math`, `JSON`, `Object`, `Array`, …), but not `require`, network access or the
filesystem beyond `include()` and `read_file()`.

## Generated files

Programs that generate settings (a wallpaper-driven palette, a theme switcher)
own a file of their own and write the helpers they care about; `config.js` only
has to import it:

```js
// ~/.config/poly/palette.js, written by an external program
config({
  general: {
    border: { active: "#8ab4f8", inactive: "#3c4043" },
  },
});
```

```js
// ~/.config/poly/config.js
include("palette.js");
```

The include runs in the same global scope, so it can call `config()`, `bind()`
or any other helper. It is part of the watched set: rewriting it while Poly runs
reloads the configuration within half a second, and the generated file is the
only thing the external program has to write. A palette file should not be
deleted while `config.js` still includes it; a missing include rejects the
configuration.

Objects passed to `config()` merge key by key, so the palette above changes the
border colours and keeps the width and radius configured elsewhere; arrays
(`window_rules`, `layer_rules`, `spawn_at_startup`) are replaced wholesale
instead of being mixed entry by entry, and objects keyed by name, such as
`keybinds` and `outputs`, merge like any other object. A file with a syntax
error rejects the whole configuration and leaves the previous one running.

## JavaScript environment

The following helpers are available while the configuration is evaluated:

| Helper | Description |
| --- | --- |
| `config(object)` | Merge `object` into the configuration. May be called repeatedly. |
| `bind(keys, action, options?)` | Bind a trigger to an action; see [keybinds](#keybinds). |
| `output(pattern, object)` | Set display settings for an output name or glob. |
| `window_rule(object)` | Append a rule applied when matching windows map. |
| `layer_rule(object)` | Append a rule for layer-shell surfaces. |
| `env(name, value)` | Add an environment variable for every child process. |
| `startup(command)` | Start a command once the compositor is up. |
| `layout(name, definition)` | Define a layout; see [layouts](#layouts). |
| `default_layout(name)` | Layout used by new workspaces. |
| `include(path)` | Execute another JavaScript file and return its completion value. Paths are relative to the file that calls `include`; `~` is expanded. Includes are watched and may nest up to 32 levels. |
| `read_file(path)` | Return the contents of a data file as a string. Paths resolve like `include`; data files are not watched. A missing file throws. |
| `print(...)` | Write its arguments to the Poly log at `info` level. |

Inside layout functions, `ctx` additionally exposes `ctx.gap`, `ctx.state` and
`ctx.relayout()`; see [layouts](#layouts).

An accidental infinite loop cannot hang the compositor: evaluating the
configuration (the root file and every include) has a budget of one second, each
layout call has ten milliseconds, and the runtime is limited to 64 MiB of memory
and a 1 MiB stack. Exceeding a budget rejects the configuration, or makes that
one layout call fall back to the built-in column layout.

## Configuration keys and their helpers

The object passed to `config()` accepts these keys. The helper column shows the
function that writes the same key without repeating the surrounding object.

| Key | Helper | Purpose |
| --- | --- | --- |
| `general` | `config()` | Gaps, decorations, animations, opacity, shaders and the pointer. |
| `input` | `config()` | Keyboard layout and libinput device settings. |
| `window_drag` | `config()` | Interactive move and resize behaviour. |
| `fullscreen` | `config()` | Fullscreen behaviour. |
| `errors` | `config()` | Additional error reporting channels. |
| `xwayland` | `config()` | X11 support through `xwayland-satellite`. |
| `keybinds` | `bind()` | Bindings from key, mouse button or scroll trigger to actions. |
| `spawn_at_startup` | `startup()` | Commands started once the compositor is up. |
| `environment` | `env()` | Extra environment variables for child processes. |
| `layer_rules` | `layer_rule()` | Rules for layer-shell surfaces (bars, launchers, lock screens). |
| `window_rules` | `window_rule()` | Per-window placement and appearance rules. |
| `outputs` | `output()` | Per-output mode, scale, transform, position and background. |
| `default_layout` | `default_layout()` | Layout used by new workspaces. |

Layouts are not a key: they are registered with `layout(name, definition)`.

## `general`

```js
config({
  general: {
    mod_key: "Super",
    prefer_no_csd: true,
    gap: 8,
    opacity: 1.0,
    window_shader: "shaders/window.frag",
    animations: {
      /* … */
    },
    border: {
      /* … */
    },
    shadow: {
      /* … */
    },
    blur: {
      /* … */
    },
    cursor: {
      /* … */
    },
    enable_overlay_planes: false,
  },
});
```

| Key | Default | Description |
| --- | --- | --- |
| `mod_key` | backend-dependent | Modifier that `Mod` refers to in bindings. Defaults to Super on the DRM/headless backend and Alt in a nested winit session. Values: `"ctrl"`/`"control"`, `"shift"`, `"alt"`, `"super"`/`"win"`, `"iso-level3-shift"`/`"mod5"`, `"iso-level5-shift"`/`"mod3"`. |
| `prefer_no_csd` | `true` | Advertise server-side decorations to clients. With `false` the decoration globals are hidden and clients keep their own decorations. |
| `gap` | `8` | Gap in logical pixels. Layer-shell exclusive zones and this inset define the layout work area; layouts normally use `ctx.gap` between windows. Range: 0-512. |
| `opacity` | `1.0` | Default window opacity. Range: 0.0-1.0. |
| `window_shader` | unset | Path to a GLSL fragment shader used for every window. Relative paths resolve against the root `config.js`, not an included file. |
| `enable_overlay_planes` | `false` | Let the DRM backend hand elements to overlay planes instead of compositing them onto the primary plane. Off by default: plane behaviour varies by driver and a misbehaving plane shows up as flicker or a stuck strip rather than an error. Enable it to experiment on a known-good machine. Only meaningful on the native backend; ignored when nested under another compositor. |

### Scan-out

The DRM backend scans out directly whenever it can, without a setting: a frame
whose only content is one fullscreen window that covers the output opaquely is
handed to the primary plane as the client's own buffer, and the pointer goes to
a hardware cursor plane when the cursor is the topmost element. Everything else
— windowed layouts, rounded corners, borders, shadows, blur, Toasts, layer
surfaces, transitions — is composited into the swapchain as usual.

A few consequences worth knowing:

- A compositor-managed fullscreen window gets scan-out; its opening animation,
  border, shadow and blur backdrop are all skipped while it is fullscreen, which
  is exactly what makes it scannable.
- A fullscreen window is drawn over a black background, but that background is
  the frame's *clear colour* rather than a solid element. It has to be there so a
  client that has not painted its first frame cannot leak the session content
  behind it, and a clear colour provides the same guarantee without occupying a
  slot in the element stack — a solid element would sit below the window and be
  the only element the plane assignment considers.
- Toggling `general.border`, `general.shadow` or a `window_shader` does not
  affect a fullscreen window, so it does not affect scan-out either.
- Screenshot and screencast frames use the same clear colour, so a capture of a
  fullscreen window shows the same black background the output shows.

### Variable refresh rate

On the native backend VRR is enabled automatically when the connector and CRTC
report support for it, so there is nothing to configure. Two consequences:

- Animations are no longer sampled on the nominal refresh grid. Under VRR a frame
  is presented when it is ready rather than on a fixed VBlank, so a frame's
  animation time is its own presentation time; sampling a fixed grid would make
  an animation run at the wrong speed whenever the panel is not at its nominal
  rate.
- A frame whose only change is the pointer is skipped, so moving the mouse over
  an idle desktop does not hold the refresh rate up. This needs a usable cursor
  plane; without one the pointer is part of the composited frame and the frame
  has to be drawn.

Outputs whose VRR support would require a modeset are left at a fixed rate:
blanking the output on every plug-in is a worse trade than the missing
smoothness.

### `general.animations`

Animations are enabled by default. `duration` is the fallback length in
milliseconds for every category; each category can override it and its easing
curve.

```js
config({
  general: {
    animations: {
      enable: true,
      duration: 180,
      slowdown: 1.0,
      window_open: { duration: 220, curve: "ease-out-expo" },
      window_close: { duration: 180, curve: "ease-out-quad" },
      window_movement: { curve: "ease-out-cubic" },
      window_resize: { curve: "ease-out-cubic" },
      workspace_switch: { curve: "ease-out-cubic" },
    },
  },
});
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

Each category accepts `duration` (milliseconds, 0-10000) and `curve`. Supported
curves are `"linear"`, `"ease-out-quad"`, `"ease-out-cubic"` and
`"ease-out-expo"`. A duration of 0 disables that category.

### `general.border`

```js
config({
  general: {
    border: {
      width: 2,
      radius: 8,
      active: "#4f8cff",
      inactive: "#3a3a3a",
    },
  },
});
```

| Key | Default | Description |
| --- | --- | --- |
| `width` | `0` | Border thickness in logical pixels. `0` disables the border. Range: 0-256. |
| `radius` | `0` | Corner radius in logical pixels. Range: 0-1024. |
| `active` | `#4f8cff` | Border colour of the focused window. |
| `inactive` | `#3a3a3a` | Border colour of every other window. |

### `general.shadow`

```js
config({
  general: {
    shadow: {
      enable: true,
      blur: 24,
      spread: 0,
      offset: [0, 6],
      color: "#000000a0",
    },
  },
});
```

| Key | Default | Description |
| --- | --- | --- |
| `enable` | `false` | Draw a drop shadow behind windows. |
| `blur` | `24` | Shadow blur (standard deviation) in logical pixels. Range: 0-512. |
| `spread` | `0` | Grow (or shrink, when negative) the shadow silhouette. Range: -1024 to 1024. |
| `offset` | `[0, 6]` | Horizontal and vertical offset in logical pixels. Exactly two numbers. |
| `color` | `#000000a0` | Shadow colour, including alpha. |

### `general.blur`

```js
config({
  general: {
    blur: {
      enable: true,
      passes: 3,
      offset: 3.0,
      noise: 0.0,
      accept_client_regions: true,
      layer_surfaces: true,
      xray: true,
      shader: {
        down: "shaders/blur_down.frag",
        up: "shaders/blur_up.frag",
        finish: "shaders/blur_finish.frag",
      },
    },
  },
});
```

| Key | Default | Description |
| --- | --- | --- |
| `enable` | `false` | Default blur state for windows. A window rule can override it per window. |
| `passes` | `3` | Number of Kawase down/up passes. Range: 0-8. |
| `offset` | `3.0` | Sampling offset in pixels. Range: 0-64. |
| `noise` | `0.0` | Dithering noise that hides banding. Range: 0.0-1.0. |
| `accept_client_regions` | `true` | Honour the blur regions clients request through `ext-background-effect-v1`. |
| `layer_surfaces` | `true` | Blur the backdrop of bars, panels and docks that request it. |
| `xray` | `true` | Blur only the wallpaper instead of every window below the blurred surface. |
| `shader` | built in | Optional replacement shader files. Paths resolve against the root `config.js`. |

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

`xray` changes what the backdrop contains. With it on (the default) the
compositor blurs the wallpaper once per output, and every blurred window, bar or
panel samples that shared copy. A blurred window then shows the wallpaper rather
than the windows behind it, and moving or animating a window does not re-run the
blur: only a change of the wallpaper, of the blur parameters or of the output
geometry does.

The shared copy is made of the output background and the `background` and
`bottom` layer surfaces, which is why those layers keep no client-requested blur
of their own while `xray` is on: their content is what everything else samples.
Put a translucent bar on the `top` layer if it should blur.

With it off, each blurred surface captures everything below it, including
other windows. That is the classic behaviour, but it costs one blur per
surface and repaints the captured area whenever anything behind the surface
changes.

Shader keys are `down` (downsample), `up` (upsample) and `finish` (final pass).
A shader that fails to compile keeps the previous program and reports the error
on screen.

### `general.cursor`

```js
config({
  general: {
    cursor: {
      theme: "Adwaita",
      size: 32,
    },
  },
});
```

| Key | Default | Description |
| --- | --- | --- |
| `theme` | `$XCURSOR_THEME`, else `default` | xcursor theme name, looked up in the usual cursor directories (`$XDG_DATA_HOME/icons`, `~/.icons`, `/usr/share/icons`). |
| `size` | `$XCURSOR_SIZE`, else `24` | Nominal pointer size in logical pixels. Range: 1-256. |

Poly draws the pointer itself, so `size` is what ends up on screen: the icon is
picked out of the theme at `size × output scale`, which is how the same number
behaves under `XCURSOR_SIZE` everywhere else. Leaving a field unset keeps
following the environment variable.

Both values are also exported as `XCURSOR_THEME` and `XCURSOR_SIZE` to spawned
children, so toolkits and X11 applications that draw their own pointer agree
with the compositor; an explicit `env()` still wins over them.
Editing either setting applies on the next reload — the pointer is reloaded but
not restarted, so an animation keeps its frame when the setting did not change.

### Colours

Any colour setting accepts `#rgb`, `#rrggbb` or `#rrggbbaa`. The leading `#` is
required and hex digits are case-insensitive. `#rrggbbaa` carries straight
(non-premultiplied) alpha.

## `input`

```js
config({
  input: {
    keyboard: {
      rules: "",
      model: "",
      layout: "us",
      variant: "",
      options: "grp:alt_shift_toggle",
      repeat_rate: 25,
      repeat_delay: 600,
    },
    devices: {
      "*touchpad*": {
        tap: true,
        natural_scroll: true,
        accel: "adaptive",
      },
      "*Mouse*": { accel: "flat" },
    },
  },
});
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

A binding maps a trigger string to an action:

```js
bind("Mod+Return", { spawn: ["foot"] });
bind("Mod+Q", "close-window");
bind("Mod+Shift+Left", "move-window-left");
bind("XF86AudioRaiseVolume", {
  spawn: ["wpctl", "set-volume", "@DEFAULT_AUDIO_SINK@", "5%+"],
});
bind("Mod+MouseLeft", "move-window");
bind("Mod+WheelScrollDown", "focus-workspace-down");
```

The trigger is a keysym name with optional modifiers.

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

The action (second argument) can be:

1. A bare action name: `"close-window"`.
2. An object with an `action` field:

   ```js
   bind("Mod+1", { action: { focus_workspace: 1 }, key_repeat: false });
   ```

3. An object with exactly one inline action field:

   ```js
   bind("Mod+Return", { spawn: ["foot"] });
   bind("Mod+V", { spawn_shell: "foot | cat" });
   bind("Mod+Tab", { set_layout: "dwindle" });
   bind("Mod+2", { focus_workspace: 2 });
   bind("Mod+3", { focus_window: 3 });
   bind("Mod+4", { move_window_to_workspace: 4 });
   ```

Binding options are an optional third argument, or fields next to `action` in
form 2:

```js
bind("Mod+R", "resize-window", { key_repeat: false });
bind("Mod+E", "move-window-right", { repeat: false });
```

Holding a binding repeats its action by default. Disable that with
`key_repeat: false` (or `repeat: false`). A binding must contain exactly one
action.

### Actions

Actions are shared by JavaScript bindings, the IPC protocol and `poly msg
action`. The kebab-case spelling below is used in configuration files.

Actions without arguments:

| Name | Effect |
| --- | --- |
| `quit` | Exit the compositor. |
| `close-window` | Ask the focused window to close. |
| `reload-config` | Reload the configuration from disk. |
| `focus-next`, `focus-prev` | Cycle through windows. |
| `focus-left`, `focus-right`, `focus-up`, `focus-down` | Focus a neighbouring window. |
| `minimize-window` | Minimize the focused window. |
| `restore-window` | Restore the most recently minimized window of the active workspace and focus it. |
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

Closing or unmapping the focused window hands the keyboard to its neighbour in
the layout order: the window before it in `ctx.windows`, or the one after it
when it was the first. Floating windows hand focus to the floating neighbour
first and fall back to the topmost window of the other list. Focus therefore
stays where the window was instead of jumping to whichever window mapped last.

Actions with arguments are written as objects:

| Form | Effect |
| --- | --- |
| `{ spawn: ["program", "arg"] }` | Run an argv directly, without a shell. The list must not be empty. |
| `{ spawn_shell: "command line" }` | Run a line through `sh -c`. |
| `{ set_layout: "name" }` | Switch the active workspace to a named layout. |
| `{ focus_workspace: 2 }` | Focus workspace 2 (1-based, 1-512), creating it when needed. |
| `{ focus_window: 3 }` | Focus a window by its runtime id. Mostly useful from scripts. |
| `{ minimize_window: true }` | Minimize the focused window. `{ minimize_window: 3 }` minimizes a window by id. |
| `{ restore_window: true }` | Restore and focus the most recently minimized window of the active workspace. `{ restore_window: 3 }` restores a window by id. |
| `{ move_window_to_workspace: 2 }` | Move the focused window to workspace 2 and follow it. |

A minimized window keeps its workspace, its position in the layout and its
entry in the IPC window list, but the compositor stops drawing it and the layout
reflows over its slot. Focusing it (from a taskbar, a keybinding or
`focus-window`) restores it. Closing a minimized window skips the close
animation, because there is no last frame to animate from.

## `window_drag`

```js
config({
  window_drag: {
    tiled: "reorder",
    threshold: 8,
    axis_lock: "shift",
    escape_cancels: true,
    resize_pick: "thirds",
    throttle_resize: true,
  },
});
```

| Key | Default | Description |
| --- | --- | --- |
| `tiled` | `"reorder"` | What dragging a tiled window does: `"reorder"` (move it in the layout), `"float"` (pull it out into a floating window) or `"ignore"`. |
| `threshold` | `8` | Pixels of pointer movement before a press becomes a drag. Must not be negative. |
| `axis_lock` | `"shift"` | Modifier that locks a drag to one axis, or `false` to disable axis locking. |
| `escape_cancels` | `true` | Let Escape cancel a drag and restore the original geometry. |
| `resize_pick` | `"thirds"` | Resize edge picker: `"thirds"` reserves the middle third of an edge as non-resizable, or a non-negative pixel distance from the border. |
| `throttle_resize` | `true` | Ask for at most one new window size per client acknowledgement while resizing. Turn it off only for clients that resize badly when a configure is withheld. |

## `spawn_at_startup`

Commands started once the compositor is up, registered with `startup()`. A
string is a shell command; an object runs an argv directly or uses a shell.

```js
startup("wl-paste --watch cliphist store");
startup({ exec: ["waybar"] });
startup({ shell: "systemctl --user import-environment WAYLAND_DISPLAY" });
startup({ spawn: ["foot", "--server"] });
startup({ spawn_shell: "mako &" });
```

`exec` and `spawn` run the argv directly. `shell` and `spawn_shell` run through
`sh -c`. Child processes inherit `WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`,
`POLY_SOCKET`, and (once X11 is set up) `DISPLAY`.

## `environment`

Extra environment variables for every process Poly spawns:

```js
env("MOZ_ENABLE_WAYLAND", "1");
env("ELECTRON_OZONE_PLATFORM_HINT", "wayland");
```

Keys must be strings. Values are strings; numbers and booleans are converted
with their JavaScript spelling (`env("POLY_DEBUG", true)` sets `"true"`).

## `layer_rules`

Layer-shell surfaces (status bars, notification daemons, launchers) can ask for
keyboard focus. A rule can override that, which is useful for bars that would
otherwise steal focus on every click.

```js
layer_rule({
  match: { namespace: "waybar" },
  keyboard_interactivity: "none",
});
layer_rule({
  match: { namespace: "fuzzel" },
  keyboard_interactivity: "on-demand",
});
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

```js
window_rule({
  match: { app_id: "org.gnome.Nautilus" },
  float: true,
  radius: 10,
});
window_rule({
  match: { app_id: "mpv" },
  float: true,
  focus: true,
  no_csd: true,
});
window_rule({
  match: { title: "Picture-in-Picture" },
  float: true,
  open_on_output: "DP-*",
  shadow: false,
});
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
| `window_shader` | string | Per-window GLSL fragment shader, overriding `general.window_shader`. Relative to the root `config.js`. |

A rule with no matcher and no settings is rejected because it would match every
window while doing nothing.

## `outputs`

Output settings are keyed by output name or glob and registered with
`output(pattern, settings)`. Every matching entry is merged; the most specific
entry wins field by field. Exact names are considered more specific than globs,
then patterns with more literal characters, then alphabetical order (earlier
names win ties). This lets `"*"` provide defaults that `"eDP-1"` overrides.

```js
output("*", {
  background: "#101010",
  scale: 1.0,
});

output("eDP-1", {
  mode: "1920x1080@60",
  scale: 1.5,
  transform: "normal",
  position: { x: 0, y: 0 },
  vrr: true,
  focus: true,
  background: "#000000",
  layout_options: {
    master_width: 0.6,
    stack_on_right: true,
  },
});

output("DP-*", {
  mode: "preferred",
});
```

| Key | Description |
| --- | --- |
| `mode` | `"preferred"` or `"<width>x<height>[@<hz>]"`, for example `"2560x1440@144"` and `"1920x1080"`. Refresh rates in configuration are in Hz. |
| `scale` | `"auto"` or a fractional scale from 0.1 to 4.0. `"auto"` leaves it to the backend. |
| `transform` | `"normal"`, `"90"`, `"180"`, `"270"`, `"flipped"`, `"flipped-90"`, `"flipped-180"` or `"flipped-270"`. `rotate90`/`rotate-90` style aliases are also accepted. |
| `position` | `"auto"`, `{ x: 0, y: 0 }`, or `[0, 0]`. |
| `vrr` | Enable variable refresh rate. Only the DRM/KMS backend can apply it; other backends log a warning. |
| `focus` | Mark this output as the preferred focus output. The first matching output with `focus: true` wins. |
| `background` | Solid colour painted behind every window on this output. |
| `layout_options` | Primitive values (boolean, integer, number or string) exposed to layouts as `ctx.options`. Entries merge across matching output patterns. |

Values the backend cannot apply are logged and skipped; the output keeps its
previous setting for that field.

## `fullscreen`

```js
config({
  fullscreen: {
    policy: "auto",
    cover_top_layer: true,
  },
});
```

`cover_top_layer` (default `true`) makes a compositor-managed fullscreen window
cover Top layer-shell surfaces. `policy` currently accepts only `"auto"`; other
values produce a warning and behave like `"auto"`.

## `errors`

```js
config({
  errors: {
    dbus: false,
  },
});
```

Poly always draws configuration and runtime problems as on-screen Toasts. With
`dbus: true` it also sends them to `org.freedesktop.Notifications`. This
requires a binary built with the `dbus` feature:

```sh
cargo build --features dbus
```

Without that feature the setting has no effect; `poly validate` prints a
warning.

## `xwayland`

X11 clients are supported through an external `xwayland-satellite` process.

```js
config({
  xwayland: {
    enable: true,
    satellite_path: "xwayland-satellite",
    on_demand: true,
    spawn_at_startup: false,
  },
});
```

| Key | Default | Description |
| --- | --- | --- |
| `enable` | `true` | Set up X11 sockets and `DISPLAY`. When false, X11-only programs cannot start. |
| `satellite_path` | `"xwayland-satellite"` | Program name in `$PATH` or a path; `~` is expanded. Must not be empty. |
| `on_demand` | `true` | Start the satellite when the first X11 client connects, and again after it exits. When false, keep it running and restart it if it exits. |
| `spawn_at_startup` | `false` | Also start the satellite at compositor startup while `on_demand` is enabled. |

`poly msg xwayland` reports whether X11 is enabled, the current `DISPLAY`, and
whether the satellite is running.

## Layouts

A layout is a JavaScript function that receives a context object and returns a
rectangle for each window id that it wants to place.

```js
layout("columns", (ctx) => {
  const area = ctx.output.work_area;
  const count = Math.max(ctx.windows.length, 1);
  const width = Math.floor((area.w - (count - 1) * ctx.gap) / count);
  const result = {};

  ctx.windows.forEach((window, index) => {
    result[window.id] = {
      x: area.x + index * (width + ctx.gap),
      y: area.y,
      w: width,
      h: area.h,
    };
  });

  return result;
});

default_layout("columns");
```

The built-in layout is called `columns` and is always available. If no layouts
are configured, or `default_layout()` is not called, the built-in column layout
is used. Calling `layout()` twice with the same name is an error.

### Layout definition

`layout(name, definition)` takes a function or a definition object:

```js
layout("dwindle", {
  setup(ctx) {
    ctx.state.order = [];
  },
  handles_fullscreen: false,
  arrange(ctx) {
    // …
  },
});
```

| Field | Description |
| --- | --- |
| `arrange` | Required. The function called whenever geometry is needed. |
| `setup` | Optional. Called once when the configuration loads, with the same `ctx` (without windows). |
| `handles_fullscreen` | When true, the layout also positions compositor-managed fullscreen windows. Otherwise fullscreen windows fill their output. |
| `actions` | Optional. Object of `name(ctx, args) { … }`, described below. |
| `keybinds` | Optional. Bindings that only apply while a workspace uses this layout. |
| `insert` | Optional. Hook deciding where a newly mapped tiled window goes. |

Layouts are per workspace. `set-layout` and `cycle-layout` change the active
workspace of the focused output. A layout function:

- runs on the compositor thread, so it must not block or start threads;
- has a budget of ten milliseconds per call;
- must return an object keyed by window id, with `{ x, y, w, h }` values;
- must return positive widths and heights.

An array is rejected: the window ids are the keys of the result, not its
positions. A tiled window missing from the result is arranged by the built-in
column fallback (floating windows keep their floating geometry), and ids that do
not belong to the workspace are ignored. A runtime layout error is logged and
the built-in column layout is used for that arrangement. A layout can request
another arrangement by calling `ctx.relayout()`.

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
| `ctx.focused` | Focused window id, or `undefined`. |
| `ctx.options` | The merged `layout_options` for this output. |

### Layout actions and keybindings

A layout can expose its own action vocabulary. The handler receives the same
context as `arrange` plus an array of string arguments, and returns an object of
effects. It must not change window order or focus directly; returning effects
keeps the Rust window model authoritative.

```js
layout("scrolling", {
  arrange(ctx) {
    // use ctx.state.workspaces[ctx.workspace.id] for per-workspace state
  },
  actions: {
    "focus-left"(ctx) {
      // Pick the window to focus (layout-specific policy).
      const target = ctx.windows[0]?.id;
      return { focus: target };
    },
    "move-column-left"(ctx) {
      // Build the complete new tiled order.
      return {
        order: ctx.windows.map((window) => window.id),
        relayout: true,
      };
    },
  },
  keybinds: {
    "Mod+H": "focus-left",
    "Mod+Shift+H": { action: "move-column-left" },
    "Mod+Comma": { action: "focus-left", key_repeat: false },
  },
});

default_layout("scrolling");
```

The supported effects are:

| Field | Description |
| --- | --- |
| `focus` | Window id to focus. It must belong to the action's workspace. |
| `order` | Complete new order of the workspace's tiled window ids. It must be a permutation of the current ids; membership is owned by the compositor. |
| `switch_workspace` | 1-based workspace number on the action's output to switch to. |
| `relayout` | `true` asks for another `arrange` call even when no other effect changed. |

Unknown fields are ignored, so a layout can keep bookkeeping in the same object.
Returning nothing is allowed.

`keybinds` uses the same key spellings and binding forms as `bind()`. Bindings
from the layout of the active workspace are checked before the global bindings,
so a layout can override `Mod+H` without touching the rest of the configuration.
A layout keybinding may only invoke an action declared in that same layout's
`actions` object; a missing name is a configuration error. Global bindings and
IPC can invoke one explicitly:

```js
bind("Mod+E", { layout_action: "focus-left" });
bind("Mod+Shift+E", { layout_action: ["move-column-left"] });
```

`insert` is called when a tiled window maps, before it joins the workspace. It
receives the workspace context without the new window and a window object with
the same fields as `ctx.windows` entries; its `x`/`y`/`w`/`h` are the client's
requested size clipped and centred on the work area, because no layout has
placed it yet. Return a 1-based position in the tiled order, or nothing to
append:

```js
insert(ctx, window) {
  return ctx.windows.length + 1;
}
```

The hook only runs for named layouts; the built-in `columns` layout always
appends.

After the window joins the workspace, `arrange` runs once before the window is
mapped, and the rectangle it returns is where the window opens. A mapping
window therefore fades in at its final slot and does not flash at a centered
position first.

[`docs/examples/scrolling.js`](examples/scrolling.js) is a complete
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
| `ctx.gap` | The configured `general.gap`, for spacing between windows. |
| `ctx.state` | An object private to the current layout. It persists across `setup` and later calls. |
| `ctx.relayout()` | Request another arrangement after state changed. |

## Complete example

```js
// ~/.config/poly/config.js
config({
  general: {
    mod_key: "Super",
    gap: 8,
    prefer_no_csd: true,
    border: {
      width: 2,
      radius: 8,
      active: "#4f8cff",
      inactive: "#3a3a3a",
    },
    shadow: { enable: true, blur: 24, offset: [0, 6] },
    animations: { duration: 180, slowdown: 1.0 },
  },

  input: {
    keyboard: { layout: "us" },
    devices: {
      "*touchpad*": { natural_scroll: true, tap: true, accel: "adaptive" },
      "*Mouse*": { accel: "flat" },
    },
  },

  window_drag: {
    tiled: "reorder",
    threshold: 8,
    axis_lock: "shift",
    escape_cancels: true,
  },

  xwayland: { enable: true, on_demand: true },
});

bind("Mod+Return", { spawn: ["foot"] });
bind("Mod+Q", "close-window");
bind("Mod+Space", "toggle-floating");
bind("Mod+F", "toggle-fullscreen");
bind("Mod+Tab", "cycle-layout");
bind("Mod+Left", "focus-left");
bind("Mod+Right", "focus-right");
bind("Mod+Shift+Left", "move-window-left");
bind("Mod+Shift+Right", "move-window-right");
bind("Mod+1", { focus_workspace: 1 });
bind("Mod+2", { focus_workspace: 2 });
bind("Mod+R", "reload-config");
bind("Mod+MouseLeft", "move-window");
bind("Mod+MouseRight", "resize-window");

window_rule({ match: { app_id: "mpv" }, float: true, focus: true });
window_rule({ match: { app_id: "org.gnome.Nautilus" }, float: true, radius: 10 });

layer_rule({ match: { namespace: "waybar" }, keyboard_interactivity: "none" });

output("*", { background: "#101010" });
output("eDP-1", { mode: "preferred", scale: "auto", focus: true });

startup({ exec: ["waybar"] });

env("MOZ_ENABLE_WAYLAND", "1");

layout("columns", (ctx) => {
  const area = ctx.output.work_area;
  const count = Math.max(ctx.windows.length, 1);
  const width = Math.floor((area.w - (count - 1) * ctx.gap) / count);
  const result = {};
  ctx.windows.forEach((window, index) => {
    result[window.id] = {
      x: area.x + index * (width + ctx.gap),
      y: area.y,
      w: width,
      h: area.h,
    };
  });
  return result;
});

default_layout("columns");
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
- `print()` writes to the log; start Poly with `RUST_LOG=poly=info` to see it.
