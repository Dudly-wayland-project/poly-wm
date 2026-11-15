//! The JavaScript layout bridge.
//!
//! The configuration loader keeps the QuickJS runtime alive after parsing when
//! the file defines named layouts. The compositor then calls
//! [`LayoutPrograms::arrange`] whenever it needs geometry; a layout error is
//! reported as [`LayoutError`] and the caller falls back to the built-in column
//! layout.
//!
//! Every call runs with its own wall-clock budget and an exception boundary, so
//! a broken layout cannot hang or crash the event loop (`AGENTS.md` §2.7).
//!
//! The runtime and its handles are `!Send` and stay on the thread that loaded
//! the configuration (`AGENTS.md` §2.5). The [`Persistent`] handles in
//! [`LayoutPrograms::programs`] must be dropped before the context they belong
//! to, which is why the field comes first: struct fields drop in declaration
//! order.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use poly_ipc::Action;
use rquickjs::{Array, Context, Ctx, Function, IntoJs, Object, Persistent, Value as JsValue};

use crate::binds::{Key, Keybind};
use crate::config::{Config, LayoutInfo, LayoutOptionValue, OutputConfig};
use crate::error::{Errors, path_index, path_key};
use crate::js::budget::Budget;
use crate::js::builder::LayoutEntry;
use crate::js::ext as ex;
use crate::js::value::Value;

/// How long one layout call (`arrange`, an action, `insert` or `setup`) may
/// run.
const ARRANGE_BUDGET: Duration = Duration::from_millis(10);

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

/// Identity of the workspace a layout call runs for.
///
/// The id survives reordering the workspace stack (moving a workspace up or
/// down), so a layout can bucket its own state by it. It is only stable for the
/// lifetime of the compositor process, like window ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutWorkspace {
    /// Stable workspace id.
    pub id: u64,
    /// 1-based position in the output's workspace stack.
    pub index: usize,
}

/// Everything a layout sees for one output.
#[derive(Debug, Clone)]
pub struct LayoutContext {
    /// The output being arranged.
    pub output: LayoutOutput,
    /// The workspace being arranged.
    pub workspace: LayoutWorkspace,
    /// Tiled windows, in layout order.
    pub windows: Vec<LayoutWindow>,
    /// Floating window ids, bottom to top.
    pub floating: Vec<u64>,
    /// Fullscreen window ids.
    pub fullscreen: Vec<u64>,
    /// The focused window id, if any.
    pub focused: Option<u64>,
}

/// State changes requested by a layout action.
///
/// Actions never mutate compositor state directly: they return this object and
/// the layout manager applies the effects in one place. This keeps the Rust
/// window order authoritative while JavaScript owns column grouping and
/// viewport state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutEffects {
    /// Window id that should receive keyboard focus.
    pub focus: Option<u64>,
    /// New tiled order, which must be a permutation of the current one.
    pub order: Option<Vec<u64>>,
    /// 1-based workspace to switch to on the action's output.
    pub switch_workspace: Option<usize>,
    /// Ask for another `arrange` call even if nothing else changed.
    pub relayout: bool,
}

/// A layout failed and the caller should keep the previous geometry or use the
/// built-in fallback.
#[derive(Debug, Clone)]
pub struct LayoutError {
    /// Human-readable error, including the JavaScript stack when available.
    pub message: String,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return write!(f, "{}", self.message);
    }
}

impl std::error::Error for LayoutError {}

/// One named layout, with its JavaScript handles.
struct Program {
    name: String,
    handles_fullscreen: bool,
    arrange: Persistent<Function<'static>>,
    /// `actions.<name> = ctx => …`.
    actions: HashMap<String, Persistent<Function<'static>>>,
    /// Optional hook deciding where a newly mapped window is inserted.
    insert: Option<Persistent<Function<'static>>>,
    /// The object behind `ctx.state`, private to this layout.
    state: Persistent<Object<'static>>,
}

/// JavaScript layouts that outlive the configuration load.
///
/// Holds the only QuickJS runtime in the compositor. It must stay on the thread
/// that loaded the configuration (`AGENTS.md` §2.5).
pub struct LayoutPrograms {
    /// Handles first: they must be dropped before the context and runtime.
    programs: Vec<Program>,
    context: Context,
    output_options: Vec<(String, OutputConfig)>,
    relayout: Rc<Cell<bool>>,
    gap: i32,
    budget: Arc<Budget>,
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
    /// Validate the `layout()` definitions, fill the matching [`Config`]
    /// fields and keep the functions alive.
    ///
    /// Returns `None` when the configuration defines no layouts. The caller
    /// owns the runtime; it is moved into the returned value only when layouts
    /// exist, so a configuration without them does not keep a QuickJS runtime
    /// alive for the lifetime of the compositor.
    pub(crate) fn build(
        context: Context,
        entries: Vec<LayoutEntry>,
        budget: Arc<Budget>,
        config: &mut Config,
        errors: &mut Errors,
    ) -> Option<Self> {
        let gap = config.general.gap;
        let relayout = Rc::new(Cell::new(false));
        let mut programs = Vec::new();
        let mut metadata = Vec::new();

        context.with(|ctx| {
            for LayoutEntry { name, value } in entries {
                let path = path_key("layouts", &name);
                let value = match value.restore(&ctx) {
                    Ok(value) => value,
                    Err(err) => {
                        errors.error(&path, format!("cannot restore the layout: {err}"));
                        continue;
                    }
                };

                let parsed = parse_program(
                    &ctx,
                    &name,
                    &path,
                    value,
                    LayoutEnv {
                        relayout: &relayout,
                        gap,
                        budget: &budget,
                    },
                    errors,
                );
                let Some((program, info)) = parsed else {
                    continue;
                };
                metadata.push(info);
                programs.push(program);
            }
        });

        // `default_layout` was already parsed from the root object; now that
        // the definitions are known, check that it names one of them.
        if let Some(name) = &config.default_layout {
            let known = name == "columns" || metadata.iter().any(|info| return &info.name == name);
            if !known {
                errors.error(
                    "default_layout",
                    format!("no layout named {name:?} is defined"),
                );
            }
        }
        config.layouts = metadata;
        if programs.is_empty() {
            return None;
        }

        return Some(Self {
            programs,
            context,
            output_options: config.outputs.clone(),
            relayout,
            gap,
            budget,
        });
    }

    /// Names of the configured layouts, in configuration order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        return self
            .programs
            .iter()
            .map(|program| return program.name.as_str());
    }

    /// Whether a layout exists.
    pub fn has(&self, name: &str) -> bool {
        return self
            .programs
            .iter()
            .any(|program| return program.name == name);
    }

    /// Whether a layout positions fullscreen windows itself.
    pub fn handles_fullscreen(&self, name: &str) -> bool {
        return self
            .programs
            .iter()
            .find(|program| return program.name == name)
            .is_some_and(|program| return program.handles_fullscreen);
    }

    /// Whether a layout called `ctx.relayout()` since the last check.
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
        let Some(program) = self.find(name) else {
            return Err(no_layout(name));
        };
        let options = self.options_for(&context.output.name);

        return self.context.with(|ctx| {
            let context_object = self.build_context(&ctx, program, context, options.as_ref())?;
            let arrange = program.arrange.clone().restore(&ctx).map_err(|err| {
                return LayoutError {
                    message: format!("cannot restore layout {name:?}: {err}"),
                };
            })?;
            let value = self.with_budget(&ctx, || {
                return arrange.call::<_, JsValue>((context_object,));
            })?;
            return parse_result(&ctx, value);
        });
    }

    /// Call an action declared by a layout.
    ///
    /// The handler receives the same context object `arrange` gets, plus an
    /// array of the string arguments from the keybinding or IPC request. It
    /// must return an object of effects (or nothing).
    pub fn invoke_action(
        &self,
        layout: &str,
        action: &str,
        context: &LayoutContext,
        args: &[String],
    ) -> Result<LayoutEffects, LayoutError> {
        let Some(program) = self.find(layout) else {
            return Err(no_layout(layout));
        };
        let Some(function) = program.actions.get(action) else {
            let mut names = program.actions.keys().cloned().collect::<Vec<_>>();
            names.sort();
            let defined = if names.is_empty() {
                "it defines no actions".to_owned()
            } else {
                format!("defined actions: {}", names.join(", "))
            };
            return Err(LayoutError {
                message: format!("layout {layout:?} has no action named {action:?} ({defined})"),
            });
        };
        let options = self.options_for(&context.output.name);

        return self.context.with(|ctx| {
            let context_object = self.build_context(&ctx, program, context, options.as_ref())?;
            let args_array =
                Array::new(ctx.clone()).map_err(|err| return context_error(&ctx, &err))?;
            for (index, arg) in args.iter().enumerate() {
                args_array
                    .set(index, arg.as_str())
                    .map_err(|err| return context_error(&ctx, &err))?;
            }
            let function = function.clone().restore(&ctx).map_err(|err| {
                return LayoutError {
                    message: format!("cannot restore action {action:?}: {err}"),
                };
            })?;
            let value = self.with_budget(&ctx, || {
                return function.call::<_, JsValue>((context_object, args_array));
            })?;
            return parse_effects(&ctx, value);
        });
    }

    /// Ask the layout where a newly mapped window should go.
    ///
    /// Returns a 0-based insertion index, or `None` when the layout has no
    /// `insert` hook or returns nothing.
    pub fn insert(
        &self,
        layout: &str,
        context: &LayoutContext,
        window: &LayoutWindow,
    ) -> Result<Option<usize>, LayoutError> {
        let Some(program) = self.find(layout) else {
            return Err(no_layout(layout));
        };
        let Some(function) = &program.insert else {
            return Ok(None);
        };
        let options = self.options_for(&context.output.name);

        return self.context.with(|ctx| {
            let context_object = self.build_context(&ctx, program, context, options.as_ref())?;
            let window_object = build_window(&ctx, window)?;
            let function = function.clone().restore(&ctx).map_err(|err| {
                return LayoutError {
                    message: format!("cannot restore the insert hook of {layout:?}: {err}"),
                };
            })?;
            let value = self.with_budget(&ctx, || {
                return function.call::<_, JsValue>((context_object, window_object));
            })?;
            if value.is_null() || value.is_undefined() {
                return Ok(None);
            }
            let kind = value.type_name();
            return match value.as_number() {
                Some(index) if index.fract() == 0.0 && index >= 1.0 => Ok(Some(index as usize - 1)),
                _ => Err(LayoutError {
                    message: format!("insert() must return a position from 1, got {kind}"),
                }),
            };
        });
    }

    fn find(&self, name: &str) -> Option<&Program> {
        return self
            .programs
            .iter()
            .find(|program| return program.name == name);
    }

    /// Run one JavaScript call with the `arrange` budget.
    fn with_budget<T>(
        &self,
        ctx: &Ctx<'_>,
        call: impl FnOnce() -> rquickjs::Result<T>,
    ) -> Result<T, LayoutError> {
        self.budget.arm(ARRANGE_BUDGET);
        let result = call();
        let expired = self.budget.expired();
        self.budget.disarm();
        return match result {
            Ok(value) => Ok(value),
            Err(_err) if expired => Err(LayoutError {
                message: "the layout ran too long (infinite loop?)".to_owned(),
            }),
            Err(err) => Err(LayoutError {
                message: super::format_error(ctx, &err),
            }),
        };
    }

    /// Build the `ctx` object one layout call receives.
    fn build_context<'js>(
        &self,
        ctx: &Ctx<'js>,
        program: &Program,
        context: &LayoutContext,
        options: Option<&OutputConfig>,
    ) -> Result<Object<'js>, LayoutError> {
        let table = new_object(ctx)?;

        let output = new_object(ctx)?;
        set(ctx, &output, "name", context.output.name.as_str())?;
        set(ctx, &output, "width", context.output.width)?;
        set(ctx, &output, "height", context.output.height)?;
        set(ctx, &output, "scale", context.output.scale)?;
        set(
            ctx,
            &output,
            "work_area",
            rect_object(ctx, &context.output.work_area)?,
        )?;
        set(ctx, &table, "output", output)?;

        let workspace = new_object(ctx)?;
        set(ctx, &workspace, "id", context.workspace.id)?;
        set(ctx, &workspace, "index", context.workspace.index)?;
        set(ctx, &table, "workspace", workspace)?;

        let windows = new_array(ctx)?;
        for (index, window) in context.windows.iter().enumerate() {
            windows
                .set(index, build_window(ctx, window)?)
                .map_err(|err| return context_error(ctx, &err))?;
        }
        set(ctx, &table, "windows", windows)?;

        let floating = new_array(ctx)?;
        for (index, id) in context.floating.iter().enumerate() {
            floating
                .set(index, *id)
                .map_err(|err| return context_error(ctx, &err))?;
        }
        set(ctx, &table, "floating", floating)?;

        let fullscreen = new_array(ctx)?;
        for (index, id) in context.fullscreen.iter().enumerate() {
            fullscreen
                .set(index, *id)
                .map_err(|err| return context_error(ctx, &err))?;
        }
        set(ctx, &table, "fullscreen", fullscreen)?;

        if let Some(id) = context.focused {
            set(ctx, &table, "focused", id)?;
        }

        let options_object = new_object(ctx)?;
        if let Some(options) = options {
            for (key, value) in &options.layout_options {
                match value {
                    LayoutOptionValue::Bool(value) => set(ctx, &options_object, key, *value)?,
                    LayoutOptionValue::Integer(value) => set(ctx, &options_object, key, *value)?,
                    LayoutOptionValue::Number(value) => set(ctx, &options_object, key, *value)?,
                    LayoutOptionValue::String(value) => {
                        set(ctx, &options_object, key, value.as_str())?
                    }
                }
            }
        }
        set(ctx, &table, "options", options_object)?;

        set(ctx, &table, "gap", self.gap)?;

        let state = program.state.clone().restore(ctx).map_err(|err| {
            return LayoutError {
                message: format!("cannot restore the layout state: {err}"),
            };
        })?;
        set(ctx, &table, "state", state)?;
        set(ctx, &table, "relayout", self.relayout_function(ctx)?)?;

        return Ok(table);
    }

    /// The `ctx.relayout()` helper, bound to this layout set's flag.
    fn relayout_function<'js>(&self, ctx: &Ctx<'js>) -> Result<Function<'js>, LayoutError> {
        let relayout = self.relayout.clone();
        return Function::new(ctx.clone(), move || return relayout.set(true))
            .map_err(|err| return context_error(ctx, &err));
    }

    /// The `layout_options` of the most specific matching `outputs` entry,
    /// merged with the defaults a glob entry provides.
    fn options_for(&self, output: &str) -> Option<OutputConfig> {
        return crate::config::resolved_output(&self.output_options, output);
    }
}

/// The layout-wide environment shared by every `layout()` definition in one
/// configuration.
///
/// Bundled so the per-definition parser and the helpers it calls only carry
/// what is specific to that definition.
#[derive(Clone, Copy)]
struct LayoutEnv<'a> {
    /// Set by `ctx.relayout()`, consumed by [`LayoutPrograms::take_relayout`].
    relayout: &'a Rc<Cell<bool>>,
    /// `general.gap`, exposed as `ctx.gap`.
    gap: i32,
    /// Armed around every call into JavaScript.
    budget: &'a Budget,
}

/// Parse one `layout(name, …)` definition.
fn parse_program<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    path: &str,
    value: JsValue<'js>,
    env: LayoutEnv<'_>,
    errors: &mut Errors,
) -> Option<(Program, LayoutInfo)> {
    let LayoutEnv {
        relayout,
        gap,
        budget,
    } = env;
    let mut setup = None;
    let mut handles_fullscreen = false;
    let mut actions = HashMap::new();
    let mut keybinds = Vec::new();
    let mut insert = None;

    let arrange = if let Some(function) = value.as_function() {
        function.clone()
    } else if let Some(object) = value.as_object() {
        warn_unknown_object(
            object,
            path,
            &[
                "arrange",
                "setup",
                "handles_fullscreen",
                "actions",
                "keybinds",
                "insert",
            ],
            errors,
        );

        let arrange = match function_field(ctx, object, "arrange", path, errors) {
            Ok(Some(function)) => function,
            Ok(None) => {
                errors.error(path, "a layout definition needs an `arrange` function");
                return None;
            }
            Err(()) => return None,
        };
        setup = function_field(ctx, object, "setup", path, errors)
            .ok()
            .flatten();
        insert = function_field(ctx, object, "insert", path, errors)
            .ok()
            .flatten();
        handles_fullscreen = boolean_field(ctx, object, "handles_fullscreen", path, errors)
            .ok()
            .flatten()
            .unwrap_or_default();
        actions = parse_actions(ctx, object, path, errors);
        keybinds = parse_layout_keybinds(ctx, object, path, &actions, errors);
        arrange
    } else {
        errors.error(
            path,
            format!(
                "expected a function or a layout definition object, got {}",
                value.type_name()
            ),
        );
        return None;
    };

    let state = match Object::new(ctx.clone()) {
        Ok(state) => state,
        Err(err) => {
            errors.error(path, format!("cannot create the layout state: {err}"));
            return None;
        }
    };

    if let Some(setup) = setup {
        match build_handle(ctx, gap, relayout, &state) {
            Ok(handle) => {
                budget.arm(ARRANGE_BUDGET);
                let result = setup.call::<_, JsValue>((handle,));
                let expired = budget.expired();
                budget.disarm();
                if let Err(err) = result {
                    let message = if expired {
                        "the layout ran too long (infinite loop?)".to_owned()
                    } else {
                        super::format_error(ctx, &err)
                    };
                    errors.error(path, format!("layout setup failed: {message}"));
                }
            }
            Err(err) => errors.error(path, format!("cannot build the layout handle: {err}")),
        }
    }

    let info = LayoutInfo {
        name: name.to_owned(),
        handles_fullscreen,
        keybinds,
    };
    let program = Program {
        name: name.to_owned(),
        handles_fullscreen,
        arrange: Persistent::save(ctx, arrange),
        actions: actions
            .into_iter()
            .map(|(name, function)| return (name, Persistent::save(ctx, function)))
            .collect(),
        insert: insert.map(|function| return Persistent::save(ctx, function)),
        state: Persistent::save(ctx, state),
    };
    return Some((program, info));
}

/// The object `setup()` receives: the parts of `ctx` that exist before there
/// are any windows.
fn build_handle<'js>(
    ctx: &Ctx<'js>,
    gap: i32,
    relayout: &Rc<Cell<bool>>,
    state: &Object<'js>,
) -> Result<Object<'js>, LayoutError> {
    let handle = new_object(ctx)?;
    set(ctx, &handle, "gap", gap)?;
    set(ctx, &handle, "state", state.clone())?;
    let flag = relayout.clone();
    let function = Function::new(ctx.clone(), move || return flag.set(true))
        .map_err(|err| return context_error(ctx, &err))?;
    set(ctx, &handle, "relayout", function)?;
    return Ok(handle);
}

fn parse_actions<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    path: &str,
    errors: &mut Errors,
) -> HashMap<String, Function<'js>> {
    let mut actions = HashMap::new();
    let Some(value) = get_field(ctx, object, "actions", path, errors) else {
        return actions;
    };
    if value.is_undefined() || value.is_null() {
        return actions;
    }
    let Some(entries) = value.as_object() else {
        errors.error(
            path_key(path, "actions"),
            format!("expected an object, got {}", value.type_name()),
        );
        return actions;
    };
    let actions_path = path_key(path, "actions");
    for entry in entries.props::<String, JsValue>() {
        let (name, value) = match entry {
            Ok(pair) => pair,
            Err(err) => {
                errors.error(
                    &actions_path,
                    format!(
                        "cannot read the actions: {}",
                        super::format_error(ctx, &err)
                    ),
                );
                return actions;
            }
        };
        let entry_path = path_key(&actions_path, &name);
        if name.is_empty() {
            errors.error(entry_path, "action names must not be empty");
            continue;
        }
        let kind = value.type_name();
        match value.into_function() {
            Some(function) => {
                actions.insert(name, function);
            }
            None => errors.error(entry_path, format!("expected a function, got {kind}")),
        }
    }
    return actions;
}

fn parse_layout_keybinds<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    path: &str,
    actions: &HashMap<String, Function<'js>>,
    errors: &mut Errors,
) -> Vec<Keybind> {
    let mut keybinds = Vec::new();
    let Some(value) = get_field(ctx, object, "keybinds", path, errors) else {
        return keybinds;
    };
    if value.is_undefined() || value.is_null() {
        return keybinds;
    }
    let table = match super::builder::convert(ctx, &value) {
        Ok(Value::Table(table)) => table,
        Ok(other) => {
            errors.error(
                path_key(path, "keybinds"),
                format!("expected an object, got {}", other.type_name()),
            );
            return keybinds;
        }
        Err(err) => {
            errors.error(
                path_key(path, "keybinds"),
                format!(
                    "cannot read the keybindings: {}",
                    super::format_error(ctx, &err)
                ),
            );
            return keybinds;
        }
    };

    let keybinds_path = path_key(path, "keybinds");
    for (name, value) in ex::string_entries(&table, &keybinds_path, errors) {
        let bind_path = path_key(&keybinds_path, &name);
        let key = match name.parse::<Key>() {
            Ok(key) => key,
            Err(err) => {
                errors.error(&bind_path, err.to_string());
                continue;
            }
        };
        let Some((action, repeat)) = parse_layout_bind(&value, &bind_path, actions, errors) else {
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

/// A layout keybinding value: an action name, `["name", …args]`, or
/// `{ action: "name", args: [ … ] }`.
fn parse_layout_bind<'js>(
    value: &Value,
    path: &str,
    actions: &HashMap<String, Function<'js>>,
    errors: &mut Errors,
) -> Option<(Action, bool)> {
    let (name, args, repeat) = match value {
        Value::String(_) => {
            let name = action_name(value, path, errors)?;
            (name, Vec::new(), true)
        }
        Value::Table(table) => {
            if let Some(action) = ex::field(table, "action") {
                ex::warn_unknown(
                    table,
                    path,
                    &["action", "args", "key_repeat", "repeat"],
                    errors,
                );
                let name = action_name(&action, &path_key(path, "action"), errors)?;
                let args = ex::string_list(table, "args", path, errors).unwrap_or_default();
                let repeat = ex::boolean(table, "key_repeat", path, errors)
                    .or_else(|| return ex::boolean(table, "repeat", path, errors))
                    .unwrap_or(true);
                (name, args, repeat)
            } else {
                let mut values = Vec::new();
                for (index, item) in table.sequence_values().iter().enumerate() {
                    let item_path = path_index(path, index + 1);
                    let Value::String(text) = item else {
                        errors.error(
                            item_path,
                            format!("expected an action name string, got {}", item.type_name()),
                        );
                        return None;
                    };
                    values.push(text.as_str().to_owned());
                }
                let Some(name) = values.first().cloned() else {
                    errors.error(
                        path,
                        "expected an action, e.g. \"focus-left\" or [\"focus-left\", \"arg\"]",
                    );
                    return None;
                };
                let args = values.into_iter().skip(1).collect();
                (name, args, true)
            }
        }
        other => {
            errors.error(
                path,
                format!("expected an action name or list, got {}", other.type_name()),
            );
            return None;
        }
    };

    if !actions.contains_key(&name) {
        let mut defined = actions.keys().cloned().collect::<Vec<_>>();
        defined.sort();
        let message = if defined.is_empty() {
            format!("the layout defines no action named {name:?}")
        } else {
            format!(
                "the layout defines no action named {name:?} (defined: {})",
                defined.join(", ")
            )
        };
        errors.error(path, message);
        return None;
    }
    return Some((Action::LayoutAction { name, args }, repeat));
}

fn action_name(value: &Value, path: &str, errors: &mut Errors) -> Option<String> {
    return match value {
        Value::String(name) if !name.as_str().is_empty() => Some(name.as_str().to_owned()),
        Value::String(_) => {
            errors.error(path, "action names must not be empty");
            None
        }
        other => {
            errors.error(
                path,
                format!("expected an action name, got {}", other.type_name()),
            );
            None
        }
    };
}

/// The `arrange()` result: an object keyed by window id.
///
/// JavaScript object keys are strings, so the id is parsed from the key. An
/// array is rejected explicitly: its keys are `0..n`, which would silently
/// look like window ids.
fn parse_result<'js>(
    ctx: &Ctx<'js>,
    value: JsValue<'js>,
) -> Result<HashMap<u64, LayoutRect>, LayoutError> {
    if value.is_undefined() || value.is_null() {
        return Err(LayoutError {
            message: format!(
                "arrange() must return an object of geometries, got {}",
                value.type_name()
            ),
        });
    }
    if value.is_array() {
        return Err(LayoutError {
            message: "arrange() must return an object keyed by window id, not an array".to_owned(),
        });
    }
    let Some(object) = value.as_object() else {
        return Err(LayoutError {
            message: format!(
                "arrange() must return an object of geometries, got {}",
                value.type_name()
            ),
        });
    };

    let mut result = HashMap::new();
    for entry in object.props::<String, JsValue>() {
        let (key, value) = match entry {
            Ok(pair) => pair,
            Err(err) => {
                return Err(LayoutError {
                    message: format!(
                        "cannot read the arrange() result: {}",
                        super::format_error(ctx, &err)
                    ),
                });
            }
        };
        let id = match key.parse::<u64>() {
            Ok(id) => id,
            Err(_) => {
                return Err(LayoutError {
                    message: format!("arrange() returned a non-window key {key:?}"),
                });
            }
        };
        let Some(rect) = value.as_object() else {
            return Err(LayoutError {
                message: format!(
                    "arrange() geometry for window {id} is {}, expected an object",
                    value.type_name()
                ),
            });
        };
        let x = number_field(ctx, rect, "x", id)?;
        let y = number_field(ctx, rect, "y", id)?;
        let w = number_field(ctx, rect, "w", id)?;
        let h = number_field(ctx, rect, "h", id)?;
        if w <= 0 || h <= 0 {
            return Err(LayoutError {
                message: format!("arrange() geometry for window {id} has a non-positive size"),
            });
        }
        result.insert(id, LayoutRect { x, y, w, h });
    }
    return Ok(result);
}

/// Parse the object a layout action returned.
///
/// Unknown fields are ignored so a layout can carry its own bookkeeping in the
/// same object. Supported fields are `focus`, `order`, `switch_workspace` and
/// `relayout`.
fn parse_effects<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> Result<LayoutEffects, LayoutError> {
    let mut effects = LayoutEffects::default();
    if value.is_undefined() || value.is_null() {
        return Ok(effects);
    }
    let Some(object) = value.as_object() else {
        return Err(LayoutError {
            message: format!(
                "an action must return an object of effects or nothing, got {}",
                value.type_name()
            ),
        });
    };

    let focus = effect_field(ctx, object, "focus")?;
    if !focus.is_undefined() && !focus.is_null() {
        let kind = focus.type_name();
        let Some(id) = focus.as_number() else {
            return Err(LayoutError {
                message: format!("effect `focus` must be a window id, got {kind}"),
            });
        };
        if id.fract() != 0.0 || id < 1.0 {
            return Err(LayoutError {
                message: format!("effect `focus` must be a window id, got {kind}"),
            });
        }
        effects.focus = Some(id as u64);
    }

    let order = effect_field(ctx, object, "order")?;
    if !order.is_undefined() && !order.is_null() {
        let kind = order.type_name();
        let Some(array) = order.into_array() else {
            return Err(LayoutError {
                message: format!("effect `order` must be a list of window ids, got {kind}"),
            });
        };
        let mut ids = Vec::with_capacity(array.len());
        for index in 0..array.len() {
            let item = array.get::<JsValue>(index).map_err(|err| {
                return LayoutError {
                    message: format!(
                        "cannot read the action effects: {}",
                        super::format_error(ctx, &err)
                    ),
                };
            })?;
            let kind = item.type_name();
            match item.as_number() {
                Some(id) if id.fract() == 0.0 && id >= 1.0 => ids.push(id as u64),
                _ => {
                    return Err(LayoutError {
                        message: format!("effect `order` must contain window ids, got {kind}"),
                    });
                }
            }
        }
        effects.order = Some(ids);
    }

    let workspace = effect_field(ctx, object, "switch_workspace")?;
    if !workspace.is_undefined() && !workspace.is_null() {
        let kind = workspace.type_name();
        let Some(index) = workspace.as_number() else {
            return Err(LayoutError {
                message: format!(
                    "effect `switch_workspace` must be a 1-based workspace number, got {kind}"
                ),
            });
        };
        if index.fract() != 0.0 || index < 1.0 {
            return Err(LayoutError {
                message: format!(
                    "effect `switch_workspace` must be a 1-based workspace number, got {kind}"
                ),
            });
        }
        effects.switch_workspace = Some(index as usize);
    }

    let relayout = effect_field(ctx, object, "relayout")?;
    if !relayout.is_undefined() && !relayout.is_null() {
        let kind = relayout.type_name();
        let Some(value) = relayout.as_bool() else {
            return Err(LayoutError {
                message: format!("effect `relayout` must be a boolean, got {kind}"),
            });
        };
        effects.relayout = value;
    }

    return Ok(effects);
}

fn effect_field<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
) -> Result<JsValue<'js>, LayoutError> {
    return object.get::<_, JsValue>(key).map_err(|err| {
        return LayoutError {
            message: format!(
                "cannot read the action effects: {}",
                super::format_error(ctx, &err)
            ),
        };
    });
}

fn number_field<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
    id: u64,
) -> Result<i32, LayoutError> {
    let value = object.get::<_, JsValue>(key).map_err(|err| {
        return LayoutError {
            message: format!(
                "cannot read window {id} field {key}: {}",
                super::format_error(ctx, &err)
            ),
        };
    })?;
    let kind = value.type_name();
    let Some(number) = value.as_number() else {
        return Err(LayoutError {
            message: format!("window {id} field {key} is a {kind}, expected a number"),
        });
    };
    if !number.is_finite() || number < f64::from(i32::MIN) || number > f64::from(i32::MAX) {
        return Err(LayoutError {
            message: format!("window {id} field {key} is out of range"),
        });
    }
    return Ok(number.round() as i32);
}

/// The object describing one window to a layout.
fn build_window<'js>(ctx: &Ctx<'js>, window: &LayoutWindow) -> Result<Object<'js>, LayoutError> {
    let entry = new_object(ctx)?;
    set(ctx, &entry, "id", window.id)?;
    if let Some(app_id) = &window.app_id {
        set(ctx, &entry, "app_id", app_id.as_str())?;
    }
    if let Some(title) = &window.title {
        set(ctx, &entry, "title", title.as_str())?;
    }
    set(ctx, &entry, "focused", window.focused)?;
    set(ctx, &entry, "x", window.rect.x)?;
    set(ctx, &entry, "y", window.rect.y)?;
    set(ctx, &entry, "w", window.rect.w)?;
    set(ctx, &entry, "h", window.rect.h)?;
    set(ctx, &entry, "geometry", rect_object(ctx, &window.rect)?)?;
    let min_size = new_object(ctx)?;
    set(ctx, &min_size, "w", window.min_size.0)?;
    set(ctx, &min_size, "h", window.min_size.1)?;
    set(ctx, &entry, "min_size", min_size)?;
    let max_size = new_object(ctx)?;
    set(ctx, &max_size, "w", window.max_size.0)?;
    set(ctx, &max_size, "h", window.max_size.1)?;
    set(ctx, &entry, "max_size", max_size)?;
    return Ok(entry);
}

fn rect_object<'js>(ctx: &Ctx<'js>, rect: &LayoutRect) -> Result<Object<'js>, LayoutError> {
    let object = new_object(ctx)?;
    set(ctx, &object, "x", rect.x)?;
    set(ctx, &object, "y", rect.y)?;
    set(ctx, &object, "w", rect.w)?;
    set(ctx, &object, "h", rect.h)?;
    return Ok(object);
}

fn new_object<'js>(ctx: &Ctx<'js>) -> Result<Object<'js>, LayoutError> {
    return Object::new(ctx.clone()).map_err(|err| return context_error(ctx, &err));
}

fn new_array<'js>(ctx: &Ctx<'js>) -> Result<Array<'js>, LayoutError> {
    return Array::new(ctx.clone()).map_err(|err| return context_error(ctx, &err));
}

fn set<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
    value: impl IntoJs<'js>,
) -> Result<(), LayoutError> {
    return object
        .set(key, value)
        .map_err(|err| return context_error(ctx, &err));
}

fn context_error(ctx: &Ctx<'_>, err: &rquickjs::Error) -> LayoutError {
    return LayoutError {
        message: format!(
            "cannot build the layout context: {}",
            super::format_error(ctx, err)
        ),
    };
}

fn no_layout(name: &str) -> LayoutError {
    return LayoutError {
        message: format!("no layout named {name:?}"),
    };
}

/// A field that holds a JavaScript function.
///
/// `Ok(None)` means the field is absent; `Err(())` means it was present but not
/// a function, and the problem is already recorded.
fn function_field<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
    path: &str,
    errors: &mut Errors,
) -> Result<Option<Function<'js>>, ()> {
    let Some(value) = get_field(ctx, object, key, path, errors) else {
        return Err(());
    };
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    let kind = value.type_name();
    return match value.into_function() {
        Some(function) => Ok(Some(function)),
        None => {
            errors.error(
                path_key(path, key),
                format!("expected a function, got {kind}"),
            );
            Err(())
        }
    };
}

/// A boolean-valued field.
fn boolean_field<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
    path: &str,
    errors: &mut Errors,
) -> Result<Option<bool>, ()> {
    let Some(value) = get_field(ctx, object, key, path, errors) else {
        return Err(());
    };
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    let kind = value.type_name();
    return match value.as_bool() {
        Some(value) => Ok(Some(value)),
        None => {
            errors.error(
                path_key(path, key),
                format!("expected a boolean, got {kind}"),
            );
            Err(())
        }
    };
}

/// Read one field, reporting a JavaScript exception as a configuration error.
fn get_field<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
    path: &str,
    errors: &mut Errors,
) -> Option<JsValue<'js>> {
    return match object.get::<_, JsValue>(key) {
        Ok(value) => Some(value),
        Err(err) => {
            errors.error(
                path_key(path, key),
                format!("cannot read the value: {}", super::format_error(ctx, &err)),
            );
            None
        }
    };
}

fn warn_unknown_object(object: &Object<'_>, path: &str, known: &[&str], errors: &mut Errors) {
    for key in object.keys::<String>() {
        let Ok(key) = key else {
            continue;
        };
        if !known.contains(&key.as_str()) {
            errors.warn(path_key(path, &key), "unknown setting, it will be ignored");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigLoadResult;
    use crate::js::test_support::{expect_config, layout_context, layout_entry};

    /// A context with two tiled windows, ids 3 and 4.
    fn two_windows() -> LayoutContext {
        let mut context = layout_context();
        context.windows = vec![
            LayoutWindow {
                id: 3,
                app_id: Some("foot".into()),
                title: Some("shell".into()),
                focused: true,
                rect: LayoutRect {
                    x: 10,
                    y: 20,
                    w: 100,
                    h: 200,
                },
                min_size: (0, 0),
                max_size: (0, 0),
            },
            LayoutWindow {
                id: 4,
                app_id: None,
                title: None,
                focused: false,
                rect: LayoutRect {
                    x: 0,
                    y: 0,
                    w: 300,
                    h: 400,
                },
                min_size: (0, 0),
                max_size: (0, 0),
            },
        ];
        context.focused = Some(3);
        return context;
    }

    fn programs(result: &ConfigLoadResult) -> &LayoutPrograms {
        return result
            .layout_programs
            .as_ref()
            .expect("the layout programs should stay alive");
    }

    #[test]
    fn arrange_returns_geometry_keyed_by_window_id() {
        let (config, result) = expect_config(
            "arrange",
            r#"
            config({ general: { gap: 5 } });
            layout("columns", ctx => {
                const out = {};
                let x = ctx.output.work_area.x;
                for (const window of ctx.windows) {
                    out[window.id] = { x: x, y: ctx.output.work_area.y, w: window.w, h: window.h };
                    x += window.w + ctx.gap;
                }
                return out;
            });
            "#,
        );
        assert_eq!(config.general.gap, 5);

        let arranged = programs(&result)
            .arrange("columns", &two_windows())
            .unwrap();
        assert_eq!(arranged.len(), 2);
        assert_eq!(
            arranged.get(&3),
            Some(&LayoutRect {
                x: 0,
                y: 0,
                w: 100,
                h: 200,
            })
        );
        // `ctx.gap` is the configured gap, not a second copy of `general.gap`.
        assert_eq!(
            arranged.get(&4),
            Some(&LayoutRect {
                x: 105,
                y: 0,
                w: 300,
                h: 400,
            })
        );
    }

    #[test]
    fn arrange_rejects_an_array() {
        let (_config, result) = expect_config("arrange-array", r#"layout("bad", ctx => []);"#);
        let err = programs(&result)
            .arrange("bad", &two_windows())
            .unwrap_err();
        assert!(err.message.contains("not an array"), "{err}");
    }

    #[test]
    fn arrange_rejects_non_window_keys_and_sizes() {
        let (_config, result) = expect_config(
            "arrange-keys",
            r#"
            layout("key", ctx => ({ width: { x: 0, y: 0, w: 10, h: 10 } }));
            layout("size", ctx => ({ 3: { x: 0, y: 0, w: 0, h: 10 } }));
            "#,
        );
        let err = programs(&result)
            .arrange("key", &two_windows())
            .unwrap_err();
        assert!(err.message.contains("non-window key"), "{err}");
        let err = programs(&result)
            .arrange("size", &two_windows())
            .unwrap_err();
        assert!(err.message.contains("non-positive size"), "{err}");
    }

    #[test]
    fn arrange_gets_state_options_and_relayout() {
        let (config, result) = expect_config(
            "arrange-state",
            r#"
            output("WL-1", { layout_options: { column_width: 240 } });
            layout("stateful", {
                setup: ctx => {
                    ctx.state.setups = (ctx.state.setups || 0) + 1;
                    ctx.relayout();
                },
                arrange: ctx => {
                    ctx.state.calls = (ctx.state.calls || 0) + 1;
                    ctx.relayout();
                    const width = ctx.options.column_width + ctx.state.calls;
                    return { 3: { x: 0, y: 0, w: width, h: 10 } };
                },
            });
            "#,
        );
        assert_eq!(config.layouts.len(), 1);

        let programs = programs(&result);
        let context = two_windows();
        // `setup()` runs once, at load time, and `relayout()` was called.
        assert!(programs.take_relayout());
        assert!(!programs.take_relayout());

        // `ctx.state` persists between calls; `ctx.options` comes from the
        // matching `output()` entry.
        let first = programs.arrange("stateful", &context).unwrap();
        assert_eq!(first.get(&3).map(|rect| return rect.w), Some(241));
        assert!(programs.take_relayout());
        let second = programs.arrange("stateful", &context).unwrap();
        assert_eq!(second.get(&3).map(|rect| return rect.w), Some(242));

        // A layout without an `insert` hook leaves placement to the manager.
        assert_eq!(
            programs
                .insert("stateful", &context, &layout_entry())
                .unwrap(),
            None
        );
    }

    #[test]
    fn handles_fullscreen_is_reported() {
        let (_config, result) = expect_config(
            "arrange-fullscreen",
            r#"
            layout("covers", { arrange: ctx => ({}), handles_fullscreen: true });
            layout("plain", ctx => ({}));
            "#,
        );
        let programs = programs(&result);
        assert!(programs.handles_fullscreen("covers"));
        assert!(!programs.handles_fullscreen("plain"));
        assert!(programs.has("plain"));
        assert_eq!(programs.names().collect::<Vec<_>>(), ["covers", "plain"]);
    }

    #[test]
    fn a_runaway_layout_is_stopped() {
        let (_config, result) = expect_config(
            "arrange-runaway",
            r#"layout("slow", ctx => { while (true) {} });"#,
        );
        let err = programs(&result)
            .arrange("slow", &two_windows())
            .unwrap_err();
        assert!(err.message.contains("ran too long"), "{err}");
    }

    #[test]
    fn an_unknown_layout_is_an_error() {
        let (_config, result) =
            expect_config("arrange-unknown", r#"layout("known", ctx => ({}));"#);
        let programs = programs(&result);
        let err = programs.arrange("missing", &two_windows()).unwrap_err();
        assert!(err.message.contains("no layout named"), "{err}");
    }
}
