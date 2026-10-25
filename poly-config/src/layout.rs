//! The Lua layout bridge (`docs/plan.md` §9.2).
//!
//! The configuration loader keeps the `Lua` state alive after parsing when the
//! file defines named layouts. The compositor then calls [`LayoutPrograms::arrange`]
//! whenever it needs geometry; a layout error is reported as [`LayoutError`] and
//! the caller falls back to the built-in column layout.
//!
//! Every call runs with its own instruction budget and a `pcall`-style error
//! boundary, so a broken layout cannot hang or crash the event loop
//! (`AGENTS.md` §2.7).

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use mlua::{Function, HookTriggers, Lua, Table, Value, VmState};

use crate::config::{
    Color, Config, LayoutInfo, LayoutOptionValue, OutputConfig, OutputMode, OutputPosition,
    OutputScale, OutputTransform, WindowRule,
};
use crate::error::{Errors, path_key};
use crate::lua_ext as lx;

/// Instructions one `arrange` call may execute (`docs/plan.md` §9.2).
const ARRANGE_BUDGET: u64 = 200_000;

/// How often the arrange hook checks the budget.
const ARRANGE_STEP: u32 = 1_000;

/// A rectangle in logical, compositor coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width, always positive.
    pub w: i32,
    /// Height, always positive.
    pub h: i32,
}

/// One window passed to `arrange(ctx)`.
#[derive(Debug, Clone)]
pub struct LayoutWindow {
    /// Stable window id, shared with IPC.
    pub id: u64,
    /// `app_id`, when the client set one.
    pub app_id: Option<String>,
    /// Window title.
    pub title: Option<String>,
    /// Whether this is the focused window.
    pub focused: bool,
    /// Current geometry.
    pub rect: LayoutRect,
    /// Client minimum size (`0` means unconstrained).
    pub min_size: (i32, i32),
    /// Client maximum size (`0` means unconstrained).
    pub max_size: (i32, i32),
}

/// The output part of `ctx.output`.
#[derive(Debug, Clone)]
pub struct LayoutOutput {
    /// Output name (`eDP-1`, `WL-1`, …).
    pub name: String,
    /// Width in logical pixels.
    pub width: i32,
    /// Height in logical pixels.
    pub height: i32,
    /// Fractional scale.
    pub scale: f64,
    /// Work area in logical pixels (output minus exclusive layer zones).
    pub work_area: LayoutRect,
}

/// Everything a layout sees for one output.
#[derive(Debug, Clone)]
pub struct LayoutContext {
    /// The output being arranged.
    pub output: LayoutOutput,
    /// Tiled windows, in layout order.
    pub windows: Vec<LayoutWindow>,
    /// Floating window ids, bottom to top.
    pub floating: Vec<u64>,
    /// Fullscreen window ids.
    pub fullscreen: Vec<u64>,
    /// The focused window id, if any.
    pub focused: Option<u64>,
}

/// A layout failed and the caller should keep the previous geometry or use the
/// built-in fallback.
#[derive(Debug, Clone)]
pub struct LayoutError {
    /// Human-readable error, including the Lua traceback when available.
    pub message: String,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return write!(f, "{}", self.message);
    }
}

impl std::error::Error for LayoutError {}

struct Program {
    name: String,
    handles_fullscreen: bool,
    arrange: Function,
    state: Table,
}

/// Lua layouts that outlive the configuration load.
///
/// Holds the only `Lua` state in the compositor. It must stay on the thread
/// that loaded the configuration (`AGENTS.md` §2.5).
pub struct LayoutPrograms {
    lua: Lua,
    poly: Table,
    programs: Vec<Program>,
    output_options: Vec<(String, OutputConfig)>,
    relayout: Rc<Cell<bool>>,
}

impl fmt::Debug for LayoutPrograms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self
            .programs
            .iter()
            .map(|program| program.name.as_str())
            .collect::<Vec<_>>();
        return f
            .debug_struct("LayoutPrograms")
            .field("layouts", &names)
            .finish();
    }
}

impl LayoutPrograms {
    /// Parse `layouts` and `default_layout`, fill the matching [`Config`]
    /// fields and keep the functions alive.
    ///
    /// Returns `None` when the configuration defines no Lua layouts.
    pub(crate) fn parse(
        lua: &Lua,
        root: &Table,
        config: &mut Config,
        errors: &mut Errors,
    ) -> Option<Self> {
        let Some(layouts) = lx::table(root, "layouts", "", errors) else {
            if let Some(name) = lx::string(root, "default_layout", "", errors) {
                if name != "columns" {
                    errors.error(
                        "default_layout",
                        format!("no layout named {name:?} is defined"),
                    );
                }
                config.default_layout = Some(name);
            }
            return None;
        };

        let Some(poly) = lua.globals().get::<Option<Table>>("poly").ok().flatten() else {
            errors.file("internal error: the poly table disappeared");
            return None;
        };
        let relayout = Rc::new(Cell::new(false));
        let poly_layout = lua.create_table().ok()?;
        {
            let relayout = relayout.clone();
            let function = lua
                .create_function(move |_lua, ()| {
                    relayout.set(true);
                    return Ok(());
                })
                .ok()?;
            if let Err(err) = poly_layout.set("relayout", function) {
                errors.file(format!("cannot install poly.layout.relayout: {err}"));
                return None;
            }
        }
        if let Err(err) = poly.set("layout", poly_layout) {
            errors.file(format!("cannot install poly.layout: {err}"));
            return None;
        }
        if let Err(err) = poly.set("gap", config.general.gap) {
            errors.file(format!("cannot install poly.gap: {err}"));
            return None;
        }

        let mut programs = Vec::new();
        let mut metadata = Vec::new();
        for (name, value) in lx::string_entries(&layouts, "layouts", errors) {
            let path = path_key("layouts", &name);
            let (arrange, setup, handles_fullscreen) = match value {
                Value::Function(function) => (function, None, false),
                Value::Table(table) => {
                    lx::warn_unknown(
                        &table,
                        &path,
                        &["arrange", "setup", "handles_fullscreen"],
                        errors,
                    );
                    let arrange = match lx::field(&table, "arrange") {
                        Some(Value::Function(function)) => function,
                        Some(value) => {
                            errors.error(
                                path_key(&path, "arrange"),
                                format!("expected a function, got {}", lx::type_name(&value)),
                            );
                            continue;
                        }
                        None => {
                            errors.error(&path, "a layout table needs an `arrange` function");
                            continue;
                        }
                    };
                    let setup = match lx::field(&table, "setup") {
                        Some(Value::Function(function)) => Some(function),
                        Some(value) => {
                            errors.error(
                                path_key(&path, "setup"),
                                format!("expected a function, got {}", lx::type_name(&value)),
                            );
                            None
                        }
                        None => None,
                    };
                    let handles_fullscreen =
                        lx::boolean(&table, "handles_fullscreen", &path, errors).unwrap_or(false);
                    (arrange, setup, handles_fullscreen)
                }
                other => {
                    errors.error(
                        &path,
                        format!(
                            "expected a function or a layout table, got {}",
                            lx::type_name(&other)
                        ),
                    );
                    continue;
                }
            };

            let state = match lua.create_table() {
                Ok(table) => table,
                Err(err) => {
                    errors.error(&path, format!("cannot create layout state: {err}"));
                    continue;
                }
            };
            if let Err(err) = poly.set("state", state.clone()) {
                errors.error(&path, format!("cannot install poly.state: {err}"));
                continue;
            }
            if let Some(setup) = setup {
                if let Err(err) = setup.call::<()>(poly.clone()) {
                    errors.error(
                        &path,
                        format!("layout setup failed: {}", format_lua_error(&err)),
                    );
                }
            }

            metadata.push(LayoutInfo {
                name: name.clone(),
                handles_fullscreen,
            });
            programs.push(Program {
                name,
                handles_fullscreen,
                arrange,
                state,
            });
        }

        let default_layout = lx::string(root, "default_layout", "", errors);
        if let Some(name) = &default_layout {
            let known = name == "columns" || metadata.iter().any(|info| &info.name == name);
            if !known {
                errors.error(
                    "default_layout",
                    format!("no layout named {name:?} is defined"),
                );
            }
        }
        config.layouts = metadata;
        config.default_layout = default_layout;
        if programs.is_empty() {
            return None;
        }

        return Some(Self {
            lua: lua.clone(),
            poly,
            programs,
            output_options: config.outputs.clone(),
            relayout,
        });
    }

    /// Names of the configured layouts, in configuration order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        return self.programs.iter().map(|program| program.name.as_str());
    }

    /// Whether a layout exists.
    pub fn has(&self, name: &str) -> bool {
        return self.programs.iter().any(|program| program.name == name);
    }

    /// Whether a layout positions fullscreen windows itself.
    pub fn handles_fullscreen(&self, name: &str) -> bool {
        return self
            .programs
            .iter()
            .find(|program| program.name == name)
            .is_some_and(|program| program.handles_fullscreen);
    }

    /// Whether a layout called `poly.layout.relayout()` since the last check.
    pub fn take_relayout(&self) -> bool {
        let requested = self.relayout.get();
        self.relayout.set(false);
        return requested;
    }

    /// Call `arrange(ctx)` for one output.
    pub fn arrange(
        &self,
        name: &str,
        context: &LayoutContext,
    ) -> Result<HashMap<u64, LayoutRect>, LayoutError> {
        let Some(program) = self.programs.iter().find(|program| program.name == name) else {
            return Err(LayoutError {
                message: format!("no layout named {name:?}"),
            });
        };

        if let Err(err) = self.poly.set("state", program.state.clone()) {
            return Err(LayoutError {
                message: format!("cannot install layout state: {err}"),
            });
        }
        let options = self.options_for(&context.output.name);
        let context_table = build_context(&self.lua, context, options.as_ref())?;

        let budget = Rc::new(Cell::new(ARRANGE_BUDGET));
        let hook_budget = budget.clone();
        if let Err(err) = self.lua.set_hook(
            HookTriggers::new().every_nth_instruction(ARRANGE_STEP),
            move |_lua, _debug| -> mlua::Result<VmState> {
                let left = hook_budget.get();
                if left == 0 {
                    return Err(mlua::Error::runtime("layout ran too long (infinite loop?)"));
                }
                hook_budget.set(left.saturating_sub(u64::from(ARRANGE_STEP)));
                return Ok(VmState::Continue);
            },
        ) {
            return Err(LayoutError {
                message: format!("cannot install the layout budget hook: {err}"),
            });
        }
        let result = program.arrange.call::<Value>(context_table);
        self.lua.remove_hook();
        let value = result.map_err(|err| {
            return LayoutError {
                message: format_lua_error(&err),
            };
        })?;

        return parse_result(value);
    }

    /// The `layout_options` of the most specific matching `outputs` entry,
    /// merged with the defaults a glob entry provides.
    fn options_for(&self, output: &str) -> Option<OutputConfig> {
        return crate::config::resolved_output(&self.output_options, output);
    }
}

fn build_context(
    lua: &Lua,
    context: &LayoutContext,
    options: Option<&OutputConfig>,
) -> Result<Table, LayoutError> {
    let table = lua.create_table().map_err(|err| {
        return LayoutError {
            message: format!("cannot create the layout context: {err}"),
        };
    })?;

    let output = lua.create_table().map_err(context_error)?;
    output
        .set("name", context.output.name.clone())
        .map_err(context_error)?;
    output
        .set("width", context.output.width)
        .map_err(context_error)?;
    output
        .set("height", context.output.height)
        .map_err(context_error)?;
    output
        .set("scale", context.output.scale)
        .map_err(context_error)?;
    let work_area = lua.create_table().map_err(context_error)?;
    work_area
        .set("x", context.output.work_area.x)
        .map_err(context_error)?;
    work_area
        .set("y", context.output.work_area.y)
        .map_err(context_error)?;
    work_area
        .set("w", context.output.work_area.w)
        .map_err(context_error)?;
    work_area
        .set("h", context.output.work_area.h)
        .map_err(context_error)?;
    output.set("work_area", work_area).map_err(context_error)?;
    table.set("output", output).map_err(context_error)?;

    let windows = lua.create_table().map_err(context_error)?;
    for (index, window) in context.windows.iter().enumerate() {
        let entry = lua.create_table().map_err(context_error)?;
        entry.set("id", window.id).map_err(context_error)?;
        if let Some(app_id) = &window.app_id {
            entry.set("app_id", app_id.clone()).map_err(context_error)?;
        }
        if let Some(title) = &window.title {
            entry.set("title", title.clone()).map_err(context_error)?;
        }
        entry
            .set("focused", window.focused)
            .map_err(context_error)?;
        entry.set("x", window.rect.x).map_err(context_error)?;
        entry.set("y", window.rect.y).map_err(context_error)?;
        entry.set("w", window.rect.w).map_err(context_error)?;
        entry.set("h", window.rect.h).map_err(context_error)?;
        let geometry = lua.create_table().map_err(context_error)?;
        geometry.set("x", window.rect.x).map_err(context_error)?;
        geometry.set("y", window.rect.y).map_err(context_error)?;
        geometry.set("w", window.rect.w).map_err(context_error)?;
        geometry.set("h", window.rect.h).map_err(context_error)?;
        entry.set("geometry", geometry).map_err(context_error)?;
        let min_size = lua.create_table().map_err(context_error)?;
        min_size
            .set("w", window.min_size.0)
            .map_err(context_error)?;
        min_size
            .set("h", window.min_size.1)
            .map_err(context_error)?;
        entry.set("min_size", min_size).map_err(context_error)?;
        let max_size = lua.create_table().map_err(context_error)?;
        max_size
            .set("w", window.max_size.0)
            .map_err(context_error)?;
        max_size
            .set("h", window.max_size.1)
            .map_err(context_error)?;
        entry.set("max_size", max_size).map_err(context_error)?;
        windows.set(index + 1, entry).map_err(context_error)?;
    }
    table.set("windows", windows).map_err(context_error)?;

    let floating = lua.create_table().map_err(context_error)?;
    for (index, id) in context.floating.iter().enumerate() {
        floating.set(index + 1, *id).map_err(context_error)?;
    }
    table.set("floating", floating).map_err(context_error)?;
    let fullscreen = lua.create_table().map_err(context_error)?;
    for (index, id) in context.fullscreen.iter().enumerate() {
        fullscreen.set(index + 1, *id).map_err(context_error)?;
    }
    table.set("fullscreen", fullscreen).map_err(context_error)?;
    if let Some(id) = context.focused {
        table.set("focused", id).map_err(context_error)?;
    }

    let options_table = lua.create_table().map_err(context_error)?;
    if let Some(options) = options {
        for (key, value) in &options.layout_options {
            let value = match value {
                LayoutOptionValue::Bool(value) => Value::Boolean(*value),
                LayoutOptionValue::Integer(value) => Value::Integer(*value),
                LayoutOptionValue::Number(value) => Value::Number(*value),
                LayoutOptionValue::String(value) => {
                    Value::String(lua.create_string(value).map_err(context_error)?)
                }
            };
            options_table
                .set(key.as_str(), value)
                .map_err(context_error)?;
        }
    }
    table.set("options", options_table).map_err(context_error)?;

    return Ok(table);
}

fn context_error(err: mlua::Error) -> LayoutError {
    return LayoutError {
        message: format!("cannot build the layout context: {err}"),
    };
}

fn parse_result(value: Value) -> Result<HashMap<u64, LayoutRect>, LayoutError> {
    let table = match value {
        Value::Table(table) => table,
        other => {
            return Err(LayoutError {
                message: format!(
                    "arrange() must return a table of geometries, got {}",
                    lx::type_name(&other)
                ),
            });
        }
    };

    let mut result = HashMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|err| {
            return LayoutError {
                message: format!("cannot read arrange() result: {err}"),
            };
        })?;
        let id = match key {
            Value::Integer(id) if id >= 0 => id as u64,
            Value::Number(id) if id >= 0.0 && id.fract() == 0.0 => id as u64,
            other => {
                return Err(LayoutError {
                    message: format!(
                        "arrange() returned a non-window key of type {}",
                        lx::type_name(&other)
                    ),
                });
            }
        };
        let rect = match value {
            Value::Table(rect) => rect,
            other => {
                return Err(LayoutError {
                    message: format!(
                        "arrange() geometry for window {id} is a {}, expected a table",
                        lx::type_name(&other)
                    ),
                });
            }
        };
        let x = number_field(&rect, "x", id)?;
        let y = number_field(&rect, "y", id)?;
        let w = number_field(&rect, "w", id)?;
        let h = number_field(&rect, "h", id)?;
        if w <= 0 || h <= 0 {
            return Err(LayoutError {
                message: format!("arrange() geometry for window {id} has a non-positive size"),
            });
        }
        result.insert(id, LayoutRect { x, y, w, h });
    }
    return Ok(result);
}

fn number_field(table: &Table, key: &str, id: u64) -> Result<i32, LayoutError> {
    let value = table.get::<Value>(key).map_err(|err| {
        return LayoutError {
            message: format!("cannot read window {id} field {key}: {err}"),
        };
    })?;
    let number = match value {
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        other => {
            return Err(LayoutError {
                message: format!(
                    "window {id} field {key} is a {}, expected a number",
                    lx::type_name(&other)
                ),
            });
        }
    };
    if !number.is_finite() || number < i32::MIN as f64 || number > i32::MAX as f64 {
        return Err(LayoutError {
            message: format!("window {id} field {key} is out of range"),
        });
    }
    return Ok(number.round() as i32);
}

fn format_lua_error(err: &mlua::Error) -> String {
    match err {
        mlua::Error::CallbackError { traceback, cause } => {
            let cause = format_lua_error(cause);
            if cause.contains("stack traceback") {
                cause
            } else {
                format!("{cause}\n{traceback}")
            }
        }
        mlua::Error::RuntimeError(message) => message.clone(),
        err => err.to_string(),
    }
}

/// Parse `window_rules` into the configuration.
pub(crate) fn parse_window_rules(
    table: &Table,
    root: &std::path::Path,
    errors: &mut Errors,
) -> Vec<WindowRule> {
    let mut rules = Vec::new();
    for (index, value) in table.clone().sequence_values::<Value>().enumerate() {
        let path = crate::error::path_index("window_rules", index + 1);
        match value {
            Ok(Value::Table(entry)) => {
                if let Some(rule) = parse_window_rule(&entry, &path, root, errors) {
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

fn parse_window_rule(
    table: &Table,
    path: &str,
    root: &std::path::Path,
    errors: &mut Errors,
) -> Option<WindowRule> {
    const KEYS: &[&str] = &[
        "match",
        "float",
        "focus",
        "open_on_output",
        "blur",
        "radius",
        "shadow",
        "opacity",
        "no_csd",
        "window_shader",
    ];
    lx::warn_unknown(table, path, KEYS, errors);

    let mut rule = WindowRule::default();
    if let Some(matcher) = lx::table(table, "match", path, errors) {
        let match_path = path_key(path, "match");
        lx::warn_unknown(&matcher, &match_path, &["app_id", "title"], errors);
        rule.app_id = lx::string(&matcher, "app_id", &match_path, errors);
        rule.title = lx::string(&matcher, "title", &match_path, errors);
    }
    rule.float = lx::boolean(table, "float", path, errors);
    rule.focus = lx::boolean(table, "focus", path, errors);
    rule.open_on_output = lx::string(table, "open_on_output", path, errors);
    rule.blur = lx::boolean(table, "blur", path, errors);
    rule.radius = lx::number(table, "radius", path, errors);
    rule.shadow = lx::boolean(table, "shadow", path, errors);
    rule.opacity = lx::number(table, "opacity", path, errors);
    rule.no_csd = lx::boolean(table, "no_csd", path, errors);
    if let Some(value) = lx::string(table, "window_shader", path, errors) {
        rule.window_shader = Some(
            crate::lua::resolve_path(Some(root), &value)
                .display()
                .to_string(),
        );
    }
    if let Some(opacity) = rule.opacity {
        if !(0.0..=1.0).contains(&opacity) {
            errors.error(
                path_key(path, "opacity"),
                "value must be between 0.0 and 1.0",
            );
            rule.opacity = None;
        }
    }
    if rule.app_id.is_none()
        && rule.title.is_none()
        && rule.float.is_none()
        && rule.focus.is_none()
        && rule.open_on_output.is_none()
        && rule.blur.is_none()
        && rule.radius.is_none()
        && rule.shadow.is_none()
        && rule.opacity.is_none()
        && rule.no_csd.is_none()
        && rule.window_shader.is_none()
    {
        errors.error(path, "an empty window rule matches every window");
    }
    return Some(rule);
}

/// Parse `outputs.<glob>`: display settings and `layout_options`.
pub(crate) fn parse_outputs(table: &Table, errors: &mut Errors) -> Vec<(String, OutputConfig)> {
    let mut outputs = Vec::new();
    for (pattern, value) in lx::string_entries(table, "outputs", errors) {
        let path = path_key("outputs", &pattern);
        let Value::Table(entry) = value else {
            errors.error(
                &path,
                format!("expected a table, got {}", lx::type_name(&value)),
            );
            continue;
        };
        const KEYS: &[&str] = &[
            "mode",
            "scale",
            "transform",
            "position",
            "vrr",
            "focus",
            "background",
            "layout_options",
        ];
        lx::warn_unknown(&entry, &path, KEYS, errors);

        let mut output = OutputConfig::default();
        parse_output_display(&entry, &path, &mut output, errors);
        if let Some(options) = lx::table(&entry, "layout_options", &path, errors) {
            let options_path = path_key(&path, "layout_options");
            for (key, value) in lx::string_entries(&options, &options_path, errors) {
                let option = match value {
                    Value::Boolean(value) => LayoutOptionValue::Bool(value),
                    Value::Integer(value) => LayoutOptionValue::Integer(value),
                    Value::Number(value) if value.is_finite() => LayoutOptionValue::Number(value),
                    Value::String(value) => match value.to_str() {
                        Ok(value) => LayoutOptionValue::String(value.to_owned()),
                        Err(_) => {
                            errors
                                .error(path_key(&options_path, &key), "string is not valid UTF-8");
                            continue;
                        }
                    },
                    other => {
                        errors.error(
                            path_key(&options_path, &key),
                            format!(
                                "expected a boolean, number or string, got {}",
                                lx::type_name(&other)
                            ),
                        );
                        continue;
                    }
                };
                output.layout_options.push((key, option));
            }
        }
        // Lua tables have no order; sort so that a configuration means the
        // same thing on every run.
        output
            .layout_options
            .sort_by(|left, right| return left.0.cmp(&right.0));
        outputs.push((pattern, output));
    }
    // Lua tables have no order, so "first match wins" needs one: exact names
    // before globs, more literal characters before fewer.
    crate::config::sort_output_patterns(&mut outputs);
    return outputs;
}

/// Parse the display settings of one `outputs.<pattern>` table.
fn parse_output_display(entry: &Table, path: &str, output: &mut OutputConfig, errors: &mut Errors) {
    if let Some(value) = lx::string(entry, "mode", path, errors) {
        match parse_output_mode(&value) {
            Some(mode) => output.mode = Some(mode),
            None => errors.error(
                path_key(path, "mode"),
                format!(
                    "invalid mode {value:?} (expected \"preferred\" or \"<width>x<height>[@<hz>]\")"
                ),
            ),
        }
    }
    if let Some(value) = lx::field(entry, "scale") {
        match parse_output_scale(&value) {
            Some(scale) => output.scale = Some(scale),
            None => errors.error(
                path_key(path, "scale"),
                format!(
                    "invalid scale {} (expected \"auto\" or a number between 0.1 and 4.0)",
                    describe_value(&value)
                ),
            ),
        }
    }
    if let Some(value) = lx::string(entry, "transform", path, errors) {
        match OutputTransform::parse(&value) {
            Some(transform) => output.transform = Some(transform),
            None => errors.error(
                path_key(path, "transform"),
                format!(
                    "invalid transform {value:?} (expected normal, 90, 180, 270, \
                     flipped, flipped-90, flipped-180 or flipped-270)"
                ),
            ),
        }
    }
    if let Some(value) = lx::field(entry, "position") {
        match parse_output_position(&value) {
            Some(position) => output.position = Some(position),
            None => errors.error(
                path_key(path, "position"),
                format!(
                    "invalid position {} (expected \"auto\" or a table {{ x = 0, y = 0 }})",
                    describe_value(&value)
                ),
            ),
        }
    }
    if let Some(value) = lx::boolean(entry, "vrr", path, errors) {
        output.vrr = Some(value);
    }
    if let Some(value) = lx::boolean(entry, "focus", path, errors) {
        output.focus = Some(value);
    }
    if let Some(value) = lx::string(entry, "background", path, errors) {
        match Color::parse(&value) {
            Some(color) => output.background = Some(color),
            None => errors.error(
                path_key(path, "background"),
                format!("invalid color: {value} (expected #rgb, #rrggbb or #rrggbbaa)"),
            ),
        }
    }
}

/// Parse `"preferred"` or `"<width>x<height>[@<hz>]"`.
fn parse_output_mode(text: &str) -> Option<OutputMode> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("preferred") {
        return Some(OutputMode::Preferred);
    }
    let (size, refresh) = match text.split_once('@') {
        Some((size, refresh)) => (size, Some(refresh)),
        None => (text, None),
    };
    let (width, height) = size.split_once(['x', 'X'])?;
    let width: u16 = width.trim().parse().ok()?;
    let height: u16 = height.trim().parse().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let refresh = match refresh {
        Some(refresh) => {
            let hz: f64 = refresh.trim().parse().ok()?;
            // A typo in the refresh rate is a configuration error, not
            // something to silently round to the nearest mode.
            if !hz.is_finite() || !(1.0..=1000.0).contains(&hz) {
                return None;
            }
            Some((hz * 1000.0).round() as u32)
        }
        None => None,
    };
    return Some(OutputMode::Size {
        width,
        height,
        refresh,
    });
}

/// Parse `"auto"` or a fractional scale factor.
fn parse_output_scale(value: &Value) -> Option<OutputScale> {
    let scale = match value {
        Value::String(text) => {
            if text.to_str().ok()?.trim().eq_ignore_ascii_case("auto") {
                return Some(OutputScale::Auto);
            }
            text.to_str().ok()?.trim().parse::<f64>().ok()?
        }
        Value::Integer(number) => *number as f64,
        Value::Number(number) => *number,
        _ => return None,
    };
    // The same guard rail the runtime path uses: 0 or a NaN would divide by
    // zero in the render path.
    if !scale.is_finite() || !(0.1..=4.0).contains(&scale) {
        return None;
    }
    return Some(OutputScale::Fixed(scale));
}

/// Parse `"auto"` or `{ x = 0, y = 0 }` / `{ 0, 0 }`.
fn parse_output_position(value: &Value) -> Option<OutputPosition> {
    let Value::Table(table) = value else {
        if let Value::String(text) = value {
            if text.to_str().ok()?.trim().eq_ignore_ascii_case("auto") {
                return Some(OutputPosition::Auto);
            }
        }
        return None;
    };
    let x = position_component(table, "x", 1)?;
    let y = position_component(table, "y", 2)?;
    return Some(OutputPosition::Fixed { x, y });
}

/// One coordinate of a position table, by name or by list index.
fn position_component(table: &Table, key: &str, index: usize) -> Option<i32> {
    let value = match lx::field(table, key) {
        Some(value) => value,
        None => table
            .get::<Value>(index as i64)
            .ok()
            .filter(|value| return !value.is_nil())?,
    };
    let number = match value {
        Value::Integer(number) => number as f64,
        Value::Number(number) => number,
        _ => return None,
    };
    if !number.is_finite() || number.fract() != 0.0 {
        return None;
    }
    let number = number as i64;
    return i32::try_from(number).ok();
}

/// A short, type-aware description of a value for error messages.
fn describe_value(value: &Value) -> String {
    return match value {
        Value::String(text) => match text.to_str() {
            Ok(text) => format!("{text:?}"),
            Err(_) => "a non-UTF-8 string".to_owned(),
        },
        Value::Integer(number) => number.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Table(_) => "a table".to_owned(),
        other => lx::type_name(other).to_owned(),
    };
}
