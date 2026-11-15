//! The global helpers a configuration script calls.
//!
//! The scripting surface is a flat set of globals — `config()`, `bind()`,
//! `layout()` and friends — rather than a `poly` object or a returned table.
//! Each call folds its arguments into one engine-independent [`Table`]; the
//! parser then reads that tree exactly as it read a returned table before.
//!
//! Keeping the helpers as plain globals means an included file can call them
//! without importing anything, so a generated file only has to contain the
//! calls (`docs/config.md`, "Locating the file").

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use rquickjs::function::{Opt, Rest};
use rquickjs::{
    Array, Coerced, Ctx, Exception, FromJs, Function, Object, Persistent, Value as JsValue,
};

use crate::js::budget::Budget;
use crate::js::value::{JsString, MAX_VALUE_DEPTH, Table, Value};

/// One `layout(name, definition)` call, kept alive until the configuration is
/// parsed and the definition has been validated.
pub(crate) struct LayoutEntry {
    /// The layout name, as written in the configuration.
    pub name: String,
    /// The function or definition object, held across the rest of the load.
    pub value: Persistent<JsValue<'static>>,
}

/// State shared by every helper and by the loader.
pub(crate) struct Builder {
    /// The configuration object built by the `config()` calls.
    pub root: Table,
    /// Layout definitions in registration order.
    pub layouts: RefCell<Vec<LayoutEntry>>,
    /// Files that were executed, in execution order (may repeat).
    pub includes: RefCell<Vec<PathBuf>>,
    /// Include chain currently executing, for cycle and depth checks. The last
    /// entry is the file whose code is running.
    pub stack: RefCell<Vec<PathBuf>>,
    /// Time budget for the current evaluation.
    pub budget: Arc<Budget>,
}

impl Builder {
    /// An empty builder with a fresh budget.
    pub fn new() -> Self {
        return Self {
            root: Table::new(),
            layouts: RefCell::new(Vec::new()),
            includes: RefCell::new(Vec::new()),
            stack: RefCell::new(Vec::new()),
            budget: Arc::new(Budget::new()),
        };
    }

    /// Every file that was executed, deduplicated, in execution order.
    pub fn includes(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        for path in self.includes.borrow().iter() {
            if seen.insert(path.clone()) {
                files.push(path.clone());
            }
        }
        return files;
    }

    /// The file whose code is currently running, if any.
    pub fn current_file(&self) -> Option<PathBuf> {
        return self.stack.borrow().last().cloned();
    }
}

impl Default for Builder {
    fn default() -> Self {
        return Self::new();
    }
}

/// Install the helper globals on `ctx`.
pub(crate) fn install<'js>(ctx: &Ctx<'js>, builder: &Rc<Builder>) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    {
        let builder = builder.clone();
        globals.set(
            "config",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, value: JsValue<'js>| -> rquickjs::Result<()> {
                    let table = expect_table(&ctx, &value, "config")?;
                    builder.root.merge_over(&table);
                    return Ok(());
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "bind",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      trigger: JsValue<'js>,
                      action: JsValue<'js>,
                      options: Opt<JsValue<'js>>|
                      -> rquickjs::Result<()> {
                    let trigger = expect_string(&ctx, &trigger, "bind")?;
                    let action = convert(&ctx, &action)?;
                    let value = match options.0 {
                        None => action,
                        Some(options) => {
                            let options = expect_table(&ctx, &options, "bind")?;
                            // An action object that already names its action
                            // (`{ action: "close-window" }`) carries the
                            // options itself; nesting another object would
                            // produce `{ action: { action: … } }`, which the
                            // parser cannot read.
                            let wrapper = match action {
                                Value::Table(table) if table.get("action").is_some() => table,
                                other => {
                                    let wrapper = Table::new();
                                    wrapper.set("action", other);
                                    wrapper
                                }
                            };
                            for (key, value) in options.entries() {
                                let Value::String(key) = key else {
                                    return Err(fail(
                                        &ctx,
                                        "bind() options must be a plain object",
                                    ));
                                };
                                wrapper.set(key.as_str(), value);
                            }
                            Value::Table(wrapper)
                        }
                    };
                    let keybinds = root_table(&builder.root, "keybinds");
                    keybinds.set(trigger, value);
                    return Ok(());
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "output",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      pattern: JsValue<'js>,
                      value: JsValue<'js>|
                      -> rquickjs::Result<()> {
                    let pattern = expect_string(&ctx, &pattern, "output")?;
                    let value = expect_table(&ctx, &value, "output")?;
                    let outputs = root_table(&builder.root, "outputs");
                    outputs.set(pattern, Value::Table(value));
                    return Ok(());
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "window_rule",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, value: JsValue<'js>| -> rquickjs::Result<()> {
                    let value = expect_table(&ctx, &value, "window_rule")?;
                    append(
                        &root_table(&builder.root, "window_rules"),
                        Value::Table(value),
                    );
                    return Ok(());
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "layer_rule",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, value: JsValue<'js>| -> rquickjs::Result<()> {
                    let value = expect_table(&ctx, &value, "layer_rule")?;
                    append(
                        &root_table(&builder.root, "layer_rules"),
                        Value::Table(value),
                    );
                    return Ok(());
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "env",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      name: JsValue<'js>,
                      value: JsValue<'js>|
                      -> rquickjs::Result<()> {
                    let name = expect_string(&ctx, &name, "env")?;
                    let text = match &value {
                        value if value.is_string() => expect_string(&ctx, value, "env")?,
                        value if value.is_bool() => value.as_bool().unwrap_or(false).to_string(),
                        value if value.is_number() => value.as_number().unwrap_or(0.0).to_string(),
                        _ => {
                            return Err(fail(
                                &ctx,
                                format!(
                                    "env({name:?}, …) expects a string, got {}",
                                    value.type_name()
                                ),
                            ));
                        }
                    };
                    let environment = root_table(&builder.root, "environment");
                    environment.set(name, Value::String(JsString::from(text)));
                    return Ok(());
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "startup",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, value: JsValue<'js>| -> rquickjs::Result<()> {
                    let value = match convert(&ctx, &value)? {
                        value @ Value::String(_) => value,
                        value @ Value::Table(_) => value,
                        other => {
                            return Err(fail(
                                &ctx,
                                format!(
                                    "startup() expects a command string or an object, got {}",
                                    other.type_name()
                                ),
                            ));
                        }
                    };
                    append(&root_table(&builder.root, "spawn_at_startup"), value);
                    return Ok(());
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "layout",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      name: JsValue<'js>,
                      value: JsValue<'js>|
                      -> rquickjs::Result<()> {
                    let name = expect_string(&ctx, &name, "layout")?;
                    if builder
                        .layouts
                        .borrow()
                        .iter()
                        .any(|entry| entry.name == name)
                    {
                        return Err(fail(&ctx, format!("layout {name:?} is already defined")));
                    }
                    let value = Persistent::save(&ctx, value);
                    builder
                        .layouts
                        .borrow_mut()
                        .push(LayoutEntry { name, value });
                    return Ok(());
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "default_layout",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, name: JsValue<'js>| -> rquickjs::Result<()> {
                    let name = expect_string(&ctx, &name, "default_layout")?;
                    builder
                        .root
                        .set("default_layout", Value::String(JsString::from(name)));
                    return Ok(());
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "include",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, path: JsValue<'js>| -> rquickjs::Result<JsValue<'js>> {
                    let path = expect_string(&ctx, &path, "include")?;
                    return super::include_file(&ctx, &builder, &path);
                },
            )?,
        )?;
    }

    {
        let builder = builder.clone();
        globals.set(
            "read_file",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, path: JsValue<'js>| -> rquickjs::Result<String> {
                    let path = expect_string(&ctx, &path, "read_file")?;
                    return super::read_file(&ctx, &builder, &path);
                },
            )?,
        )?;
    }

    globals.set(
        "print",
        Function::new(
            ctx.clone(),
            move |_ctx: Ctx<'js>, Rest(args): Rest<JsValue<'js>>| -> rquickjs::Result<()> {
                let mut parts = Vec::with_capacity(args.len());
                for arg in args {
                    parts.push(match arg.get::<Coerced<String>>() {
                        Ok(Coerced(text)) => text,
                        Err(_) => arg.type_name().to_owned(),
                    });
                }
                tracing::info!(target: "poly::config", "{}", parts.join(" "));
                return Ok(());
            },
        )?,
    )?;

    return Ok(());
}

/// The table stored under `key`, created on first use.
fn root_table(root: &Table, key: &str) -> Table {
    if let Some(Value::Table(table)) = root.get(key) {
        return table;
    }
    let table = Table::new();
    root.set(key, Value::Table(table.clone()));
    return table;
}

/// Append to an array stored in the root object, creating it on first use.
fn append(array: &Table, value: Value) {
    let next = array
        .entries()
        .iter()
        .filter_map(|(key, _)| match key {
            Value::Integer(index) if *index >= 1 => Some(*index),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    array.set_index(next as usize, value);
}

/// Convert a JavaScript value into the engine-independent tree.
pub(crate) fn convert<'js>(ctx: &Ctx<'js>, value: &JsValue<'js>) -> rquickjs::Result<Value> {
    return convert_at(ctx, value, 0);
}

fn convert_at<'js>(ctx: &Ctx<'js>, value: &JsValue<'js>, depth: usize) -> rquickjs::Result<Value> {
    if depth > MAX_VALUE_DEPTH {
        return Err(fail(
            ctx,
            format!("value is nested deeper than {MAX_VALUE_DEPTH} levels"),
        ));
    }
    if value.is_undefined() || value.is_null() {
        return Ok(Value::Nil);
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(Value::Boolean(boolean));
    }
    if let Some(integer) = value.as_int() {
        return Ok(Value::Integer(i64::from(integer)));
    }
    if value.is_float() {
        return Ok(Value::Number(value.as_float().unwrap_or(f64::NAN)));
    }
    if value.is_string() {
        let text = value
            .get::<String>()
            .map_err(|_| fail(ctx, "string is not valid UTF-8"))?;
        return Ok(Value::String(JsString::from(text)));
    }
    if value.is_function() {
        return Ok(Value::Function);
    }
    if value.is_array() {
        let array = Array::from_js(ctx, value.clone())?;
        let table = Table::new_array();
        for index in 0..array.len() {
            let item = array.get::<JsValue<'js>>(index)?;
            if item.is_undefined() {
                // A hole in an array literal means "not set", like an omitted
                // object property.
                continue;
            }
            table.set_index(index + 1, convert_at(ctx, &item, depth + 1)?);
        }
        return Ok(Value::Table(table));
    }
    if value.is_object() {
        let object = Object::from_js(ctx, value.clone())?;
        let table = Table::new();
        for entry in object.props::<String, JsValue<'js>>() {
            let (key, item) = entry?;
            if item.is_undefined() {
                continue;
            }
            table.set(key, convert_at(ctx, &item, depth + 1)?);
        }
        return Ok(Value::Table(table));
    }
    return Ok(Value::Other(value.type_name()));
}

/// A thrown JavaScript error carrying `message`.
pub(crate) fn fail(ctx: &Ctx<'_>, message: impl AsRef<str>) -> rquickjs::Error {
    return match Exception::from_message(ctx.clone(), message.as_ref()) {
        Ok(exception) => exception.throw(),
        Err(err) => err,
    };
}

fn expect_table<'js>(
    ctx: &Ctx<'js>,
    value: &JsValue<'js>,
    helper: &str,
) -> rquickjs::Result<Table> {
    return match convert(ctx, value)? {
        Value::Table(table) => Ok(table),
        other => Err(fail(
            ctx,
            format!("{helper}() expects an object, got {}", other.type_name()),
        )),
    };
}

fn expect_string<'js>(
    ctx: &Ctx<'js>,
    value: &JsValue<'js>,
    helper: &str,
) -> rquickjs::Result<String> {
    if !value.is_string() {
        return Err(fail(
            ctx,
            format!("{helper}() expects a string, got {}", value.type_name()),
        ));
    }
    return value
        .get::<String>()
        .map_err(|_| fail(ctx, "string is not valid UTF-8"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::{Context, Runtime};

    fn with_ctx(test: impl FnOnce(&Ctx<'_>, &Rc<Builder>)) {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let builder = Rc::new(Builder::new());
        context.with(|ctx| {
            install(&ctx, &builder).unwrap();
            test(&ctx, &builder);
            // Layout handles are `Persistent` values: they hold a reference to
            // the context and have to be released before it is dropped.
            builder.layouts.borrow_mut().clear();
        });
        drop(builder);
        drop(context);
        drop(runtime);
    }

    #[test]
    fn config_merges_objects_and_replaces_arrays() {
        with_ctx(|ctx, builder| {
            let _: () = ctx
                .eval(
                    r##"
                    config({ general: { gap: 8, border: { width: 2 } } });
                    config({ general: { border: { active: "#ffffff" } } });
                    "##,
                )
                .unwrap();
            let general = builder.root.get("general").unwrap();
            let general = general.as_table().unwrap();
            assert_eq!(general.get("gap"), Some(Value::from(8)));
            let border = general.get("border").unwrap();
            let border = border.as_table().unwrap();
            assert_eq!(border.get("width"), Some(Value::from(2)));
            assert_eq!(border.get("active").unwrap(), Value::from("#ffffff"));
        });
    }

    #[test]
    fn bind_wraps_binding_options() {
        with_ctx(|ctx, builder| {
            let _: () = ctx
                .eval(
                    r#"
                    bind("Mod+Q", "close-window");
                    bind("Mod+W", { spawn: ["foot"] }, { repeat: false });
                    "#,
                )
                .unwrap();
            let keybinds = builder.root.get("keybinds").unwrap();
            let keybinds = keybinds.as_table().unwrap();
            assert_eq!(keybinds.get("Mod+Q").unwrap(), Value::from("close-window"));
            let wrapped = keybinds.get("Mod+W").unwrap();
            let wrapped = wrapped.as_table().unwrap();
            assert_eq!(wrapped.get("repeat"), Some(Value::from(false)));
            assert!(wrapped.get("action").is_some());
        });
    }

    #[test]
    fn rules_and_outputs_accumulate() {
        with_ctx(|ctx, builder| {
            let _: () = ctx
                .eval(
                    r#"
                    window_rule({ match: { app_id: "foot" } });
                    window_rule({ match: { app_id: "mpv" } });
                    output("eDP-1", { scale: 2 });
                    env("MOZ_ENABLE_WAYLAND", 1);
                    startup("waybar");
                    "#,
                )
                .unwrap();
            let rules = builder.root.get("window_rules").unwrap();
            assert_eq!(rules.as_table().unwrap().sequence_values().len(), 2);
            let outputs = builder.root.get("outputs").unwrap();
            assert!(outputs.as_table().unwrap().get("eDP-1").is_some());
            let environment = builder.root.get("environment").unwrap();
            assert_eq!(
                environment
                    .as_table()
                    .unwrap()
                    .get("MOZ_ENABLE_WAYLAND")
                    .unwrap(),
                Value::from("1")
            );
            let startup = builder.root.get("spawn_at_startup").unwrap();
            assert_eq!(startup.as_table().unwrap().sequence_values().len(), 1);
        });
    }

    #[test]
    fn layouts_are_kept_for_the_parser() {
        with_ctx(|ctx, builder| {
            let _: () = ctx
                .eval(
                    r#"
                    layout("columns", { arrange: ctx => ({}) });
                    "#,
                )
                .unwrap();
            assert_eq!(builder.layouts.borrow().len(), 1);
            assert_eq!(builder.layouts.borrow()[0].name, "columns");
        });
    }

    #[test]
    fn helpers_reject_the_wrong_type() {
        with_ctx(|ctx, _builder| {
            let error = ctx.eval::<(), _>("config(1)").unwrap_err();
            assert!(error.is_exception());
        });
    }

    #[test]
    fn undefined_properties_are_skipped() {
        with_ctx(|ctx, builder| {
            let _: () = ctx
                .eval(r#"config({ general: { gap: undefined, border: { width: 3 } } })"#)
                .unwrap();
            let general = builder.root.get("general").unwrap();
            let general = general.as_table().unwrap();
            assert!(general.get("gap").is_none());
        });
    }
}
