# IPC protocol

Poly exposes a Unix-domain socket for status bars, scripts and the `poly msg`
command. The protocol is newline-delimited JSON: every request is one JSON value
on one line, and every request receives exactly one reply line. A client can
then subscribe to a stream of state changes.

This document describes the wire protocol. For configuration, see
[config.md](config.md).

## Socket discovery

The compositor creates one socket per session:

```text
$XDG_RUNTIME_DIR/poly.$WAYLAND_DISPLAY.$PID.sock
```

For example, `wayland-1` in `/run/user/1000` with PID 4242 produces:

```text
/run/user/1000/poly.wayland-1.4242.sock
```

Poly exports the exact path as `POLY_SOCKET` to every process it starts. A
client normally just reads that variable:

```sh
poly socket
printf '%s\n' "$POLY_SOCKET"
```

Every request, including event subscriptions, opens a new connection to this
path. The socket is a same-user control channel. Its access is protected by
`$XDG_RUNTIME_DIR` and the process umask; treat it like the Wayland socket and
do not expose it to other users.

## Framing and limits

- Requests and replies are UTF-8 JSON, one value per line.
- The newline (`\n`) is the only frame delimiter. There is no length prefix.
- One request produces one reply, in order.
- Empty lines are ignored.
- A request line larger than 256 KiB causes the compositor to disconnect the
  client.
- A client that stops reading is disconnected once 64 queued messages have
  accumulated. Rendering never blocks on a slow socket.
- Malformed JSON or an unknown variant produces an `{"Err":"..."}` reply. The
  connection stays open unless the request exceeded the size limit.
- Closing the socket ends the conversation. A subsequent client should
  reconnect and (for live state) request a new event stream.

Strings, objects and arrays follow the JSON types in the tables below. Serde's
externally tagged enum representation is used throughout: unit variants are
plain strings, and data-carrying variants are single-key objects.

## Request and reply envelope

A reply is a JSON `Result`:

```json
{"Ok":"Handled"}
{"Ok":{"Version":"0.1.0"}}
{"Err":"malformed request: unknown variant `frobnicate`"}
```

The successful payload is the `Response` variant. The error payload is a
human-readable string; clients should treat it as diagnostic text, not parse
it.

### Requests

| Request | Wire form | Successful reply payload |
| --- | --- | --- |
| Version | `"Version"` | `{"Version":"0.1.0"}` |
| Outputs | `"Outputs"` | `{"Outputs":{...}}` |
| Workspaces | `"Workspaces"` | `{"Workspaces":[...]}` |
| Windows | `"Windows"` | `{"Windows":[...]}` |
| FocusedWindow | `"FocusedWindow"` | `{"FocusedWindow":{...}}` or `{"FocusedWindow":null}` |
| FocusedOutput | `"FocusedOutput"` | `{"FocusedOutput":"eDP-1"}` or `{"FocusedOutput":null}` |
| XwaylandStatus | `"XwaylandStatus"` | `{"XwaylandStatus":{...}}` |
| Action | `{"Action":<action>}` | `"Handled"` |
| ReloadConfig | `"ReloadConfig"` | `"Handled"` |
| OutputConfig | `{"OutputConfig":{"output":"eDP-1","action":<output action>}}` | `{"OutputConfigChanged":"Applied"}` |
| LayoutSwitch | `{"LayoutSwitch":{"output":"eDP-1","name":"dwindle"}}` | `"Handled"` |
| EventStream | `"EventStream"` | `"Handled"`, then events |

Each table row shows the payload inside the `Ok` wrapper. For example, a
`Version` request is answered with:

```json
{"Ok":{"Version":"0.1.0"}}
```

and a malformed request with:

```json
{"Err":"malformed request: ..."}
```

`ReloadConfig` returns `"Handled"` even when the new file has errors. A failed
load leaves the previous configuration active (or the built-in defaults at
startup); observe `ConfigLoaded.failed` on an event stream, check the log, or
look at the on-screen Toast. `Action` also returns `"Handled"` without reporting
whether the action changed anything; actions that need a window do nothing when
no window is focused.

`OutputConfig` and `LayoutSwitch` can fail in ways the reply describes:

- `OutputConfigChanged` is `"Applied"`, `"OutputWasMissing"` or `"Rejected"`.
  A rejected change (an unsupported mode, for example) is not applied and the
  output keeps its previous value. All runtime output changes are temporary.
- `LayoutSwitch` returns `Err("no output named ...")` or
  `Err("no output has focus")` when it cannot resolve the target, and
  `Err("no layout named ... is configured")` for an unknown layout. A `null`
  output means "the focused output".

Runtime output changes are not written back to `config.lua`; `reload-config`
restores the file's values. Raw examples:

```text
{"OutputConfig":{"output":"eDP-1","action":{"Scale":1.5}}}
{"Ok":{"OutputConfigChanged":"Applied"}}

{"LayoutSwitch":{"output":null,"name":"columns"}}
{"Ok":"Handled"}
```

## Code examples

Using `socat`:

```sh
printf '"Version"\n' | socat - UNIX-CONNECT:"$POLY_SOCKET"
# {"Ok":{"Version":"0.1.0"}}

printf '"Windows"\n' | socat - UNIX-CONNECT:"$POLY_SOCKET"
# {"Ok":{"Windows":[]}}

printf '{"Action":{"Spawn":{"command":["foot"]}}}\n' \
    | socat - UNIX-CONNECT:"$POLY_SOCKET"
# {"Ok":"Handled"}
```

A minimal client:

```python
import os
import socket

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(os.environ["POLY_SOCKET"])
sock.sendall(b'"FocusedOutput"\n')
print(sock.makefile("r").readline().strip())
sock.close()
```

Requesting the event stream and reading every line:

```python
import os
import socket

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(os.environ["POLY_SOCKET"])
sock.sendall(b'"EventStream"\n')
for line in sock.makefile("r"):
    print(line.strip())
```

The first line is the `{"Ok":"Handled"}` reply. Every following line is one
event.

## Data types

### `Output`

Serialized representation of one connected output. Coordinate pairs are JSON
arrays, not objects.

```json
{
  "name": "eDP-1",
  "make": "BOE",
  "model": "0x095f",
  "serial": null,
  "physical_size": [344, 194],
  "position": [0, 0],
  "logical_size": [1920, 1080],
  "scale": 1.0,
  "current_mode": {"width": 1920, "height": 1080, "refresh": 60000},
  "modes": [
    {"width": 1920, "height": 1080, "refresh": 60000},
    {"width": 1920, "height": 1080, "refresh": 48000}
  ]
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `name` | string | Output name (`eDP-1`, `headless`, `winit`, ...). |
| `make`, `model` | string | Vendor and model reported by the backend. |
| `serial` | string or null | Serial number, when the backend reports one. |
| `physical_size` | `[width, height]` or null | Physical size in millimetres. |
| `position` | `[x, y]` | Position in compositor coordinates. |
| `logical_size` | `[width, height]` | Output geometry in logical pixels, as a fullscreen window would see it. |
| `scale` | number | Fractional scale. |
| `current_mode` | `Mode` or null | Mode currently driving the output. |
| `modes` | array of `Mode` | Supported modes, preferred first. |

`Mode` fields are `width` and `height` in pixels and `refresh` in millihertz
(`60000` means 60 Hz). Configuration files use Hz; the IPC protocol uses
millihertz.

### `Window`

```json
{
  "id": 3,
  "title": "Terminal",
  "app_id": "foot",
  "is_focused": true,
  "is_floating": false,
  "fullscreen": null,
  "output": "eDP-1",
  "workspace": 1
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | integer | Stable id for the lifetime of the compositor process. Do not persist it across restarts. |
| `title` | string or null | Client title. |
| `app_id` | string or null | Client app id. |
| `is_focused` | bool | Whether the window holds keyboard focus. |
| `is_floating` | bool | Whether the window floats above the layout. |
| `fullscreen` | `"Fullscreen"`, `"Windowed"` or null | Fullscreen mode. `"Windowed"` means the client believes it is fullscreen while the layout still places it. |
| `output` | string or null | Output the window is on. |
| `workspace` | integer or null | 1-based workspace index in that output's stack. |

### `Workspace`

```json
{
  "index": 1,
  "output": "eDP-1",
  "is_active": true,
  "is_focused": true,
  "active_window_id": 3,
  "layout": "columns"
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `index` | integer | 1-based position in the output's vertical workspace stack. |
| `output` | string or null | Owning output. |
| `is_active` | bool | Whether this workspace owns its output's screen. |
| `is_focused` | bool | Whether it is the active workspace of the focused output. |
| `active_window_id` | integer or null | Window the workspace would focus when entered. |
| `layout` | string | Layout arranging this workspace. |

Workspaces have no stable ids. Identify one by output name and 1-based index.
Moving workspaces changes indices.

### `XwaylandStatus`

```json
{
  "enabled": true,
  "configured": true,
  "display": ":2",
  "running": false,
  "on_demand": true,
  "satellite_path": "xwayland-satellite"
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `enabled` | bool | X11 sockets exist and children receive `DISPLAY`. False also means setup failed despite being configured. |
| `configured` | bool | Whether `xwayland.enable` was true. |
| `display` | string or null | `DISPLAY` value for child processes, when set up. |
| `running` | bool | Whether an `xwayland-satellite` process is running now. |
| `on_demand` | bool | Effective activation mode; a failing keep-running satellite falls back to on-demand. |
| `satellite_path` | string | Program that is started. |

### `OutputConfigChanged`

One of the strings `"Applied"`, `"OutputWasMissing"` or `"Rejected"`.

## Actions

The action vocabulary is shared with `config.lua` and `poly msg action`. JSON
uses the Rust variant names (PascalCase); Lua and the command line use kebab-case.
For example, `"CloseWindow"` on the wire is `close-window` in configuration and
`poly msg action close-window` on the command line, and the Lua form
`{ spawn = { "foot" } }` becomes:

```json
{"Spawn":{"command":["foot"]}}
```

Actions without fields are JSON strings:

```text
"Quit"
"CloseWindow"
"ReloadConfig"
"FocusNext"
"FocusPrev"
"FocusLeft"
"FocusRight"
"FocusUp"
"FocusDown"
"MoveWindow"
"ResizeWindow"
"MoveWindowLeft"
"MoveWindowRight"
"MoveWindowUp"
"MoveWindowDown"
"ToggleFloating"
"ToggleFullscreen"
"ToggleWindowedFullscreen"
"CycleLayout"
"FocusWorkspaceUp"
"FocusWorkspaceDown"
"MoveWindowToWorkspaceUp"
"MoveWindowToWorkspaceDown"
"MoveWorkspaceUp"
"MoveWorkspaceDown"
"ToggleOverview"
```

Actions with fields:

| JSON form | Meaning |
| --- | --- |
| `{"Spawn":{"command":["foot","-e","top"]}}` | Run an argv directly. The array must not be empty. |
| `{"SpawnShell":{"command":"foot -e top"}}` | Run a shell command line. |
| `{"SetLayout":{"name":"dwindle"}}` | Switch the active workspace to a named layout. |
| `{"FocusWorkspace":{"index":2}}` | Focus workspace 2 (1-based), creating it if needed. |
| `{"FocusWindow":{"id":3}}` | Focus a window by id. Ids are valid only while this compositor process runs. |
| `{"MoveWindowToWorkspace":{"index":2}}` | Move the focused window to workspace 2 and follow it. |
| `{"LayoutAction":{"name":"focus-left","args":[]}}` | Invoke an action declared by the active workspace's Lua layout. |

`LayoutAction` is the generic entry point for layout-defined actions. The name
and arguments are passed to the active layout's `actions` table; an unknown name
is reported in the log and does nothing. `args` is optional and defaults to an
empty list. Layout-scoped keybindings use the same action internally, but the
configuration loader already checks their names against the layout.

A complete action request wraps the action in the `Action` variant:

```json
{"Action":"ToggleOverview"}
{"Action":{"FocusWorkspace":{"index":2}}}
```

### Output actions

`OutputConfig.action` uses the same externally tagged representation:

```json
{"Scale":1.5}
{"Mode":{"width":1920,"height":1080,"refresh":60000}}
{"Position":{"x":1920,"y":0}}
```

- `Scale` takes a fractional scale.
- `Mode.refresh` is in millihertz and may be `null` to choose the preferred mode
  of that size.
- `Position` is in logical pixels.

## Events

Send `"EventStream"` once on a fresh connection. The reply is always
`{"Ok":"Handled"}`; the connection then becomes a one-way event stream. Further
request lines on it are ignored, so use a second connection for queries and
actions.

The stream begins with a full snapshot in this exact order:

1. `WorkspacesChanged`
2. `WindowsChanged`
3. `OutputsChanged`
4. `OverviewOpenedOrClosed`
5. `ConfigLoaded`

It then sends incremental events as the compositor state changes. The sequence
looks like this (payloads abbreviated):

```json
{"Ok":"Handled"}
{"WorkspacesChanged":{"workspaces":[...]}}
{"WindowsChanged":{"windows":[...]}}
{"OutputsChanged":{"outputs":[...]}}
{"OverviewOpenedOrClosed":{"is_open":false}}
{"ConfigLoaded":{"failed":false}}
{"WindowOpenedOrChanged":{"window":{...}}}
{"WindowFocusChanged":{"id":3}}
```

The five snapshot events are always sent, including when their arrays are empty
or the overview is closed. Reconnecting is the intended way to resynchronize
after a client has missed events.

### Event variants

| Event | Payload | Meaning |
| --- | --- | --- |
| `WorkspacesChanged` | `{"workspaces":[Workspace,...]}` | Full workspace state. Sent at stream start, when the stack shape changes, or when active/focused workspaces change. |
| `WorkspaceActiveWindowChanged` | `{"output":"eDP-1","index":1,"active_window_id":3}` | The window a workspace would focus changed. |
| `WindowsChanged` | `{"windows":[Window,...]}` | Full window state. Sent as part of the opening snapshot; later changes use the per-window events below. |
| `WindowOpenedOrChanged` | `{"window":Window}` | A window mapped or a non-focus property changed (title, app id, output, workspace, floating/fullscreen flags, ...). |
| `WindowClosed` | `{"id":3}` | A window disappeared. |
| `WindowFocusChanged` | `{"id":3}` or `{"id":null}` | Keyboard focus moved, or was cleared. |
| `FloatingChanged` | `{"id":3,"floating":true}` | A window started or stopped floating. |
| `FullscreenChanged` | `{"id":3,"fullscreen":"Fullscreen"}` or `{"id":3,"fullscreen":null}` | Fullscreen mode changed; `null` means windowed. |
| `LayoutChanged` | `{"output":"eDP-1","workspace_index":1,"name":"dwindle"}` | A workspace switched layouts. Both `output` and `workspace_index` may be `null` in the type; the compositor currently sends both when known. |
| `OutputsChanged` | `{"outputs":[Output,...]}` | Full output state. Sent as part of the opening snapshot; later changes use `OutputAdded`, `OutputRemoved` and `OutputChanged`. |
| `OutputAdded` | `{"output":Output}` | An output appeared. |
| `OutputRemoved` | `{"name":"eDP-1"}` | An output disappeared. |
| `OutputChanged` | `{"output":Output}` | The output's mode, scale, position or other reported properties changed. |
| `OverviewOpenedOrClosed` | `{"is_open":true}` | The workspace overview opened or closed. |
| `ConfigLoaded` | `{"failed":false}` | A configuration load finished. `true` means the file had errors: on reload the previous configuration stays active, and at startup the built-in defaults remain in use. |

`WindowOpenedOrChanged` excludes the focus flag. A focus change is a separate
`WindowFocusChanged`; a window that starts floating also produces
`FloatingChanged`, and a fullscreen change also produces `FullscreenChanged`.
Consumers that need a coherent view of one object should use the event for that
object rather than assuming multiple state slices are updated atomically.

The event stream starts from a baseline kept by the compositor. A new client
receives the current snapshot immediately, even if it connected long after
startup. Once the stream is connected, events are generated by diffing against
that baseline, so changes to the state exposed by these types are reported no
matter whether they came from IPC, keybindings, configuration reloads or
backend hotplug. Window geometry is not part of this state model, so a Lua
layout changing rectangles does not produce a geometry event.

A slow event client that cannot keep up is disconnected rather than allowed to
stall the compositor. Reconnect and request the stream again to get a fresh
snapshot. Query connections only queue replies to requests they sent, so a
normal query client is not affected by event-stream backpressure.

## `poly msg`

`poly msg` is a convenience front end over this protocol. Without `--json` it
prints human-readable text; that format is for people and may change. Pass
`--json` for the payload as machine-readable JSON:

```sh
poly msg version
poly msg outputs
poly msg workspaces
poly msg windows
poly msg focused-window
poly msg focused-output
poly msg xwayland

poly msg action close-window
poly msg action toggle-overview
poly msg action -- spawn foot -e top
poly msg action focus-workspace 2
poly msg action move-window-to-workspace 3
poly msg action layout-action focus-left
poly msg action layout-action resize-column 0.5

poly msg output eDP-1 scale 1.5
poly msg output eDP-1 mode 1920x1080@60
poly msg output eDP-1 position 1920 0
poly msg layout dwindle --output eDP-1

poly msg event-stream
poly msg --json event-stream
poly socket
```

`poly msg action` accepts kebab-case names and arguments, the same spelling
used in Lua. Put `--` before arguments that start with `-`, as in the `spawn`
example above; otherwise clap treats them as `poly msg` options. The
`poly msg output` form follows the CLI spelling:

```sh
poly msg output NAME scale 1.5
poly msg output NAME mode 1920x1080
poly msg output NAME mode 1920x1080@60
poly msg output NAME position X Y
```

`poly msg --json` prints only the successful payload, without the
`{"Ok":...}` envelope:

```sh
$ poly msg --json version
"0.1.0"

$ poly msg --json windows
[]

$ poly msg --json action toggle-overview
null
```

`poly msg --json event-stream` prints one raw event per line; it omits the
initial `Handled` reply. This is the recommended way to consume events from a
shell script:

```sh
poly msg --json event-stream | while IFS= read -r event; do
    printf '%s\n' "$event"
done
```

`poly socket` prints `$POLY_SOCKET`. It must be run in an environment that
inherits the variable, such as a terminal started by Poly. From an unrelated
shell, set `POLY_SOCKET` to the path shown by the path convention above.

## Compatibility

The protocol is versioned only by the compositor version (`Version`) and by
additive evolution:

- New fields are optional or have defaults.
- New request, response and event variants are appended; existing variants are
  not renamed or reordered.
- Clients should ignore fields and variants they do not understand, and should
  not depend on JSON object key order.
- Unknown request variants produce an error, so a client should send only
  requests it knows.
- The human-readable output of `poly msg` is not a stable interface; use
  `--json` for scripts.

For related configuration settings, see [config.md](config.md).
