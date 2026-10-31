//! The Lua configuration loader.
//!
//! The root file is executed with a `poly` table in scope and should return a
//! table (or call `poly.setup{…}`):
//!
//! ```lua
//! return {
//!     keybinds = { ["Mod+Return"] = { spawn = { "foot" } } },
//! }
//! ```
//!
//! `poly.include("theme.lua")` evaluates another file — resolved relative to
//! the file that called it — and returns its value. Included files are tracked
//! so the reload watcher (M6) knows what to watch. All problems are collected
//! with field paths, and a runaway script is stopped by an instruction budget.
//!
//! Lua state lives on the thread that loads the configuration and is never
//! sent anywhere (`AGENTS.md` §2.5).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, Table, Value, VmState};
use poly_ipc::Action;

use crate::binds::{Key, Keybind};
use crate::config::{
    AccelProfile, AnimationCurve, AnimationOverride, Animations, Blur, Border, ClickMethod, Color,
    Config, Device, Fullscreen, General, Input, Keyboard, LayerInteractivity, LayerRule, ModKey,
    Overview, ResizePick, ScrollMethod, Shadow, StartupSpawn, TiledDrag, WindowDrag, Xwayland,
};
use crate::error::{ConfigError, ConfigMessage, Errors, path_index, path_key};
use crate::layout::{LayoutPrograms, parse_outputs, parse_window_rules};
use crate::locate::ConfigSource;
use crate::lua_ext as lx;

/// Maximum nesting of `poly.include` (`docs/plan.md` §11).
const MAX_INCLUDE_DEPTH: usize = 32;

/// VM instructions one configuration may execute before it is aborted.
const INSTRUCTION_BUDGET: u64 = 20_000_000;

/// How often the instruction hook runs.
const INSTRUCTION_STEP: u32 = 500_000;

/// Runs a chunk with a traceback. Both the root file and everything pulled in
/// with `poly.include` go through this, so `chunk` names are real file paths
/// and tracebacks point at the offending line.
const RUNNER: &str = r#"
function poly_run(source, name)
  local chunk, err = load(source, name)
  if not chunk then return false, err end
  local function handler(err)
    -- Errors raised by the Rust helpers arrive as error objects, and
    -- `debug.traceback` only prints a message when it is a string.
    if type(err) ~= "string" then err = tostring(err) end
    return debug.traceback(err)
  end
  local ok, result = xpcall(chunk, handler)
  if not ok then return false, result end
  return true, result
end
"#;

/// The Lua half of `poly.include`: raises a plain error string, so a missing
/// or cyclic include is reported like any other Lua error, with the position
/// of the call (`error(value, 2)` blames the caller).
const INCLUDE_WRAPPER: &str = r#"
function poly.include(path)
  local ok, value = poly._include(path)
  if not ok then error(value, 2) end
  return value
end
"#;

/// The outcome of a configuration load.
#[derive(Debug)]
pub struct ConfigLoadResult {
    /// The parsed configuration, or every problem found while loading it.
    pub config: Result<Config, ConfigError>,
    /// Every file the load touched, for the reload watcher (M6).
    pub includes: Vec<PathBuf>,
    /// Non-fatal problems: unknown settings, a missing configuration file.
    pub warnings: Vec<ConfigMessage>,
    /// Lua layouts kept alive for the compositor. `None` means the built-in
    /// fallback layout is the only one.
    pub layout_programs: Option<LayoutPrograms>,
    /// The file's `errors.dbus`, even when the file did not load.
    ///
    /// A configuration with a typo is exactly when a desktop notification
    /// helps, so this is read best-effort from the parsed root table before the
    /// configuration is thrown away.
    pub error_notifications: bool,
}

/// Shared state between the Rust helpers exposed to Lua.
struct State {
    /// Files that were executed, in execution order (may repeat).
    includes: RefCell<Vec<PathBuf>>,
    /// Include chain currently executing, for cycle and depth checks.
    stack: RefCell<Vec<PathBuf>>,
    /// Table passed to `poly.setup{…}`, if the config used that form.
    setup: RefCell<Option<Table>>,
    /// Instructions left before the load is aborted.
    budget: Cell<u64>,
}

impl State {
    fn new() -> Self {
        return Self {
            includes: RefCell::new(Vec::new()),
            stack: RefCell::new(Vec::new()),
            setup: RefCell::new(None),
            budget: Cell::new(INSTRUCTION_BUDGET),
        };
    }

    /// Every file that was executed, deduplicated, in execution order.
    fn includes(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        for path in self.includes.borrow().iter() {
            if seen.insert(path.clone()) {
                files.push(path.clone());
            }
        }
        return files;
    }
}

/// Load the configuration described by `source`.
///
/// A missing optional file is not an error: the built-in defaults apply and a
/// warning is reported.
pub fn load(source: &ConfigSource) -> ConfigLoadResult {
    match source {
        ConfigSource::Cli(path)
        | ConfigSource::Env(path)
        | ConfigSource::User(path)
        | ConfigSource::System(path) => return load_file(path),
        ConfigSource::Default(None) => {
            return ConfigLoadResult {
                config: Ok(Config::default()),
                includes: Vec::new(),
                warnings: vec![ConfigMessage::file(
                    "no configuration found, using the built-in defaults",
                )],
                layout_programs: None,
                error_notifications: false,
            };
        }
        ConfigSource::Default(Some(path)) => {
            if !path.exists() {
                return ConfigLoadResult {
                    config: Ok(Config::default()),
                    includes: Vec::new(),
                    warnings: vec![ConfigMessage::file(format!(
                        "no configuration at {}, using the built-in defaults",
                        path.display()
                    ))],
                    layout_programs: None,
                    error_notifications: false,
                };
            }
            return load_file(path);
        }
    }
}

/// Load one configuration file, following its includes.
pub fn load_file(path: &Path) -> ConfigLoadResult {
    let mut errors = Errors::default();
    let state = Rc::new(State::new());

    let loaded = load_inner(path, &state, &mut errors);

    // Read `errors.dbus` before the configuration is thrown away: a file that
    // fails to load is the case where a desktop notification is most wanted.
    let error_notifications = loaded
        .as_ref()
        .is_ok_and(|loaded| return loaded.config.errors.dbus);

    let (messages, warnings) = errors.into_parts();
    let (config, layout_programs) = match loaded {
        Ok(loaded) if messages.is_empty() => (Ok(loaded.config), loaded.layouts),
        _ => (
            Err(ConfigError {
                source: path.to_owned(),
                messages,
            }),
            None,
        ),
    };
    return ConfigLoadResult {
        config,
        includes: state.includes(),
        warnings,
        layout_programs,
        error_notifications,
    };
}

/// What a successful load produces: the parsed configuration and the Lua state
/// behind its layouts.
struct Loaded {
    config: Config,
    layouts: Option<LayoutPrograms>,
}

/// Load and parse, pushing problems into `errors`; `Err(())` means the load
/// could not get far enough to produce a configuration at all.
fn load_inner(path: &Path, state: &Rc<State>, errors: &mut Errors) -> Result<Loaded, ()> {
    let canonical = std::fs::canonicalize(path).map_err(|err| {
        errors.file(format!("cannot read {}: {err}", path.display()));
    })?;

    let lua = setup_lua(state).map_err(|err| {
        errors.file(format!("cannot set up the Lua environment: {err}"));
    })?;

    let value = run_root(&lua, state, &canonical).map_err(|err| {
        errors.file(lua_error_text(&err));
    })?;

    let Some(table) = value_table(value, state, errors) else {
        return Err(());
    };
    let (config, layouts) = parse_config(&lua, &table, &canonical, errors);
    return Ok(Loaded { config, layouts });
}

/// Create the Lua state: the `poly` helpers, the chunk runner and the
/// instruction budget.
fn setup_lua(state: &Rc<State>) -> mlua::Result<Lua> {
    // The `debug` library is needed for `debug.traceback` in the runner so that
    // configuration errors point at the offending file and line.
    //
    // SAFETY: the configuration runs on the thread that owns this Lua state and
    // is written by the user who started the compositor, so the debug library
    // cannot break any sandbox we rely on (`AGENTS.md` §2.5).
    let lua =
        unsafe { Lua::unsafe_new_with(StdLib::ALL_SAFE | StdLib::DEBUG, LuaOptions::default()) };
    lua.load(RUNNER).set_name("@poly/runner.lua").exec()?;

    let poly = lua.create_table()?;

    {
        let state = state.clone();
        // The Rust half returns `(ok, value)`; the Lua wrapper below turns a
        // failure into a plain Lua error. An `mlua::Error` raised straight from
        // Rust reaches Lua as an opaque object, and then no traceback handler
        // can print a useful message for it.
        poly.set(
            "_include",
            lua.create_function(
                move |lua, requested: String| -> mlua::Result<(bool, Value)> {
                    return match include_file(lua, &state, &requested) {
                        Ok(value) => Ok((true, value)),
                        Err(err) => {
                            let message = lua.create_string(lua_error_text(&err))?;
                            return Ok((false, Value::String(message)));
                        }
                    };
                },
            )?,
        )?;
    }
    {
        let state = state.clone();
        poly.set(
            "read_file",
            lua.create_function(
                move |lua, requested: String| -> mlua::Result<(Value, Value)> {
                    return read_file(lua, &state, &requested);
                },
            )?,
        )?;
    }

    // Action constructors: `poly.spawn{…}` returns the same table as writing
    // `{ spawn = { … } }` inline, validation happens in one place.
    poly.set(
        "spawn",
        lua.create_function(|lua, command: Value| -> mlua::Result<Value> {
            let table = lua.create_table()?;
            table.set("spawn", command)?;
            return Ok(Value::Table(table));
        })?,
    )?;
    poly.set(
        "spawn_shell",
        lua.create_function(|lua, command: Value| -> mlua::Result<Value> {
            let table = lua.create_table()?;
            table.set("spawn_shell", command)?;
            return Ok(Value::Table(table));
        })?,
    )?;

    {
        let state = state.clone();
        poly.set(
            "setup",
            lua.create_function(move |_lua, config: Table| -> mlua::Result<()> {
                *state.setup.borrow_mut() = Some(config);
                return Ok(());
            })?,
        )?;
    }

    lua.globals().set("poly", poly)?;

    // Needs the `poly` global to be in place, hence after the table is built.
    lua.load(INCLUDE_WRAPPER)
        .set_name("@poly/include.lua")
        .exec()?;

    // A runaway configuration must not hang the compositor.
    let hook_state = state.clone();
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(INSTRUCTION_STEP),
        move |_lua, _debug| -> mlua::Result<VmState> {
            let left = hook_state.budget.get();
            if left == 0 {
                return Err(mlua::Error::runtime(
                    "configuration ran too long (infinite loop?)",
                ));
            }
            hook_state
                .budget
                .set(left.saturating_sub(u64::from(INSTRUCTION_STEP)));
            return Ok(VmState::Continue);
        },
    )?;

    return Ok(lua);
}

/// Execute the root file, tracking it on the include stack.
fn run_root(lua: &Lua, state: &Rc<State>, canonical: &Path) -> mlua::Result<Value> {
    state.stack.borrow_mut().push(canonical.to_owned());
    let result = run_file(lua, state, canonical);
    state.stack.borrow_mut().pop();
    return result;
}

/// Read and execute one file through the tracebacking runner.
fn run_file(lua: &Lua, state: &Rc<State>, path: &Path) -> mlua::Result<Value> {
    let source = std::fs::read_to_string(path)
        .map_err(|err| mlua::Error::runtime(format!("cannot read {}: {err}", path.display())))?;
    state.includes.borrow_mut().push(path.to_owned());

    let run: mlua::Function = lua.globals().get("poly_run")?;
    let (ok, value) = run.call::<(bool, Value)>((source, format!("@{}", path.display())))?;
    if !ok {
        // The runner hands back the `debug.traceback` text; a Rust callback
        // error travels as an `Error` value, everything else as a string.
        let message = match &value {
            Value::String(s) => match s.to_str() {
                Ok(s) => s.to_owned(),
                Err(_) => "configuration error (non-UTF-8 message)".to_owned(),
            },
            Value::Error(err) => lua_error_text(err),
            other => format!("configuration error ({} value)", lx::type_name(other)),
        };
        return Err(mlua::Error::runtime(strip_loader_frames(&message)));
    }
    return Ok(value);
}

/// Drop the traceback frames that point at the loader itself: the user debugged
/// their configuration, and `poly/runner.lua` is not part of it.
fn strip_loader_frames(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut lines = text.lines();
    // The first line is the message and always stays.
    if let Some(first) = lines.next() {
        kept.push(first);
    }
    for line in lines {
        let frame = line.trim();
        let is_loader = frame.contains("poly/runner.lua")
            || frame.contains("poly/include.lua")
            || frame == "[C]: in function 'xpcall'"
            || frame == "[C]: in function 'error'"
            || frame == "[C]: in field 'include'"
            || frame == "[C]: in field '_include'"
            || frame == "(...tail calls...)";
        if !is_loader {
            kept.push(line);
        }
    }
    // A traceback heading whose frames were all ours has nothing left to show.
    while kept.len() > 1 && kept.last().map(|line| return line.trim()) == Some("stack traceback:") {
        kept.pop();
    }
    return kept.join("\n");
}

/// `poly.include(path)`: run another file and return its value.
fn include_file(lua: &Lua, state: &Rc<State>, requested: &str) -> mlua::Result<Value> {
    let from = caller_file(lua).or_else(|| return state.stack.borrow().last().cloned());
    let resolved = resolve_path(from.as_deref(), requested);
    let canonical = std::fs::canonicalize(&resolved).map_err(|err| {
        return mlua::Error::runtime(format!(
            "cannot read included file {}: {err}",
            resolved.display()
        ));
    })?;

    {
        let stack = state.stack.borrow();
        if stack.len() >= MAX_INCLUDE_DEPTH {
            return Err(mlua::Error::runtime(format!(
                "too many nested includes (more than {MAX_INCLUDE_DEPTH}): {}",
                chain_text(&stack, &canonical)
            )));
        }
        if stack.contains(&canonical) {
            return Err(mlua::Error::runtime(format!(
                "include cycle: {}",
                chain_text(&stack, &canonical)
            )));
        }
    }

    state.stack.borrow_mut().push(canonical.clone());
    let result = run_file(lua, state, &canonical);
    state.stack.borrow_mut().pop();
    return result;
}

/// `poly.read_file(path)`: `contents` or `nil, err`. Relative paths resolve
/// like `poly.include`, but data files are not tracked as dependencies.
fn read_file(lua: &Lua, state: &Rc<State>, requested: &str) -> mlua::Result<(Value, Value)> {
    let from = caller_file(lua).or_else(|| return state.stack.borrow().last().cloned());
    let resolved = resolve_path(from.as_deref(), requested);
    return match std::fs::read_to_string(&resolved) {
        Ok(contents) => Ok((Value::String(lua.create_string(&contents)?), Value::Nil)),
        Err(err) => Ok((
            Value::Nil,
            Value::String(lua.create_string(format!("cannot read {}: {err}", resolved.display()))?),
        )),
    };
}

/// The file of the Lua chunk that called into `poly`. Level 1 is the caller of
/// the Rust function; chunk names are `@<real path>` (`run_file`).
/// The file the currently running code came from, skipping the helper chunks
/// we run ourselves (`@poly/…`), which can sit between the configuration and
/// the call and would otherwise capture relative paths.
fn caller_file(lua: &Lua) -> Option<PathBuf> {
    // A Rust helper called from Lua: level 1 is its caller, further levels are
    // its callers. A handful is plenty; the loop stops when the stack ends.
    for level in 1..=8 {
        let file = lua.inspect_stack(level, |debug| {
            let source = debug.source().source?;
            let path = source.strip_prefix('@')?;
            if path.starts_with("poly/") {
                return None;
            }
            return Some(PathBuf::from(path));
        });
        match file {
            None => break,
            Some(None) => continue,
            Some(Some(path)) => return Some(path),
        }
    }
    return None;
}

/// Resolve a path the way the whole configuration does (`docs/plan.md` §11):
/// `~` expands to the home directory, relative paths are relative to the file
/// that wrote them.
pub(crate) fn resolve_path(from_file: Option<&Path>, requested: &str) -> PathBuf {
    let requested = expand_tilde(requested);
    if requested.is_absolute() {
        return requested;
    }
    let base = from_file
        .and_then(|path| return path.parent())
        .unwrap_or(Path::new("."));
    return base.join(requested);
}

fn expand_tilde(requested: &str) -> PathBuf {
    if requested == "~" {
        return home_dir().unwrap_or_else(|| return PathBuf::from(requested));
    }
    if let Some(rest) = requested.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    return PathBuf::from(requested);
}

fn home_dir() -> Option<PathBuf> {
    return std::env::var_os("HOME")
        .filter(|home| return !home.is_empty())
        .map(PathBuf::from);
}

/// `a.lua -> b.lua -> c.lua`, for include errors.
fn chain_text(stack: &[PathBuf], next: &Path) -> String {
    let mut text = String::new();
    for path in stack {
        text.push_str(&path.display().to_string());
        text.push_str(" -> ");
    }
    text.push_str(&next.display().to_string());
    return text;
}

/// The returned value, or the table from `poly.setup{…}`.
fn value_table(value: Value, state: &State, errors: &mut Errors) -> Option<Table> {
    return match value {
        Value::Table(table) => Some(table),
        Value::Nil => {
            if let Some(table) = state.setup.borrow().clone() {
                return Some(table);
            }
            errors.file(
                "the configuration returned nothing: use `return { … }` or `poly.setup{ … }`",
            );
            None
        }
        other => {
            errors.file(format!(
                "the configuration must return a table, got {}",
                lx::type_name(&other)
            ));
            None
        }
    };
}

/// Full error text: mlua splits the traceback off callback errors.
fn lua_error_text(err: &mlua::Error) -> String {
    return match err {
        mlua::Error::CallbackError { traceback, cause } => {
            let cause = lua_error_text(cause);
            // A Lua-level traceback (from our runner) points at the real file
            // and line; mlua's Rust-level one adds nothing then.
            if cause.contains("stack traceback") {
                return cause;
            }
            return format!("{cause}\n{traceback}");
        }
        // `Display` would prepend "runtime error: ", which reads badly when the
        // message is nested inside another error.
        mlua::Error::RuntimeError(message) => return message.clone(),
        err => err.to_string(),
    };
}

// ---------------------------------------------------------------------------
// Parsing the returned table into `Config`.
// ---------------------------------------------------------------------------

const ROOT_KEYS: &[&str] = &[
    "general",
    "input",
    "keybinds",
    "window_drag",
    "spawn_at_startup",
    "environment",
    "layer_rules",
    "layouts",
    "default_layout",
    "window_rules",
    "outputs",
    "fullscreen",
    "overview",
    "errors",
    "xwayland",
];

fn parse_config(
    lua: &Lua,
    table: &Table,
    root: &Path,
    errors: &mut Errors,
) -> (Config, Option<LayoutPrograms>) {
    let mut config = Config::default();
    lx::warn_unknown(table, "", ROOT_KEYS, errors);

    if let Some(t) = lx::table(table, "general", "", errors) {
        config.general = parse_general(&t, root, errors);
    }
    if let Some(t) = lx::table(table, "input", "", errors) {
        config.input = parse_input(&t, errors);
    }
    if let Some(t) = lx::table(table, "keybinds", "", errors) {
        config.keybinds = parse_keybinds(&t, errors);
    }
    if let Some(t) = lx::table(table, "window_drag", "", errors) {
        config.window_drag = parse_window_drag(&t, errors);
    }
    if let Some(t) = lx::table(table, "spawn_at_startup", "", errors) {
        config.spawn_at_startup = parse_spawn_at_startup(&t, errors);
    }
    if let Some(t) = lx::table(table, "environment", "", errors) {
        config.environment = parse_environment(&t, errors);
    }
    if let Some(t) = lx::table(table, "layer_rules", "", errors) {
        config.layer_rules = parse_layer_rules(&t, errors);
    }
    if let Some(t) = lx::table(table, "window_rules", "", errors) {
        config.window_rules = parse_window_rules(&t, root, errors);
    }
    if let Some(t) = lx::table(table, "outputs", "", errors) {
        config.outputs = parse_outputs(&t, errors);
    }
    if let Some(t) = lx::table(table, "fullscreen", "", errors) {
        config.fullscreen = parse_fullscreen(&t, errors);
    }
    if let Some(t) = lx::table(table, "overview", "", errors) {
        config.overview = parse_overview(&t, errors);
    }
    if let Some(t) = lx::table(table, "errors", "", errors) {
        config.errors = parse_errors(&t, errors);
    }
    if let Some(t) = lx::table(table, "xwayland", "", errors) {
        config.xwayland = parse_xwayland(&t, errors);
    }
    let layouts = LayoutPrograms::parse(lua, table, &mut config, errors);
    return (config, layouts);
}

fn parse_fullscreen(table: &Table, errors: &mut Errors) -> Fullscreen {
    let mut fullscreen = Fullscreen::default();
    lx::warn_unknown(table, "fullscreen", &["policy", "cover_top_layer"], errors);
    if let Some(value) = lx::string(table, "policy", "fullscreen", errors) {
        if value != "auto" {
            errors.warn(
                path_key("fullscreen", "policy"),
                format!("unsupported fullscreen policy {value:?}, using auto"),
            );
        }
    }
    if let Some(value) = lx::boolean(table, "cover_top_layer", "fullscreen", errors) {
        fullscreen.cover_top_layer = value;
    }
    return fullscreen;
}

fn parse_general(table: &Table, root: &Path, errors: &mut Errors) -> General {
    let mut general = General::default();
    lx::warn_unknown(
        table,
        "general",
        &[
            "mod_key",
            "prefer_no_csd",
            "gap",
            "animations",
            "border",
            "shadow",
            "blur",
            "opacity",
            "window_shader",
        ],
        errors,
    );

    if let Some(value) = lx::string(table, "mod_key", "general", errors) {
        match parse_mod_key(&value) {
            Some(mod_key) => general.mod_key = Some(mod_key),
            None => errors.error(
                path_key("general", "mod_key"),
                format!(
                    "invalid mod key: {value} (supported: ctrl, shift, alt, super, \
                     iso-level3-shift, iso-level5-shift)"
                ),
            ),
        }
    }
    if let Some(value) = lx::boolean(table, "prefer_no_csd", "general", errors) {
        general.prefer_no_csd = value;
    }
    if let Some(value) = lx::integer(table, "gap", "general", 0, 512, errors) {
        general.gap = value as i32;
    }
    if let Some(t) = lx::table(table, "animations", "general", errors) {
        general.animations = parse_animations(&t, errors);
    }
    if let Some(t) = lx::table(table, "border", "general", errors) {
        general.border = parse_border(&t, errors);
    }
    if let Some(t) = lx::table(table, "shadow", "general", errors) {
        general.shadow = parse_shadow(&t, "general.shadow", Shadow::default(), errors);
    }
    if let Some(t) = lx::table(table, "blur", "general", errors) {
        general.blur = parse_blur(&t, root, errors);
    }
    if let Some(value) = lx::number(table, "opacity", "general", errors) {
        if (0.0..=1.0).contains(&value) {
            general.opacity = value;
        } else {
            errors.error(
                path_key("general", "opacity"),
                "value must be between 0.0 and 1.0",
            );
        }
    }
    if let Some(value) = lx::string(table, "window_shader", "general", errors) {
        general.window_shader = Some(resolve_path(Some(root), &value).display().to_string());
    }
    return general;
}

/// Parse `errors = { dbus = false }` (`docs/plan.md` §14).
fn parse_errors(table: &Table, errors: &mut Errors) -> crate::config::Errors {
    let mut result = crate::config::Errors::default();
    lx::warn_unknown(table, "errors", &["dbus"], errors);
    if let Some(value) = lx::boolean(table, "dbus", "errors", errors) {
        result.dbus = value;
    }
    return result;
}

/// Parse `xwayland = { enable, satellite_path, on_demand, spawn_at_startup }`
/// (`docs/plan.md` §13).
fn parse_xwayland(table: &Table, errors: &mut Errors) -> Xwayland {
    let mut xwayland = Xwayland::default();
    lx::warn_unknown(
        table,
        "xwayland",
        &["enable", "satellite_path", "on_demand", "spawn_at_startup"],
        errors,
    );
    if let Some(value) = lx::boolean(table, "enable", "xwayland", errors) {
        xwayland.enable = value;
    }
    if let Some(value) = lx::string(table, "satellite_path", "xwayland", errors) {
        if value.is_empty() {
            errors.error(
                path_key("xwayland", "satellite_path"),
                "must name the xwayland-satellite program or a path to it",
            );
        } else {
            xwayland.satellite_path = value;
        }
    }
    if let Some(value) = lx::boolean(table, "on_demand", "xwayland", errors) {
        xwayland.on_demand = value;
    }
    if let Some(value) = lx::boolean(table, "spawn_at_startup", "xwayland", errors) {
        xwayland.spawn_at_startup = value;
    }
    return xwayland;
}

fn parse_overview(table: &Table, errors: &mut Errors) -> Overview {
    let mut overview = Overview::default();
    lx::warn_unknown(
        table,
        "overview",
        &["zoom", "backdrop_color", "card_color", "shadow"],
        errors,
    );
    if let Some(value) = lx::number(table, "zoom", "overview", errors) {
        if (0.0001..=0.75).contains(&value) {
            overview.zoom = value;
        } else {
            errors.error(
                path_key("overview", "zoom"),
                "value must be between 0.0001 and 0.75 (niri clamps it the same way)",
            );
        }
    }
    if let Some(value) = lx::string(table, "backdrop_color", "overview", errors) {
        match Color::parse(&value) {
            Some(color) => overview.backdrop_color = color,
            None => errors.error(
                path_key("overview", "backdrop_color"),
                format!("invalid color: {value} (expected #rgb, #rrggbb or #rrggbbaa)"),
            ),
        }
    }
    if let Some(value) = lx::string(table, "card_color", "overview", errors) {
        match Color::parse(&value) {
            Some(color) => overview.card_color = color,
            None => errors.error(
                path_key("overview", "card_color"),
                format!("invalid color: {value} (expected #rgb, #rrggbb or #rrggbbaa)"),
            ),
        }
    }
    if let Some(t) = lx::table(table, "shadow", "overview", errors) {
        overview.shadow = parse_shadow(&t, "overview.shadow", overview.shadow, errors);
    }
    return overview;
}

fn parse_border(table: &Table, errors: &mut Errors) -> Border {
    let mut border = Border::default();
    lx::warn_unknown(
        table,
        "general.border",
        &["width", "radius", "active", "inactive"],
        errors,
    );
    if let Some(value) = lx::number(table, "width", "general.border", errors) {
        if (0.0..=256.0).contains(&value) {
            border.width = value;
        } else {
            errors.error(
                path_key("general.border", "width"),
                "value must be between 0 and 256",
            );
        }
    }
    if let Some(value) = lx::number(table, "radius", "general.border", errors) {
        if (0.0..=1024.0).contains(&value) {
            border.radius = value;
        } else {
            errors.error(
                path_key("general.border", "radius"),
                "value must be between 0 and 1024",
            );
        }
    }
    if let Some(value) = lx::string(table, "active", "general.border", errors) {
        match Color::parse(&value) {
            Some(color) => border.active = color,
            None => errors.error(
                path_key("general.border", "active"),
                format!("invalid color: {value} (expected #rgb, #rrggbb or #rrggbbaa)"),
            ),
        }
    }
    if let Some(value) = lx::string(table, "inactive", "general.border", errors) {
        match Color::parse(&value) {
            Some(color) => border.inactive = color,
            None => errors.error(
                path_key("general.border", "inactive"),
                format!("invalid color: {value} (expected #rgb, #rrggbb or #rrggbbaa)"),
            ),
        }
    }
    return border;
}

fn parse_shadow(table: &Table, path: &str, mut shadow: Shadow, errors: &mut Errors) -> Shadow {
    lx::warn_unknown(
        table,
        path,
        &["enable", "blur", "spread", "offset", "color"],
        errors,
    );
    if let Some(value) = lx::boolean(table, "enable", path, errors) {
        shadow.enable = value;
    }
    if let Some(value) = lx::number(table, "blur", path, errors) {
        if (0.0..=512.0).contains(&value) {
            shadow.blur = value;
        } else {
            errors.error(path_key(path, "blur"), "value must be between 0 and 512");
        }
    }
    if let Some(value) = lx::number(table, "spread", path, errors) {
        if (-1024.0..=1024.0).contains(&value) {
            shadow.spread = value;
        } else {
            errors.error(
                path_key(path, "spread"),
                "value must be between -1024 and 1024",
            );
        }
    }
    if let Some(offset) = lx::number_list(table, "offset", path, errors) {
        if offset.len() == 2 {
            shadow.offset_x = offset[0].round() as i32;
            shadow.offset_y = offset[1].round() as i32;
        } else {
            errors.error(
                path_key(path, "offset"),
                format!("expected two numbers, got {}", offset.len()),
            );
        }
    }
    if let Some(value) = lx::string(table, "color", path, errors) {
        match Color::parse(&value) {
            Some(color) => shadow.color = color,
            None => errors.error(
                path_key(path, "color"),
                format!("invalid color: {value} (expected #rgb, #rrggbb or #rrggbbaa)"),
            ),
        }
    }
    return shadow;
}

fn parse_blur(table: &Table, root: &Path, errors: &mut Errors) -> Blur {
    let mut blur = Blur::default();
    lx::warn_unknown(
        table,
        "general.blur",
        &[
            "enable",
            "passes",
            "offset",
            "noise",
            "accept_client_regions",
            "layer_surfaces",
            "shader",
        ],
        errors,
    );
    if let Some(value) = lx::boolean(table, "enable", "general.blur", errors) {
        blur.enable = value;
    }
    if let Some(value) = lx::integer(table, "passes", "general.blur", 0, 8, errors) {
        blur.passes = value as u32;
    }
    if let Some(value) = lx::number(table, "offset", "general.blur", errors) {
        if (0.0..=64.0).contains(&value) {
            blur.offset = value;
        } else {
            errors.error(
                path_key("general.blur", "offset"),
                "value must be between 0 and 64",
            );
        }
    }
    if let Some(value) = lx::number(table, "noise", "general.blur", errors) {
        if (0.0..=1.0).contains(&value) {
            blur.noise = value;
        } else {
            errors.error(
                path_key("general.blur", "noise"),
                "value must be between 0.0 and 1.0",
            );
        }
    }
    if let Some(value) = lx::boolean(table, "accept_client_regions", "general.blur", errors) {
        blur.accept_client_regions = value;
    }
    if let Some(value) = lx::boolean(table, "layer_surfaces", "general.blur", errors) {
        blur.layer_surfaces = value;
    }
    if let Some(t) = lx::table(table, "shader", "general.blur", errors) {
        let path = "general.blur.shader";
        lx::warn_unknown(&t, path, &["down", "up", "finish"], errors);
        blur.shader.down = lx::string(&t, "down", path, errors)
            .map(|value| return resolve_path(Some(root), &value).display().to_string());
        blur.shader.up = lx::string(&t, "up", path, errors)
            .map(|value| return resolve_path(Some(root), &value).display().to_string());
        blur.shader.finish = lx::string(&t, "finish", path, errors)
            .map(|value| return resolve_path(Some(root), &value).display().to_string());
    }
    return blur;
}

fn parse_animations(table: &Table, errors: &mut Errors) -> Animations {
    return parse_animations_at(table, "general.animations", errors);
}

fn parse_animations_at(table: &Table, base: &str, errors: &mut Errors) -> Animations {
    let mut animations = Animations::default();
    lx::warn_unknown(
        table,
        base,
        &[
            "enable",
            "duration",
            "slowdown",
            "window_open",
            "window_close",
            "window_movement",
            "window_resize",
            "workspace_switch",
            "overview",
        ],
        errors,
    );
    if let Some(value) = lx::boolean(table, "enable", base, errors) {
        animations.enable = value;
    }
    if let Some(value) = lx::integer(table, "duration", base, 0, 10_000, errors) {
        animations.duration = value as u16;
    }
    if let Some(value) = lx::number(table, "slowdown", base, errors) {
        if (0.0..=100.0).contains(&value) {
            animations.slowdown = value;
        } else {
            errors.error(
                path_key(base, "slowdown"),
                "value must be between 0.0 and 100.0",
            );
        }
    }

    for (key, target) in [
        ("window_open", &mut animations.window_open),
        ("window_close", &mut animations.window_close),
        ("window_movement", &mut animations.window_movement),
        ("window_resize", &mut animations.window_resize),
        ("workspace_switch", &mut animations.workspace_switch),
        ("overview", &mut animations.overview),
    ] {
        if let Some(animation) = lx::table(table, key, base, errors) {
            *target = parse_animation_override(&animation, &path_key(base, key), errors);
        }
    }
    return animations;
}

fn parse_animation_override(table: &Table, base: &str, errors: &mut Errors) -> AnimationOverride {
    let mut animation = AnimationOverride::default();
    lx::warn_unknown(table, base, &["duration", "curve"], errors);
    if let Some(value) = lx::integer(table, "duration", base, 0, 10_000, errors) {
        animation.duration = Some(value as u16);
    }
    if let Some(value) = lx::string(table, "curve", base, errors) {
        match AnimationCurve::parse(&value) {
            Some(curve) => animation.curve = Some(curve),
            None => errors.error(
                path_key(base, "curve"),
                format!(
                    "unknown curve {value:?} (supported: linear, ease-out-quad, \
                     ease-out-cubic, ease-out-expo)"
                ),
            ),
        }
    }
    return animation;
}

fn parse_mod_key(name: &str) -> Option<ModKey> {
    return match name.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(ModKey::Ctrl),
        "shift" => Some(ModKey::Shift),
        "alt" => Some(ModKey::Alt),
        "super" | "win" => Some(ModKey::Super),
        "iso-level3-shift" | "mod5" => Some(ModKey::IsoLevel3Shift),
        "iso-level5-shift" | "mod3" => Some(ModKey::IsoLevel5Shift),
        _ => None,
    };
}

fn parse_input(table: &Table, errors: &mut Errors) -> Input {
    let mut input = Input::default();
    lx::warn_unknown(table, "input", &["keyboard", "devices"], errors);

    if let Some(t) = lx::table(table, "keyboard", "input", errors) {
        input.keyboard = parse_keyboard(&t, errors);
    }
    if let Some(t) = lx::table(table, "devices", "input", errors) {
        input.devices = parse_devices(&t, errors);
    }
    return input;
}

fn parse_keyboard(table: &Table, errors: &mut Errors) -> Keyboard {
    let mut keyboard = Keyboard::default();
    const KEYS: &[&str] = &[
        "rules",
        "model",
        "layout",
        "variant",
        "options",
        "repeat_rate",
        "repeat_delay",
    ];
    let base = "input.keyboard";
    lx::warn_unknown(table, base, KEYS, errors);

    if let Some(value) = lx::string(table, "rules", base, errors) {
        keyboard.rules = value;
    }
    if let Some(value) = lx::string(table, "model", base, errors) {
        keyboard.model = value;
    }
    if let Some(value) = lx::string(table, "layout", base, errors) {
        keyboard.layout = value;
    }
    if let Some(value) = lx::string(table, "variant", base, errors) {
        keyboard.variant = value;
    }
    if let Some(value) = lx::string(table, "options", base, errors) {
        keyboard.options = Some(value);
    }
    if let Some(value) = lx::integer(table, "repeat_rate", base, 0, 255, errors) {
        keyboard.repeat_rate = value as u8;
    }
    if let Some(value) = lx::integer(table, "repeat_delay", base, 0, 10_000, errors) {
        keyboard.repeat_delay = value as u16;
    }
    return keyboard;
}

fn parse_devices(table: &Table, errors: &mut Errors) -> Vec<(String, Device)> {
    let mut devices = Vec::new();
    for (glob, value) in lx::string_entries(table, "input.devices", errors) {
        let path = path_key("input.devices", &glob);
        let Value::Table(t) = value else {
            errors.error(
                &path,
                format!("expected a table, got {}", lx::type_name(&value)),
            );
            continue;
        };
        devices.push((glob, parse_device(&t, &path, errors)));
    }
    return devices;
}

fn parse_device(table: &Table, base: &str, errors: &mut Errors) -> Device {
    let mut device = Device::default();
    const KEYS: &[&str] = &[
        "tap",
        "drag",
        "drag_lock",
        "dwt",
        "disable_while_typing",
        "natural_scroll",
        "left_handed",
        "click_method",
        "scroll_method",
        "accel",
        "accel_profile",
        "accel_speed",
        "calibration",
        "map_to_output",
    ];
    lx::warn_unknown(table, base, KEYS, errors);

    if let Some(value) = lx::boolean(table, "tap", base, errors) {
        device.tap = Some(value);
    }
    if let Some(value) = lx::boolean(table, "drag", base, errors) {
        device.drag = Some(value);
    }
    if let Some(value) = lx::boolean(table, "drag_lock", base, errors) {
        device.drag_lock = Some(value);
    }
    if let Some(value) = lx::boolean(table, "dwt", base, errors) {
        device.dwt = Some(value);
    }
    // `disable_while_typing` is libinput's name for the same setting.
    if let Some(value) = lx::boolean(table, "disable_while_typing", base, errors) {
        device.dwt = Some(value);
    }
    if let Some(value) = lx::boolean(table, "natural_scroll", base, errors) {
        device.natural_scroll = Some(value);
    }
    if let Some(value) = lx::boolean(table, "left_handed", base, errors) {
        device.left_handed = Some(value);
    }
    if let Some(value) = lx::string(table, "click_method", base, errors) {
        device.click_method = match value.as_str() {
            "button-areas" => Some(ClickMethod::ButtonAreas),
            "clickfinger" => Some(ClickMethod::Clickfinger),
            other => {
                errors.error(
                    path_key(base, "click_method"),
                    format!("invalid click method: {other} (supported: button-areas, clickfinger)"),
                );
                None
            }
        };
    }
    if let Some(value) = lx::string(table, "scroll_method", base, errors) {
        device.scroll_method = match value.as_str() {
            "no-scroll" => Some(ScrollMethod::NoScroll),
            "two-finger" => Some(ScrollMethod::TwoFinger),
            "edge" => Some(ScrollMethod::Edge),
            "on-button-down" => Some(ScrollMethod::OnButtonDown),
            other => {
                errors.error(
                    path_key(base, "scroll_method"),
                    format!(
                        "invalid scroll method: {other} \
                         (supported: no-scroll, two-finger, edge, on-button-down)"
                    ),
                );
                None
            }
        };
    }
    // `accel` is the spelling of the plan's example (§11); `accel_profile` is
    // libinput's name. `accel_profile` wins when both are set.
    let accel_profile = match lx::string(table, "accel_profile", base, errors) {
        Some(profile) => Some(profile),
        None => lx::string(table, "accel", base, errors),
    };
    if let Some(value) = accel_profile {
        device.accel_profile = match value.as_str() {
            "adaptive" => Some(AccelProfile::Adaptive),
            "flat" => Some(AccelProfile::Flat),
            other => {
                errors.error(
                    path_key(base, "accel_profile"),
                    format!("invalid accel profile: {other} (supported: adaptive, flat)"),
                );
                None
            }
        };
    }
    if let Some(value) = lx::number(table, "accel_speed", base, errors) {
        if (-1.0..=1.0).contains(&value) {
            device.accel_speed = Some(value);
        } else {
            errors.error(
                path_key(base, "accel_speed"),
                "value must be between -1.0 and 1.0",
            );
        }
    }
    if let Some(value) = lx::number_list(table, "calibration", base, errors) {
        if value.len() == 6 {
            device.calibration = Some(value);
        } else {
            errors.error(
                path_key(base, "calibration"),
                format!("expected 6 numbers (a 2x3 matrix), got {}", value.len()),
            );
        }
    }
    if let Some(value) = lx::string(table, "map_to_output", base, errors) {
        device.map_to_output = Some(value);
    }
    return device;
}

fn parse_keybinds(table: &Table, errors: &mut Errors) -> Vec<Keybind> {
    let mut keybinds = Vec::new();
    for (name, value) in lx::string_entries(table, "keybinds", errors) {
        let path = path_key("keybinds", &name);
        let key = match name.parse::<Key>() {
            Ok(key) => key,
            Err(err) => {
                errors.error(&path, err.to_string());
                continue;
            }
        };
        let Some((action, repeat)) = parse_bind(&value, &path, errors) else {
            continue;
        };
        keybinds.push(Keybind {
            key,
            action,
            repeat,
        });
    }
    return keybinds;
}

/// A binding value: an action name, an action table, or a table carrying the
/// action plus binding options (`repeat`).
fn parse_bind(value: &Value, path: &str, errors: &mut Errors) -> Option<(Action, bool)> {
    return match value {
        Value::Table(table) => {
            // `repeat` is a Lua keyword, so the option is spelled `key_repeat`
            // (`["repeat"]` is accepted too).
            const KEYS: &[&str] = &[
                "action",
                "key_repeat",
                "repeat",
                "spawn",
                "spawn_shell",
                "set_layout",
                "focus_workspace",
                "focus_window",
                "move_window_to_workspace",
                "layout_action",
            ];
            lx::warn_unknown(table, path, KEYS, errors);
            let repeat = lx::boolean(table, "key_repeat", path, errors)
                .or_else(|| return lx::boolean(table, "repeat", path, errors))
                .unwrap_or(true);
            let action = if let Some(value) = lx::field(table, "action") {
                parse_action_value(&value, &path_key(path, "action"), errors)?
            } else {
                parse_inline_action(table, path, errors)?
            };
            Some((action, repeat))
        }
        other => {
            let action = parse_action_value(other, path, errors)?;
            Some((action, true))
        }
    };
}

/// An action value: a bare name (`"close-window"`) or an action table.
fn parse_action_value(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    return match value {
        Value::String(s) => {
            let Ok(name) = s.to_str() else {
                errors.error(path, "string is not valid UTF-8");
                return None;
            };
            match name.parse::<Action>() {
                Ok(action) => Some(action),
                Err(err) => {
                    let message = match err.hint {
                        Some(hint) => format!("invalid action: {} ({hint})", err.name),
                        None => format!("invalid action: {}", err.name),
                    };
                    errors.error(path, message);
                    None
                }
            }
        }
        Value::Table(table) => parse_inline_action(table, path, errors),
        Value::Function(_) => {
            errors.error(path, "Lua functions as actions are not supported yet");
            None
        }
        other => {
            errors.error(
                path,
                format!(
                    "expected an action (a name string or a table), got {}",
                    lx::type_name(other)
                ),
            );
            None
        }
    };
}

/// The table form of an action: `{ spawn = { … } }`, `{ spawn_shell = "…" }`,
/// `{ set_layout = "name" }`, `{ focus_workspace = 2 }`.
fn parse_inline_action(table: &Table, path: &str, errors: &mut Errors) -> Option<Action> {
    const KEYS: &[&str] = &[
        "spawn",
        "spawn_shell",
        "set_layout",
        "focus_workspace",
        "focus_window",
        "move_window_to_workspace",
        "layout_action",
    ];

    let mut action = None;
    let mut found_any = false;
    for key in KEYS {
        let Some(value) = lx::field(table, key) else {
            continue;
        };
        if found_any {
            errors.error(path, "a binding takes exactly one action");
            break;
        }
        found_any = true;
        let field_path = path_key(path, key);
        action = match *key {
            "spawn" => parse_spawn(&value, &field_path, errors),
            "spawn_shell" => parse_spawn_shell(&value, &field_path, errors),
            "set_layout" => parse_set_layout(&value, &field_path, errors),
            "focus_workspace" => parse_workspace_index(&value, &field_path, errors, true),
            "focus_window" => parse_window_id(&value, &field_path, errors),
            "layout_action" => parse_layout_action(&value, &field_path, errors),
            _ => parse_workspace_index(&value, &field_path, errors, false),
        };
    }

    if !found_any {
        errors.error(
            path,
            "expected an action, e.g. \"close-window\" or { spawn = { \"foot\" } }",
        );
    }
    return action;
}

fn parse_spawn(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    let command = lx::string_list_value(value, path, errors)?;
    if command.is_empty() {
        errors.error(path, "the command must not be empty");
        return None;
    }
    return Some(Action::Spawn { command });
}

fn parse_spawn_shell(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    return match value {
        Value::String(s) => match s.to_str() {
            Ok(line) => Some(Action::SpawnShell {
                command: line.to_owned(),
            }),
            Err(_) => {
                errors.error(path, "string is not valid UTF-8");
                None
            }
        },
        other => {
            errors.error(
                path,
                format!(
                    "expected a shell command string, got {}",
                    lx::type_name(other)
                ),
            );
            None
        }
    };
}

/// `{ layout_action = "focus-left" }` or
/// `{ layout_action = { "resize-column", "0.5" } }`.
fn parse_layout_action(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    match value {
        Value::String(name) => match name.to_str() {
            Ok(name) if !name.is_empty() => Some(Action::LayoutAction {
                name: name.to_owned(),
                args: Vec::new(),
            }),
            Ok(_) => {
                errors.error(path, "layout action names must not be empty");
                None
            }
            Err(_) => {
                errors.error(path, "string is not valid UTF-8");
                None
            }
        },
        Value::Table(table) => {
            let mut values = Vec::new();
            for (index, item) in table.clone().sequence_values::<Value>().enumerate() {
                let item_path = crate::error::path_index(path, index + 1);
                let item = match item {
                    Ok(item) => item,
                    Err(err) => {
                        errors.error(item_path, err.to_string());
                        return None;
                    }
                };
                let Value::String(item) = item else {
                    errors.error(
                        item_path,
                        format!(
                            "expected a layout action name or argument string, got {}",
                            lx::type_name(&item)
                        ),
                    );
                    return None;
                };
                match item.to_str() {
                    Ok(item) => values.push(item.to_owned()),
                    Err(_) => {
                        errors.error(item_path, "string is not valid UTF-8");
                        return None;
                    }
                }
            }
            let Some(name) = values.first().cloned() else {
                errors.error(
                    path,
                    "expected a layout action name, e.g. { layout_action = \"focus-left\" }",
                );
                return None;
            };
            Some(Action::LayoutAction {
                name,
                args: values.into_iter().skip(1).collect(),
            })
        }
        other => {
            errors.error(
                path,
                format!(
                    "expected a layout action name or list, got {}",
                    lx::type_name(other)
                ),
            );
            None
        }
    }
}

fn parse_set_layout(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    return match value {
        Value::String(s) => match s.to_str() {
            Ok(name) => Some(Action::SetLayout {
                name: name.to_owned(),
            }),
            Err(_) => {
                errors.error(path, "string is not valid UTF-8");
                None
            }
        },
        other => {
            errors.error(
                path,
                format!("expected a layout name, got {}", lx::type_name(other)),
            );
            None
        }
    };
}

fn parse_workspace_index(
    value: &Value,
    path: &str,
    errors: &mut Errors,
    focus: bool,
) -> Option<Action> {
    // Workspaces are numbered from 1 in the configuration, so the first
    // workspace is `{ focus_workspace = 1 }`, not 0.
    const MAX_WORKSPACE: i64 = 512;
    let index = match value {
        Value::Integer(index) => *index,
        Value::Number(number) if number.fract() == 0.0 && number.is_finite() => *number as i64,
        other => {
            errors.error(
                path,
                format!("expected a workspace number, got {}", lx::type_name(other)),
            );
            return None;
        }
    };
    if !(1..=MAX_WORKSPACE).contains(&index) {
        errors.error(
            path,
            format!("workspace numbers start at 1 (expected 1 to {MAX_WORKSPACE})"),
        );
        return None;
    }
    let index = index as usize;
    return Some(if focus {
        Action::FocusWorkspace { index }
    } else {
        Action::MoveWindowToWorkspace { index }
    });
}

/// `{ focus_window = 3 }`.
///
/// Window ids only exist while the compositor runs, so this is mostly for
/// scripts; a configuration that binds it targets whatever window happened to
/// get that id.
fn parse_window_id(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    let id = match value {
        Value::Integer(id) => *id,
        Value::Number(number) if number.fract() == 0.0 && number.is_finite() => *number as i64,
        other => {
            errors.error(
                path,
                format!("expected a window id, got {}", lx::type_name(other)),
            );
            return None;
        }
    };
    if id < 1 {
        errors.error(path, "window ids start at 1");
        return None;
    }
    return Some(Action::FocusWindow { id: id as u64 });
}

fn parse_window_drag(table: &Table, errors: &mut Errors) -> WindowDrag {
    let mut drag = WindowDrag::default();
    const KEYS: &[&str] = &[
        "tiled",
        "threshold",
        "axis_lock",
        "escape_cancels",
        "resize_pick",
    ];
    let base = "window_drag";
    lx::warn_unknown(table, base, KEYS, errors);

    if let Some(value) = lx::string(table, "tiled", base, errors) {
        drag.tiled = match value.as_str() {
            "reorder" => TiledDrag::Reorder,
            "float" => TiledDrag::Float,
            "ignore" => TiledDrag::Ignore,
            other => {
                errors.error(
                    path_key(base, "tiled"),
                    format!("invalid tiled drag mode: {other} (supported: reorder, float, ignore)"),
                );
                TiledDrag::Reorder
            }
        };
    }
    if let Some(value) = lx::number(table, "threshold", base, errors) {
        if value >= 0.0 {
            drag.threshold = value;
        } else {
            errors.error(path_key(base, "threshold"), "value must not be negative");
        }
    }
    if let Some(value) = lx::field(table, "axis_lock") {
        match value {
            Value::Boolean(false) => drag.axis_lock = None,
            Value::String(s) => match s.to_str() {
                Ok(name) => match parse_mod_key(&name) {
                    Some(mod_key) => drag.axis_lock = Some(mod_key),
                    None => errors.error(
                        path_key(base, "axis_lock"),
                        "expected a modifier name (ctrl, shift, alt, super) or false",
                    ),
                },
                Err(_) => errors.error(path_key(base, "axis_lock"), "string is not valid UTF-8"),
            },
            other => errors.error(
                path_key(base, "axis_lock"),
                format!(
                    "expected a modifier name or false, got {}",
                    lx::type_name(&other)
                ),
            ),
        }
    }
    if let Some(value) = lx::boolean(table, "escape_cancels", base, errors) {
        drag.escape_cancels = value;
    }
    if let Some(value) = lx::field(table, "resize_pick") {
        match value {
            Value::Integer(pixels) if pixels >= 0 => {
                drag.resize_pick = ResizePick::Pixels(pixels as f64);
            }
            Value::Number(pixels) if pixels >= 0.0 => {
                drag.resize_pick = ResizePick::Pixels(pixels);
            }
            Value::String(s) if s == "thirds" => drag.resize_pick = ResizePick::Thirds,
            other => errors.error(
                path_key(base, "resize_pick"),
                format!(
                    "expected \"thirds\" or a pixel distance, got {}",
                    lx::type_name(&other)
                ),
            ),
        }
    }
    return drag;
}

fn parse_spawn_at_startup(table: &Table, errors: &mut Errors) -> Vec<StartupSpawn> {
    let mut spawns = Vec::new();
    for (index, value) in table.clone().sequence_values::<Value>().enumerate() {
        let path = path_index("spawn_at_startup", index + 1);
        match value {
            // A bare string is a shell command, the common case.
            Ok(Value::String(s)) => match s.to_str() {
                Ok(line) => spawns.push(StartupSpawn::Shell(line.to_owned())),
                Err(_) => errors.error(&path, "string is not valid UTF-8"),
            },
            Ok(Value::Table(t)) => {
                if let Some(spawn) = parse_startup_entry(&t, &path, errors) {
                    spawns.push(spawn);
                }
            }
            Ok(other) => errors.error(
                &path,
                format!(
                    "expected a shell command string or a table, got {}",
                    lx::type_name(&other)
                ),
            ),
            Err(err) => errors.error(&path, err.to_string()),
        }
    }
    return spawns;
}

fn parse_startup_entry(table: &Table, path: &str, errors: &mut Errors) -> Option<StartupSpawn> {
    const KEYS: &[&str] = &["exec", "shell", "spawn", "spawn_shell"];
    lx::warn_unknown(table, path, KEYS, errors);

    if let Some(argv) = lx::string_list(table, "exec", path, errors) {
        return check_argv(argv, &path_key(path, "exec"), errors)
            .map(|command| return StartupSpawn::Exec(command));
    }
    if let Some(line) = lx::string(table, "shell", path, errors) {
        return Some(StartupSpawn::Shell(line));
    }
    // The same action spellings as in `keybinds`.
    if let Some(value) = lx::field(table, "spawn") {
        return match parse_spawn(&value, &path_key(path, "spawn"), errors) {
            Some(Action::Spawn { command }) => Some(StartupSpawn::Exec(command)),
            _ => None,
        };
    }
    if let Some(value) = lx::field(table, "spawn_shell") {
        return match parse_spawn_shell(&value, &path_key(path, "spawn_shell"), errors) {
            Some(Action::SpawnShell { command }) => Some(StartupSpawn::Shell(command)),
            _ => None,
        };
    }
    errors.error(path, "expected { exec = { … } } or { shell = \"…\" }");
    return None;
}

fn check_argv(argv: Vec<String>, path: &str, errors: &mut Errors) -> Option<Vec<String>> {
    if argv.is_empty() {
        errors.error(path, "the command must not be empty");
        return None;
    }
    return Some(argv);
}

fn parse_layer_rules(table: &Table, errors: &mut Errors) -> Vec<LayerRule> {
    let mut rules = Vec::new();
    for (index, value) in table.clone().sequence_values::<Value>().enumerate() {
        let path = path_index("layer_rules", index + 1);
        match value {
            Ok(Value::Table(entry)) => {
                if let Some(rule) = parse_layer_rule(&entry, &path, errors) {
                    rules.push(rule);
                }
            }
            Ok(other) => errors.error(
                &path,
                format!("expected a table, got {}", lx::type_name(&other)),
            ),
            Err(err) => errors.error(&path, err.to_string()),
        }
    }
    return rules;
}

fn parse_layer_rule(table: &Table, path: &str, errors: &mut Errors) -> Option<LayerRule> {
    const KEYS: &[&str] = &["match", "keyboard_interactivity"];
    lx::warn_unknown(table, path, KEYS, errors);

    let mut rule = LayerRule::default();
    if let Some(t) = lx::table(table, "match", path, errors) {
        let match_path = path_key(path, "match");
        lx::warn_unknown(&t, &match_path, &["namespace"], errors);
        rule.namespace = lx::string(&t, "namespace", &match_path, errors);
    }
    if let Some(value) = lx::string(table, "keyboard_interactivity", path, errors) {
        let value_path = path_key(path, "keyboard_interactivity");
        rule.keyboard_interactivity = match value.as_str() {
            "none" => Some(LayerInteractivity::None),
            "on-demand" => Some(LayerInteractivity::OnDemand),
            "exclusive" => Some(LayerInteractivity::Exclusive),
            other => {
                errors.error(
                    value_path,
                    format!(
                        "invalid keyboard interactivity: {other} \
                         (supported: none, on-demand, exclusive)"
                    ),
                );
                None
            }
        };
    }
    if rule.namespace.is_none() && rule.keyboard_interactivity.is_none() {
        errors.error(
            path,
            "a layer rule needs `match.namespace` or `keyboard_interactivity`",
        );
        return None;
    }
    return Some(rule);
}

fn parse_environment(table: &Table, errors: &mut Errors) -> Vec<(String, String)> {
    let mut environment = Vec::new();
    for (name, value) in lx::string_entries(table, "environment", errors) {
        let path = path_key("environment", &name);
        match value {
            Value::String(s) => match s.to_str() {
                Ok(value) => environment.push((name, value.to_owned())),
                Err(_) => errors.error(path, "string is not valid UTF-8"),
            },
            other => errors.error(
                path,
                format!(
                    "expected a string value, got {} (use tostring(…) for numbers)",
                    lx::type_name(&other)
                ),
            ),
        }
    }
    return environment;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Color, LayoutOptionValue, OutputMode, OutputPosition, OutputScale, OutputTransform,
    };

    /// A temp directory that removes itself; test-only plumbing.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("poly-config-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            return Self(dir);
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).unwrap();
            return path;
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn load_text(name: &str, contents: &str) -> ConfigLoadResult {
        let dir = TempDir::new(name);
        let path = dir.write("config.lua", contents);
        return load_file(&path);
    }

    fn expect_config(name: &str, contents: &str) -> (Config, ConfigLoadResult) {
        let result = load_text(name, contents);
        let config = match &result.config {
            Ok(config) => config.clone(),
            Err(err) => panic!("config failed to load: {err}"),
        };
        return (config, result);
    }

    fn expect_errors(name: &str, contents: &str) -> ConfigError {
        let result = load_text(name, contents);
        return match result.config {
            Ok(_) => panic!("config unexpectedly loaded"),
            Err(err) => err,
        };
    }

    /// A minimal context for calling layout actions directly in tests.
    fn layout_context() -> crate::layout::LayoutContext {
        return crate::layout::LayoutContext {
            output: crate::layout::LayoutOutput {
                name: "WL-1".into(),
                width: 1280,
                height: 800,
                scale: 1.0,
                work_area: crate::layout::LayoutRect {
                    x: 0,
                    y: 0,
                    w: 1280,
                    h: 800,
                },
            },
            workspace: crate::layout::LayoutWorkspace { id: 7, index: 1 },
            windows: Vec::new(),
            floating: Vec::new(),
            fullscreen: Vec::new(),
            focused: None,
        };
    }

    /// A tiled window entry for calling the `insert` hook in tests.
    fn layout_entry() -> crate::layout::LayoutWindow {
        return crate::layout::LayoutWindow {
            id: 1,
            app_id: None,
            title: None,
            focused: false,
            rect: crate::layout::LayoutRect {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            min_size: (0, 0),
            max_size: (0, 0),
        };
    }

    /// All error messages as `path: message` strings, for assertions.
    fn error_text(err: &ConfigError) -> String {
        return err
            .messages
            .iter()
            .map(|message| {
                return format!("{}: {}", message.path, message.message);
            })
            .collect::<Vec<_>>()
            .join("\n");
    }

    #[test]
    fn parses_xwayland_section() {
        let (config, result) = expect_config(
            "xwayland",
            r#"
            return {
                xwayland = {
                    enable = true,
                    satellite_path = "/usr/local/bin/xwayland-satellite",
                    on_demand = false,
                    spawn_at_startup = true,
                },
            }
            "#,
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert!(config.xwayland.enable);
        assert_eq!(
            config.xwayland.satellite_path,
            "/usr/local/bin/xwayland-satellite"
        );
        assert!(!config.xwayland.on_demand);
        assert!(config.xwayland.spawn_at_startup);

        // Absent from the file: on demand, nothing started before a client.
        let (config, _) = expect_config("xwayland-defaults", "return {}");
        assert!(config.xwayland.enable);
        assert_eq!(config.xwayland.satellite_path, "xwayland-satellite");
        assert!(config.xwayland.on_demand);
        assert!(!config.xwayland.spawn_at_startup);
    }

    #[test]
    fn rejects_an_empty_satellite_path() {
        let err = expect_errors(
            "xwayland-path",
            r#"
            return { xwayland = { satellite_path = "" } }
            "#,
        );
        assert!(
            error_text(&err).contains("xwayland.satellite_path"),
            "{}",
            error_text(&err)
        );
    }

    #[test]
    fn parses_animation_overrides_and_legacy_duration() {
        let (config, result) = expect_config(
            "animations",
            r#"
            return {
                general = {
                    animations = {
                        enable = true,
                        duration = 240,
                        slowdown = 2.0,
                        window_open = { curve = "linear" },
                        window_close = { duration = 80, curve = "ease-out-quad" },
                        window_movement = { duration = 0 },
                    },
                },
            }
            "#,
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let animations = config.general.animations;
        assert_eq!(animations.duration, 240);
        assert_eq!(animations.slowdown, 2.0);
        // The legacy duration is the fallback; slowdown multiplies it.
        assert_eq!(
            animations.window_open().duration,
            std::time::Duration::from_millis(480)
        );
        assert_eq!(animations.window_open().curve, AnimationCurve::Linear);
        assert_eq!(
            animations.window_close().duration,
            std::time::Duration::from_millis(160)
        );
        assert_eq!(animations.window_close().curve, AnimationCurve::EaseOutQuad);
        // An explicit zero disables just this category.
        assert_eq!(
            animations.window_movement().duration,
            std::time::Duration::ZERO
        );
        // A category without its own duration falls back to the legacy value,
        // including `slowdown`.
        assert_eq!(
            animations.workspace_switch().duration,
            std::time::Duration::from_millis(480)
        );
    }

    #[test]
    fn animation_errors_name_the_field() {
        let err = expect_errors(
            "animations-invalid",
            r#"
            return {
                general = {
                    animations = {
                        slowdown = -1,
                        window_open = { curve = "spring" },
                    },
                },
            }
            "#,
        );
        let text = error_text(&err);
        assert!(text.contains("general.animations.slowdown"), "{text}");
        assert!(
            text.contains("general.animations.window_open.curve"),
            "{text}"
        );
    }

    #[test]
    fn parses_the_overview_settings() {
        let (config, result) = expect_config(
            "overview",
            r##"
            return {
                overview = {
                    zoom = 0.6,
                    backdrop_color = "#101010",
                    card_color = "#303030",
                    shadow = { enable = false, blur = 20, spread = 6, offset = { 1, 2 }, color = "#00000040" },
                },
            }
            "##,
        );

        let overview = &config.overview;
        assert!((overview.zoom - 0.6).abs() < f64::EPSILON);
        assert_eq!(overview.backdrop_color, Color::rgb(0x10, 0x10, 0x10));
        assert_eq!(overview.card_color, Color::rgb(0x30, 0x30, 0x30));
        assert!(!overview.shadow.enable);
        assert_eq!(overview.shadow.blur, 20.0);
        assert_eq!(overview.shadow.spread, 6.0);
        assert_eq!((overview.shadow.offset_x, overview.shadow.offset_y), (1, 2));
        assert_eq!(overview.shadow.color, Color::rgba(0, 0, 0, 0x40));
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }

    #[test]
    fn overview_defaults_follow_niri() {
        let config = Config::default();
        let overview = &config.overview;
        assert!((overview.zoom - 0.5).abs() < f64::EPSILON);
        assert_eq!(overview.backdrop_color, Color::rgb(0x26, 0x26, 0x26));
        assert_eq!(overview.card_color, Color::rgb(0x40, 0x40, 0x40));
        assert!(overview.shadow.enable);
        assert_eq!(overview.shadow.blur, 40.0);
        assert_eq!(overview.shadow.spread, 10.0);
        assert_eq!(
            (overview.shadow.offset_x, overview.shadow.offset_y),
            (0, 10)
        );
        assert_eq!(overview.shadow.color, Color::rgba(0, 0, 0, 0x50));
    }

    #[test]
    fn overview_zoom_out_of_range_is_an_error() {
        let err = expect_errors(
            "overview-zoom",
            r##"
            return { overview = { zoom = 1.5 } }
            "##,
        );
        assert!(err.to_string().contains("overview.zoom"), "{err}");
    }

    #[test]
    fn parses_visual_settings() {
        let (config, result) = expect_config(
            "visuals",
            r##"
            return {
                general = {
                    border = { width = 2, radius = 12, active = "#ff0000", inactive = "#11223344" },
                    shadow = { enable = true, blur = 24, offset = { 0, 6 }, color = "#000000a0" },
                    blur = { enable = true, passes = 4, offset = 2.5, noise = 0.1,
                             accept_client_regions = false,
                             shader = { up = "up.frag", finish = "~/finish.frag" } },
                    opacity = 0.9,
                    window_shader = "glass.frag",
                },
            }
            "##,
        );

        let border = &config.general.border;
        assert_eq!(border.width, 2.0);
        assert_eq!(border.radius, 12.0);
        assert_eq!(border.active, Color::rgb(0xff, 0x00, 0x00));
        assert_eq!(border.inactive, Color::rgba(0x11, 0x22, 0x33, 0x44));

        let shadow = &config.general.shadow;
        assert!(shadow.enable);
        assert_eq!(shadow.blur, 24.0);
        assert_eq!((shadow.offset_x, shadow.offset_y), (0, 6));
        assert_eq!(shadow.color, Color::rgba(0, 0, 0, 0xa0));

        let blur = &config.general.blur;
        assert!(blur.enable);
        assert_eq!(blur.passes, 4);
        assert_eq!(blur.offset, 2.5);
        assert_eq!(blur.noise, 0.1);
        assert!(!blur.accept_client_regions);
        assert!(blur.shader.down.is_none());
        assert!(blur.shader.up.as_deref().unwrap().ends_with("up.frag"));
        assert!(
            blur.shader
                .finish
                .as_deref()
                .unwrap()
                .ends_with("finish.frag")
        );

        assert!((config.general.opacity - 0.9).abs() < f64::EPSILON);
        assert!(
            config
                .general
                .window_shader
                .as_deref()
                .unwrap()
                .ends_with("glass.frag")
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }

    #[test]
    fn visual_setting_errors_are_reported() {
        let err = expect_errors(
            "visual-errors",
            r##"
            return {
                general = {
                    border = { width = -1, radius = 5000, active = "red" },
                    shadow = { blur = -2, offset = { 1 }, color = "#12" },
                    blur = { passes = 12, offset = -1, noise = 2, shader = { finish = 3 } },
                    opacity = 1.5,
                },
            }
            "##,
        );
        let text = error_text(&err);
        for expected in [
            "general.border.width",
            "general.border.radius",
            "general.border.active",
            "general.shadow.blur",
            "general.shadow.offset",
            "general.shadow.color",
            "general.blur.passes",
            "general.blur.offset",
            "general.blur.noise",
            "general.blur.shader.finish",
            "general.opacity",
        ] {
            assert!(text.contains(expected), "missing {expected} in:\n{text}");
        }
    }

    #[test]
    fn parses_actions_that_carry_an_argument() {
        let (config, result) = expect_config(
            "action-args",
            r#"
            return {
                keybinds = {
                    ["Mod+3"] = { focus_workspace = 3 },
                    ["Mod+5"] = { focus_window = 5 },
                    ["Mod+4"] = { move_window_to_workspace = 4 },
                    ["Mod+D"] = { set_layout = "dwindle" },
                    ["Mod+E"] = { layout_action = "focus-left" },
                    ["Mod+Shift+E"] = { layout_action = { "resize-column", "0.5" } },
                },
            }
            "#,
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let action = |name: &str| -> Action {
            return config
                .keybinds
                .iter()
                .find(|bind| return bind.key == name.parse::<Key>().unwrap())
                .map(|bind| return bind.action.clone())
                .expect("binding missing");
        };
        assert_eq!(action("Mod+3"), Action::FocusWorkspace { index: 3 });
        assert_eq!(action("Mod+5"), Action::FocusWindow { id: 5 });
        assert_eq!(action("Mod+4"), Action::MoveWindowToWorkspace { index: 4 });
        assert_eq!(
            action("Mod+D"),
            Action::SetLayout {
                name: "dwindle".into(),
            }
        );
        assert_eq!(
            action("Mod+E"),
            Action::LayoutAction {
                name: "focus-left".into(),
                args: Vec::new(),
            }
        );
        assert_eq!(
            action("Mod+Shift+E"),
            Action::LayoutAction {
                name: "resize-column".into(),
                args: vec!["0.5".into()],
            }
        );
    }

    #[test]
    fn parses_layout_actions_and_keybinds() {
        let (config, result) = expect_config(
            "layout-actions",
            r#"
            return {
                layouts = {
                    scrolling = {
                        arrange = function(ctx) return {} end,
                        actions = {
                            ["focus-left"] = function(ctx) return { focus = 1 } end,
                            ["resize-column"] = function(ctx, args)
                                return { relayout = true, width = args[1] }
                            end,
                            ["bad-effects"] = function(ctx)
                                return { focus = "not a window id" }
                            end,
                        },
                        keybinds = {
                            ["Mod+H"] = "focus-left",
                            ["Mod+Shift+H"] = {
                                action = "resize-column",
                                args = { "0.5" },
                                key_repeat = false,
                            },
                        },
                        insert = function(ctx, window) return 1 end,
                    },
                },
                default_layout = "scrolling",
            }
            "#,
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(config.layouts.len(), 1);
        let layout = &config.layouts[0];
        assert_eq!(layout.name, "scrolling");
        let bind = |name: &str| -> &Keybind {
            return layout
                .keybinds
                .iter()
                .find(|bind| return bind.key == name.parse::<Key>().unwrap())
                .unwrap_or_else(|| panic!("no binding for {name}"));
        };
        assert_eq!(
            bind("Mod+H").action,
            Action::LayoutAction {
                name: "focus-left".into(),
                args: Vec::new(),
            }
        );
        assert!(bind("Mod+H").repeat);
        assert_eq!(
            bind("Mod+Shift+H").action,
            Action::LayoutAction {
                name: "resize-column".into(),
                args: vec!["0.5".into()],
            }
        );
        assert!(!bind("Mod+Shift+H").repeat);

        let programs = result
            .layout_programs
            .as_ref()
            .expect("the layout programs should stay alive");
        let context = layout_context();
        let effects = programs
            .invoke_action("scrolling", "focus-left", &context, &[])
            .unwrap();
        assert_eq!(effects.focus, Some(1));
        let effects = programs
            .invoke_action("scrolling", "resize-column", &context, &["0.5".into()])
            .unwrap();
        assert!(effects.relayout);
        assert!(
            programs
                .invoke_action("scrolling", "missing", &context, &[])
                .is_err()
        );
        assert!(
            programs
                .invoke_action("scrolling", "bad-effects", &context, &[])
                .is_err()
        );

        assert_eq!(
            programs
                .insert("scrolling", &context, &layout_entry())
                .unwrap(),
            Some(0)
        );
    }

    #[test]
    fn layout_insert_must_return_a_position() {
        let (_config, result) = expect_config(
            "layout-bad-insert",
            r#"
            return {
                layouts = {
                    scrolling = {
                        arrange = function(ctx) return {} end,
                        insert = function(ctx, window) return 0 end,
                    },
                },
            }
            "#,
        );
        let programs = result
            .layout_programs
            .as_ref()
            .expect("the layout programs should stay alive");
        assert!(
            programs
                .insert("scrolling", &layout_context(), &layout_entry())
                .is_err()
        );
    }

    #[test]
    fn layout_keybinds_must_name_a_declared_action() {
        let err = expect_errors(
            "layout-unknown-action",
            r#"
            return {
                layouts = {
                    scrolling = {
                        arrange = function(ctx) return {} end,
                        actions = {},
                        keybinds = { ["Mod+H"] = "focus-left" },
                    },
                },
            }
            "#,
        );
        let text = error_text(&err);
        assert!(text.contains("layouts.scrolling.keybinds"), "{text}");
        assert!(text.contains("no action named"), "{text}");
    }

    #[test]
    fn loads_a_full_config() {
        let (config, result) = expect_config(
            "full",
            r#"
            return {
                general = { mod_key = "super", prefer_no_csd = true, gap = 12 },
                input = {
                    keyboard = { layout = "us,cn", options = "grp:alt_shift_toggle", repeat_rate = 40 },
                    devices = {
                        ["*touchpad*"] = { tap = true, natural_scroll = true, scroll_method = "two-finger" },
                    },
                },
                keybinds = {
                    ["Mod+Return"] = { spawn = { "foot" } },
                    ["Mod+Shift+Q"] = "close-window",
                    ["Mod+V"] = poly.spawn_shell("foot | cat"),
                    ["Mod+F"] = "toggle-floating",
                    ["Mod+R"] = { action = "resize-window", key_repeat = false },
                },
                window_drag = { tiled = "ignore", threshold = 4, escape_cancels = false },
                spawn_at_startup = { { exec = { "waybar" } }, "wl-paste --watch cliphist store" },
                environment = { POLY_TEST = "1" },
                layer_rules = {
                    { match = { namespace = "waybar" }, keyboard_interactivity = "none" },
                    { keyboard_interactivity = "on-demand" },
                },
            }
            "#,
        );

        assert_eq!(config.general.mod_key, Some(ModKey::Super));
        assert_eq!(config.general.gap, 12);
        assert_eq!(config.input.keyboard.layout, "us,cn");
        assert_eq!(
            config.input.keyboard.options.as_deref(),
            Some("grp:alt_shift_toggle")
        );
        assert_eq!(config.input.keyboard.repeat_rate, 40);
        assert_eq!(config.input.devices.len(), 1);
        assert_eq!(config.input.devices[0].0, "*touchpad*");
        assert_eq!(config.input.devices[0].1.tap, Some(true));
        assert_eq!(
            config.input.devices[0].1.scroll_method,
            Some(ScrollMethod::TwoFinger)
        );

        assert_eq!(config.keybinds.len(), 5);
        // Lua table order is unspecified, look the bindings up by trigger.
        let bind = |name: &str| -> &Keybind {
            return config
                .keybinds
                .iter()
                .find(|bind| return bind.key == name.parse::<Key>().unwrap())
                .unwrap_or_else(|| panic!("no binding for {name}"));
        };
        assert_eq!(
            bind("Mod+Return").action,
            Action::Spawn {
                command: vec!["foot".to_owned()]
            }
        );
        assert!(bind("Mod+Return").repeat);
        assert_eq!(bind("Mod+Shift+Q").action, Action::CloseWindow);
        assert_eq!(
            bind("Mod+V").action,
            Action::SpawnShell {
                command: "foot | cat".to_owned()
            }
        );
        assert_eq!(bind("Mod+F").action, Action::ToggleFloating);
        assert_eq!(bind("Mod+R").action, Action::ResizeWindow);
        assert!(!bind("Mod+R").repeat);

        assert_eq!(config.window_drag.tiled, TiledDrag::Ignore);
        assert_eq!(config.window_drag.threshold, 4.0);
        assert!(!config.window_drag.escape_cancels);
        assert_eq!(
            config.spawn_at_startup,
            vec![
                StartupSpawn::Exec(vec!["waybar".to_owned()]),
                StartupSpawn::Shell("wl-paste --watch cliphist store".to_owned()),
            ]
        );
        assert_eq!(
            config.environment,
            vec![("POLY_TEST".to_owned(), "1".to_owned())]
        );
        assert_eq!(
            config.layer_rules,
            vec![
                LayerRule {
                    namespace: Some("waybar".to_owned()),
                    keyboard_interactivity: Some(LayerInteractivity::None),
                },
                LayerRule {
                    namespace: None,
                    keyboard_interactivity: Some(LayerInteractivity::OnDemand),
                },
            ]
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }

    #[test]
    fn layer_rule_errors_are_reported() {
        let result = load_text(
            "layer-rules",
            r#"
            return {
                layer_rules = {
                    { keyboard_interactivity = "sometimes" },
                    { match = { namespace = 42 } },
                    { match = { namespac = "waybar" }, keyboard_interactivity = "none" },
                    {},
                },
            }
            "#,
        );
        let err = match &result.config {
            Ok(_) => panic!("config unexpectedly loaded"),
            Err(err) => err,
        };
        let text = error_text(err);
        assert!(
            text.contains(
                r#"layer_rules[1].keyboard_interactivity: invalid keyboard interactivity: sometimes"#
            ),
            "{text}"
        );
        assert!(
            text.contains("layer_rules[2].match.namespace: expected a string"),
            "{text}"
        );
        assert!(
            text.contains("layer_rules[4]: a layer rule needs"),
            "{text}"
        );
        let warnings = result
            .warnings
            .iter()
            .map(|message| return message.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            warnings.contains(r#"layer_rules[3].match.namespac: unknown setting"#),
            "{warnings}"
        );
    }

    #[test]
    fn collects_every_problem() {
        let err = expect_errors(
            "problems",
            r#"
            return {
                general = { gap = -1, mod_key = "hyper" },
                keybinds = {
                    ["Mod+Frobnicate"] = "close-window",
                    ["Mod+Q"] = "frobnicate",
                    ["Mod+W"] = { spawn = {} },
                    ["Mod+E"] = 42,
                },
                unknown_setting = true,
            }
            "#,
        );
        let text = error_text(&err);
        assert!(
            text.contains(r#"general.mod_key: invalid mod key: hyper"#),
            "{text}"
        );
        assert!(
            text.contains("general.gap: value must be between 0 and 512"),
            "{text}"
        );
        assert!(
            text.contains(r#"keybinds["Mod+Frobnicate"]: invalid key"#),
            "{text}"
        );
        assert!(
            text.contains(r#"keybinds["Mod+Q"]: invalid action: frobnicate"#),
            "{text}"
        );
        assert!(
            text.contains(r#"keybinds["Mod+W"].spawn: the command must not be empty"#),
            "{text}"
        );
        assert!(
            text.contains(r#"keybinds["Mod+E"]: expected an action"#),
            "{text}"
        );
    }

    #[test]
    fn unknown_settings_are_warnings() {
        let (_, result) = expect_config(
            "warnings",
            r#"return { unknown_setting = true, general = { color = "red" } }"#,
        );
        let text: Vec<String> = result
            .warnings
            .iter()
            .map(|message| return format!("{}: {}", message.path, message.message))
            .collect();
        assert_eq!(text.len(), 2, "{text:?}");
        assert!(text[0].starts_with("unknown_setting: "), "{text:?}");
        assert!(text[1].starts_with("general.color: "), "{text:?}");
    }

    #[test]
    fn lua_errors_carry_a_traceback() {
        let err = expect_errors("lua-error", "local x = nil\nreturn { gap = x.field }");
        let text = error_text(&err);
        assert!(text.contains("config.lua:2"), "{text}");
        assert!(text.contains("attempt to index"), "{text}");
        assert!(text.contains("stack traceback"), "{text}");
    }

    #[test]
    fn setup_form_works() {
        let (config, _) = expect_config(
            "setup",
            r#"poly.setup{ keybinds = { ["Mod+Q"] = "quit" } }"#,
        );
        assert_eq!(config.keybinds.len(), 1);
        assert_eq!(config.keybinds[0].action, Action::Quit);
    }

    #[test]
    fn includes_resolve_relative_and_are_tracked() {
        let dir = TempDir::new("include");
        let root = dir.write(
            "config.lua",
            r#"
            local theme = poly.include("theme.lua")
            return { general = { gap = theme.gap } }
            "#,
        );
        let theme = dir.write("theme.lua", "return { gap = 21 }");

        let result = load_file(&root);
        let config = match &result.config {
            Ok(config) => config.clone(),
            Err(err) => panic!("config failed to load: {err}"),
        };
        assert_eq!(config.general.gap, 21);
        assert_eq!(
            result.includes,
            vec![
                std::fs::canonicalize(&root).unwrap(),
                std::fs::canonicalize(&theme).unwrap(),
            ]
        );
    }

    #[test]
    fn include_cycles_are_reported_with_the_chain() {
        let dir = TempDir::new("cycle");
        let root = dir.write("config.lua", r#"return poly.include("b.lua")"#);
        dir.write("b.lua", r#"return poly.include("config.lua")"#);

        let err = match load_file(&root).config {
            Ok(_) => panic!("cycle went unnoticed"),
            Err(err) => err,
        };
        let text = error_text(&err);
        assert!(text.contains("include cycle"), "{text}");
        assert!(text.contains("config.lua -> "), "{text}");
    }

    #[test]
    fn missing_include_is_an_error() {
        let err = expect_errors("missing", r#"return poly.include("nope.lua")"#);
        let text = error_text(&err);
        assert!(text.contains("cannot read included file"), "{text}");
        assert!(text.contains("nope.lua"), "{text}");
    }

    #[test]
    fn read_file_returns_contents_or_nil() {
        let dir = TempDir::new("read-file");
        let root = dir.write(
            "config.lua",
            r#"
            local data, err = poly.read_file("words.txt")
            local missing, missing_err = poly.read_file("nope.txt")
            return { general = { gap = data == nil and 0 or tonumber(data) or 0 } }
            "#,
        );
        dir.write("words.txt", "33");

        let result = load_file(&root);
        match &result.config {
            Ok(config) => assert_eq!(config.general.gap, 33),
            Err(err) => panic!("config failed to load: {err}"),
        }
    }

    #[test]
    fn runaway_configs_are_stopped() {
        let result = load_text("runaway", "while true do end");
        let err = match result.config {
            Ok(_) => panic!("runaway config was allowed"),
            Err(err) => err,
        };
        let text = error_text(&err);
        assert!(text.contains("ran too long"), "{text}");
    }

    #[test]
    fn loader_frames_are_stripped_from_tracebacks() {
        let text = "/c/config.lua:4: cannot read included file x: no such file\n\
                    stack traceback:\n\
                    \tpoly/runner.lua:9: in function <poly/runner.lua:5>\n\
                    \t[C]: in function 'error'\n\
                    \tpoly/include.lua:4: in field 'include'\n\
                    \t/c/config.lua:4: in main chunk\n\
                    \t[C]: in function 'xpcall'\n\
                    \t(...tail calls...)\n";
        let stripped = strip_loader_frames(text);
        assert!(
            stripped.starts_with("/c/config.lua:4: cannot read"),
            "{stripped}"
        );
        assert!(stripped.contains("stack traceback:"), "{stripped}");
        assert!(
            stripped.contains("/c/config.lua:4: in main chunk"),
            "{stripped}"
        );
        assert!(!stripped.contains("poly/"), "{stripped}");
        assert!(!stripped.contains("[C]: in"), "{stripped}");
        assert!(!stripped.contains("tail calls"), "{stripped}");

        // When every frame was ours, the heading has nothing left to introduce.
        let only_ours = "/c/config.lua:1: boom\nstack traceback:\n\tpoly/runner.lua:9: in function <poly/runner.lua:5>\n";
        assert_eq!(strip_loader_frames(only_ours), "/c/config.lua:1: boom");
    }

    #[test]
    fn include_errors_carry_the_config_position() {
        let dir = TempDir::new("include-position");
        let root = dir.write(
            "config.lua",
            "local theme = poly.include(\"theme.lua\")\nreturn { general = { gap = theme.gap } }",
        );
        let result = load_file(&root);
        let err = match result.config {
            Ok(_) => panic!("missing include went unnoticed"),
            Err(err) => err,
        };
        let text = error_text(&err);
        assert!(text.contains("cannot read included file"), "{text}");
        // The position is the `poly.include` call in the root file (line 1).
        assert!(text.contains("config.lua:1"), "{text}");
        assert!(!text.contains("poly/runner.lua"), "{text}");
    }

    #[test]
    fn parses_output_display_settings() {
        let (config, result) = expect_config(
            "outputs",
            r##"
            return {
                outputs = {
                    ["eDP-1"] = {
                        mode = "2560x1600@60",
                        scale = 1.5,
                        transform = "90",
                        position = { x = 0, y = 0 },
                        vrr = true,
                        focus = true,
                        background = "#101010",
                        layout_options = { master_ratio = 0.7 },
                    },
                    ["DP-*"] = { mode = "preferred", scale = "auto", position = "auto" },
                },
            }
            "##,
        );
        assert!(
            result.warnings.is_empty(),
            "unexpected warnings: {:?}",
            result.warnings
        );

        let internal = config.output_for("eDP-1").expect("eDP-1 config");
        assert_eq!(
            internal.mode,
            Some(OutputMode::Size {
                width: 2560,
                height: 1600,
                refresh: Some(60_000),
            })
        );
        assert_eq!(internal.scale, Some(OutputScale::Fixed(1.5)));
        assert_eq!(internal.transform, Some(OutputTransform::Rotate90));
        assert_eq!(
            internal.position,
            Some(OutputPosition::Fixed { x: 0, y: 0 })
        );
        assert_eq!(internal.vrr, Some(true));
        assert_eq!(internal.focus, Some(true));
        assert_eq!(internal.background, Some(Color::rgb(0x10, 0x10, 0x10)));
        assert_eq!(internal.layout_options.len(), 1);

        // A glob entry matches whatever the exact name does not.
        let external = config.output_for("DP-3").expect("DP-3 config");
        assert_eq!(external.mode, Some(OutputMode::Preferred));
        assert_eq!(external.scale, Some(OutputScale::Auto));
        assert_eq!(external.position, Some(OutputPosition::Auto));
        assert_eq!(external.transform, None);

        // Exact names win over globs, no matter what Lua's `pairs()` returns.
        assert_eq!(
            config.output_for("eDP-1").map(|config| return config.mode),
            Some(Some(OutputMode::Size {
                width: 2560,
                height: 1600,
                refresh: Some(60_000),
            }))
        );
        assert!(config.output_for("HDMI-A-1").is_none());
    }

    #[test]
    fn exact_output_names_beat_globs() {
        let (config, _) = expect_config(
            "outputs-specific",
            r#"
            return {
                outputs = {
                    ["*"] = { scale = 2.0 },
                    ["DP-?"] = { scale = 1.5 },
                    ["DP-1"] = { scale = 1.0 },
                },
            }
            "#,
        );
        let patterns = config
            .outputs
            .iter()
            .map(|(pattern, _)| return pattern.as_str())
            .collect::<Vec<_>>();
        assert_eq!(patterns, ["DP-1", "DP-?", "*"]);
        assert_eq!(
            config.output_for("DP-1").unwrap().scale,
            Some(OutputScale::Fixed(1.0))
        );
        assert_eq!(
            config.output_for("DP-2").unwrap().scale,
            Some(OutputScale::Fixed(1.5))
        );
        assert_eq!(
            config.output_for("eDP-1").unwrap().scale,
            Some(OutputScale::Fixed(2.0))
        );
    }

    #[test]
    fn output_entries_merge_from_general_to_specific() {
        let (config, _) = expect_config(
            "outputs-merge",
            r##"
            return {
                outputs = {
                    ["*"] = {
                        scale = 2.0,
                        vrr = true,
                        background = "#000000",
                        layout_options = { master_ratio = 0.5, view = "rows" },
                    },
                    ["eDP-1"] = {
                        scale = 1.0,
                        background = "#101010",
                        layout_options = { master_ratio = 0.7 },
                    },
                },
            }
            "##,
        );
        let merged = config.output_for("eDP-1").expect("eDP-1 config");
        // The specific entry wins where both set a field...
        assert_eq!(merged.scale, Some(OutputScale::Fixed(1.0)));
        assert_eq!(merged.background, Some(Color::rgb(0x10, 0x10, 0x10)));
        // ...and the glob still provides what it does not mention.
        assert_eq!(merged.vrr, Some(true));
        // `layout_options` merge key by key.
        assert_eq!(
            merged.layout_options,
            vec![
                ("master_ratio".to_owned(), LayoutOptionValue::Number(0.7)),
                (
                    "view".to_owned(),
                    LayoutOptionValue::String("rows".to_owned())
                ),
            ]
        );
        // The glob alone is still a complete answer for another output.
        let other = config.output_for("HDMI-A-1").expect("HDMI-A-1 config");
        assert_eq!(other.scale, Some(OutputScale::Fixed(2.0)));
        assert_eq!(other.background, Some(Color::rgb(0, 0, 0)));
    }

    #[test]
    fn output_display_settings_are_validated() {
        let err = expect_errors(
            "outputs-invalid",
            r#"
            return {
                outputs = {
                    ["eDP-1"] = {
                        mode = "big",
                        scale = 9.0,
                        transform = "sideways",
                        position = { x = 0 },
                        background = "rebeccapurple",
                    },
                },
            }
            "#,
        );
        let text = error_text(&err);
        for expected in [
            "outputs[\"eDP-1\"].mode: invalid mode",
            "outputs[\"eDP-1\"].scale: invalid scale",
            "outputs[\"eDP-1\"].transform: invalid transform",
            "outputs[\"eDP-1\"].position: invalid position",
            "outputs[\"eDP-1\"].background: invalid color",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn output_mode_and_transform_spellings() {
        assert_eq!(
            OutputTransform::parse("flipped-270"),
            Some(OutputTransform::Flipped270)
        );
        assert_eq!(
            OutputTransform::parse(" Rotate180 "),
            Some(OutputTransform::Rotate180)
        );
        assert_eq!(OutputTransform::parse("nope"), None);

        let (config, _) = expect_config(
            "outputs-modes",
            r#"
            return {
                outputs = {
                    ["eDP-1"] = { mode = "1920x1080" },
                    ["eDP-2"] = { mode = "PREFERRED" },
                    ["eDP-3"] = { mode = "1920x1080@59.94" },
                },
            }
            "#,
        );
        assert_eq!(
            config.output_for("eDP-1").unwrap().mode,
            Some(OutputMode::Size {
                width: 1920,
                height: 1080,
                refresh: None,
            })
        );
        assert_eq!(
            config.output_for("eDP-2").unwrap().mode,
            Some(OutputMode::Preferred)
        );
        assert_eq!(
            config.output_for("eDP-3").unwrap().mode,
            Some(OutputMode::Size {
                width: 1920,
                height: 1080,
                refresh: Some(59_940),
            })
        );
    }
}
