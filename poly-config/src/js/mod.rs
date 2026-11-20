//! The JavaScript configuration loader.
//!
//! A configuration is a plain script that calls flat global helpers:
//!
//! ```js
//! config({ general: { gap: 8 } });
//! bind("Mod+Return", { spawn: ["foot"] });
//! ```
//!
//! `include("bindings.js")` evaluates another file — resolved relative to the
//! file that called it — and returns its completion value. Included files are
//! tracked so the reload watcher knows what to watch, which is also how a
//! program that generates a palette keeps working: it writes its own file, and
//! the configuration `include()`s it. All problems are collected with field
//! paths, and a runaway script is stopped by a wall-clock budget.
//!
//! The QuickJS runtime lives on the thread that loads the configuration and is
//! never sent anywhere (`AGENTS.md` §2.5).

pub(crate) mod budget;
pub(crate) mod builder;
pub(crate) mod ext;
pub(crate) mod layout;
pub(crate) mod parse;
pub(crate) mod path;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod value;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use rquickjs::context::EvalOptions;
use rquickjs::runtime::InterruptHandler;
use rquickjs::{Context, Ctx, Exception, Runtime, Value as JsValue};

use crate::error::{ConfigError, ConfigMessage, Errors};
use crate::locate::ConfigSource;

use self::builder::{Builder, fail};

/// Maximum nesting of `include()`.
const MAX_INCLUDE_DEPTH: usize = 32;

/// How long the whole configuration load (root and its includes) may run.
const LOAD_BUDGET: Duration = Duration::from_secs(1);

/// Heap QuickJS may use for one load.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// C stack QuickJS may use, which bounds JavaScript recursion.
const STACK_LIMIT: usize = 1024 * 1024;

/// The outcome of a configuration load.
#[derive(Debug)]
pub struct ConfigLoadResult {
    /// The parsed configuration, or every problem found while loading it.
    pub config: Result<crate::config::Config, ConfigError>,
    /// Every file the load touched, for the reload watcher.
    ///
    /// This is what the watcher stats: the root file and everything
    /// `include()` pulled in, in the order they were read.
    pub includes: Vec<PathBuf>,
    /// Non-fatal problems: unknown settings, a missing configuration file.
    pub warnings: Vec<ConfigMessage>,
    /// JavaScript layouts kept alive for the compositor. `None` means the
    /// built-in fallback layout is the only one.
    pub layout_programs: Option<layout::LayoutPrograms>,
    /// The file's `errors.dbus`, even when the file did not load.
    ///
    /// A configuration with a typo is exactly when a desktop notification
    /// helps, so this is read best-effort from the parsed root object before
    /// the configuration is thrown away.
    pub error_notifications: bool,
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
                config: Ok(crate::config::Config::default()),
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
                    config: Ok(crate::config::Config::default()),
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
    let builder = Rc::new(Builder::new());

    let loaded = load_inner(path, &builder, &mut errors);

    // Read `errors.dbus` before the configuration is thrown away: a file that
    // fails to load is the case where a desktop notification is most wanted.
    // A script that threw never produced a `Config`, so the raw root object is
    // the best source left.
    let error_notifications = match &loaded {
        Ok(loaded) => loaded.config.errors.dbus,
        Err(()) => parse::error_notifications(&builder.root),
    };

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
        includes: builder.includes(),
        warnings,
        layout_programs,
        error_notifications,
    };
}

/// What a successful load produces: the parsed configuration and the QuickJS
/// state behind its layouts.
struct Loaded {
    config: crate::config::Config,
    layouts: Option<layout::LayoutPrograms>,
}

/// Load and parse, pushing problems into `errors`; `Err(())` means the load
/// could not get far enough to produce a configuration at all.
fn load_inner(path: &Path, builder: &Rc<Builder>, errors: &mut Errors) -> Result<Loaded, ()> {
    let canonical = std::fs::canonicalize(path).map_err(|err| {
        errors.file(format!("cannot read {}: {err}", path.display()));
    })?;

    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            errors.file(format!("cannot create the script runtime: {err}"));
            return Err(());
        }
    };
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let handler: InterruptHandler = Box::new(builder.budget.handler());
    runtime.set_interrupt_handler(Some(handler));

    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(err) => {
            errors.file(format!("cannot create the script context: {err}"));
            return Err(());
        }
    };

    builder.budget.arm(LOAD_BUDGET);
    let outcome = context.with(|ctx| -> Result<(), String> {
        builder::install(&ctx, builder).map_err(|err| return format_error(&ctx, &err))?;
        let value = run_root(&ctx, builder, &canonical)
            .map_err(|err| return eval_error(&ctx, builder, &err))?;
        if value.is_object() && !value.is_function() {
            return Err(
                "the configuration must call config({ … }); returning an object is not supported"
                    .to_owned(),
            );
        }
        return Ok(());
    });
    builder.budget.disarm();

    // Persistent handles must be dropped before the runtime; taking them here
    // also guarantees a failed load does not leave them in the builder.
    let entries = std::mem::take(&mut *builder.layouts.borrow_mut());

    if let Err(message) = outcome {
        errors.file(message);
        return Err(());
    }

    let mut config = parse::parse_config(&builder.root, &canonical, errors);
    let layouts = layout::LayoutPrograms::build(
        context,
        entries,
        builder.budget.clone(),
        &mut config,
        errors,
    );
    // The runtime is kept alive by the context, which `build` moved into the
    // programs when there are layouts; otherwise both are dropped here.
    drop(runtime);
    return Ok(Loaded { config, layouts });
}

/// Execute the root file, tracking it on the include stack.
fn run_root<'js>(
    ctx: &Ctx<'js>,
    builder: &Rc<Builder>,
    canonical: &Path,
) -> rquickjs::Result<JsValue<'js>> {
    builder.stack.borrow_mut().push(canonical.to_owned());
    let result = run_file(ctx, builder, canonical);
    builder.stack.borrow_mut().pop();
    return result;
}

/// Read and execute one file.
fn run_file<'js>(
    ctx: &Ctx<'js>,
    builder: &Rc<Builder>,
    path: &Path,
) -> rquickjs::Result<JsValue<'js>> {
    let source = std::fs::read_to_string(path).map_err(|err| {
        return fail(ctx, format!("cannot read {}: {err}", path.display()));
    })?;
    builder.includes.borrow_mut().push(path.to_owned());

    let mut options = EvalOptions::default();
    options.global = true;
    options.strict = true;
    options.backtrace_barrier = true;
    options.promise = false;
    options.filename = Some(path.display().to_string());
    return ctx.eval_with_options::<JsValue, _>(source, options);
}

/// `include(path)`: run another file and return its completion value.
fn include_file<'js>(
    ctx: &Ctx<'js>,
    builder: &Rc<Builder>,
    requested: &str,
) -> rquickjs::Result<JsValue<'js>> {
    let from = builder.current_file();
    let resolved = path::resolve_path(from.as_deref(), requested);
    let canonical = std::fs::canonicalize(&resolved).map_err(|err| {
        return fail(
            ctx,
            format!("cannot read included file {}: {err}", resolved.display()),
        );
    })?;

    {
        let stack = builder.stack.borrow();
        if stack.len() >= MAX_INCLUDE_DEPTH {
            return Err(fail(
                ctx,
                format!(
                    "too many nested includes (more than {MAX_INCLUDE_DEPTH}): {}",
                    chain_text(&stack, &canonical)
                ),
            ));
        }
        if stack.contains(&canonical) {
            return Err(fail(
                ctx,
                format!("include cycle: {}", chain_text(&stack, &canonical)),
            ));
        }
    }

    builder.stack.borrow_mut().push(canonical.clone());
    let result = run_file(ctx, builder, &canonical);
    builder.stack.borrow_mut().pop();
    return result;
}

/// `read_file(path)`: the contents of a data file.
///
/// Relative paths resolve like `include()`, but data files are not tracked as
/// dependencies.
fn read_file(ctx: &Ctx<'_>, builder: &Rc<Builder>, requested: &str) -> rquickjs::Result<String> {
    let from = builder.current_file();
    let resolved = path::resolve_path(from.as_deref(), requested);
    return std::fs::read_to_string(&resolved).map_err(|err| {
        return fail(ctx, format!("cannot read {}: {err}", resolved.display()));
    });
}

/// `a.js -> b.js -> c.js`, for include errors.
fn chain_text(stack: &[PathBuf], next: &Path) -> String {
    let mut text = String::new();
    for path in stack {
        text.push_str(&path.display().to_string());
        text.push_str(" -> ");
    }
    text.push_str(&next.display().to_string());
    return text;
}

/// Full error text for a failed evaluation.
///
/// A JavaScript exception carries a message and a stack; both are useful, and
/// the stack names the configuration file and line.
fn format_error(ctx: &Ctx<'_>, err: &rquickjs::Error) -> String {
    if err.is_exception() {
        let value = ctx.catch();
        return exception_text(&value);
    }
    return err.to_string();
}

/// The message (and stack, when there is one) of a thrown JavaScript value.
fn exception_text(value: &JsValue<'_>) -> String {
    if value.is_error() {
        if let Some(object) = value.as_object() {
            let name = object
                .get::<_, Option<String>>("name")
                .ok()
                .flatten()
                .filter(|name| return !name.is_empty() && name != "Error");
            if let Some(exception) = Exception::from_object(object.clone()) {
                let message = exception
                    .message()
                    .unwrap_or_else(|| return "configuration error".to_owned());
                // QuickJS keeps the constructor name out of the message and
                // the stack; `TypeError: …` is what a reader expects.
                let message = match name {
                    Some(name) => format!("{name}: {message}"),
                    None => message,
                };
                if let Some(stack) = exception.stack() {
                    let stack = stack.trim_end();
                    if !stack.is_empty() {
                        return format!("{message}\n{stack}");
                    }
                }
                return message;
            }
        }
    }
    if let Some(text) = value.as_string() {
        if let Ok(text) = text.to_string() {
            return text;
        }
    }
    return format!("configuration error ({} value)", value.type_name());
}

/// The error text of a failed evaluation, with the budget taking precedence.
fn eval_error(ctx: &Ctx<'_>, builder: &Builder, err: &rquickjs::Error) -> String {
    let message = format_error(ctx, err);
    if builder.budget.expired() {
        return "the script ran too long (infinite loop?)".to_owned();
    }
    return message;
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::test_support::{
        TempDir, error_text, expect_config, expect_errors, layout_context, layout_entry, load_text,
        warning_text,
    };
    use super::*;
    use crate::binds::Key;
    use crate::config::{
        AnimationCurve, Color, Cursor, LayerInteractivity, LayerRule, LayoutOptionValue, ModKey,
        OutputMode, OutputPosition, OutputScale, OutputTransform, ScrollMethod, StartupSpawn,
        TiledDrag,
    };
    use poly_ipc::Action;

    #[test]
    fn parses_xwayland_section() {
        let (config, result) = expect_config(
            "xwayland",
            r#"
            config({
                xwayland: {
                    enable: true,
                    satellite_path: "/usr/local/bin/xwayland-satellite",
                    on_demand: false,
                    spawn_at_startup: true,
                },
            });
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
        let (config, _) = expect_config("xwayland-defaults", "config({});");
        assert!(config.xwayland.enable);
        assert_eq!(config.xwayland.satellite_path, "xwayland-satellite");
        assert!(config.xwayland.on_demand);
        assert!(!config.xwayland.spawn_at_startup);
    }

    #[test]
    fn rejects_an_empty_satellite_path() {
        let err = expect_errors(
            "xwayland-path",
            r#"config({ xwayland: { satellite_path: "" } });"#,
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
            config({
                general: {
                    animations: {
                        enable: true,
                        duration: 240,
                        slowdown: 2.0,
                        window_open: { curve: "linear" },
                        window_close: { duration: 80, curve: "ease-out-quad" },
                        window_movement: { duration: 0 },
                    },
                },
            });
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
            config({
                general: {
                    animations: {
                        slowdown: -1,
                        window_open: { curve: "spring" },
                    },
                },
            });
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
    fn parses_visual_settings() {
        let (config, result) = expect_config(
            "visuals",
            r##"
            config({
                general: {
                    border: { width: 2, radius: 12, active: "#ff0000", inactive: "#11223344" },
                    shadow: { enable: true, blur: 24, offset: [0, 6], color: "#000000a0" },
                    blur: { enable: true, passes: 4, offset: 2.5, noise: 0.1,
                            accept_client_regions: false, xray: false,
                            shader: { up: "up.frag", finish: "~/finish.frag" } },
                    opacity: 0.9,
                    window_shader: "glass.frag",
                },
            });
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
        assert!(!blur.xray);
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
            config({
                general: {
                    border: { width: -1, radius: 5000, active: "red" },
                    shadow: { blur: -2, offset: [1], color: "#12" },
                    blur: { passes: 12, offset: -1, noise: 2, xray: 1, shader: { finish: 3 } },
                    opacity: 1.5,
                },
            });
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
            "general.blur.xray",
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
            bind("Mod+3", { focus_workspace: 3 });
            bind("Mod+5", { focus_window: 5 });
            bind("Mod+4", { move_window_to_workspace: 4 });
            bind("Mod+D", { set_layout: "dwindle" });
            bind("Mod+E", { layout_action: "focus-left" });
            bind("Mod+Shift+E", { layout_action: ["resize-column", "0.5"] });
            bind("Mod+M", { minimize_window: true });
            bind("Mod+Shift+M", { restore_window: true });
            bind("Mod+Alt+M", { minimize_window: 7 });
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
        assert_eq!(action("Mod+M"), Action::MinimizeWindow { id: None });
        assert_eq!(action("Mod+Shift+M"), Action::RestoreWindow { id: None });
        assert_eq!(action("Mod+Alt+M"), Action::MinimizeWindow { id: Some(7) });
    }

    #[test]
    fn parses_layout_actions_and_keybinds() {
        let (config, result) = expect_config(
            "layout-actions",
            r#"
            layout("scrolling", {
                arrange: ctx => ({}),
                actions: {
                    "focus-left": ctx => ({ focus: 1 }),
                    "resize-column": (ctx, args) => ({ relayout: true, width: args[0] }),
                    "bad-effects": ctx => ({ focus: "not a window id" }),
                },
                keybinds: {
                    "Mod+H": "focus-left",
                    "Mod+Shift+H": {
                        action: "resize-column",
                        args: ["0.5"],
                        key_repeat: false,
                    },
                },
                insert: (ctx, window) => 1,
            });
            default_layout("scrolling");
            "#,
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(config.layouts.len(), 1);
        let layout = &config.layouts[0];
        assert_eq!(layout.name, "scrolling");
        let bind = |name: &str| -> &crate::Keybind {
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
            layout("scrolling", {
                arrange: ctx => ({}),
                insert: (ctx, window) => 0,
            });
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
            layout("scrolling", {
                arrange: ctx => ({}),
                actions: {},
                keybinds: { "Mod+H": "focus-left" },
            });
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
            config({
                general: { mod_key: "super", prefer_no_csd: true, gap: 12 },
                input: {
                    keyboard: { layout: "us,cn", options: "grp:alt_shift_toggle", repeat_rate: 40 },
                    devices: {
                        "*touchpad*": { tap: true, natural_scroll: true, scroll_method: "two-finger" },
                    },
                },
                window_drag: { tiled: "ignore", threshold: 4, escape_cancels: false, throttle_resize: false },
            });
            bind("Mod+Return", { spawn: ["foot"] });
            bind("Mod+Shift+Q", "close-window");
            bind("Mod+V", { spawn_shell: "foot | cat" });
            bind("Mod+F", "toggle-floating");
            bind("Mod+R", "resize-window", { key_repeat: false });
            startup({ exec: ["waybar"] });
            startup("wl-paste --watch cliphist store");
            env("POLY_TEST", "1");
            layer_rule({ match: { namespace: "waybar" }, keyboard_interactivity: "none" });
            layer_rule({ keyboard_interactivity: "on-demand" });
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
        // Object key order is not the file's order, so look bindings up.
        let bind = |name: &str| -> &crate::Keybind {
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
        assert!(!config.window_drag.throttle_resize);
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
            layer_rule({ keyboard_interactivity: "sometimes" });
            layer_rule({ match: { namespace: 42 } });
            layer_rule({ match: { namespac: "waybar" }, keyboard_interactivity: "none" });
            layer_rule({});
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
        let warnings = warning_text(&result).join("\n");
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
            config({ general: { gap: -1, mod_key: "hyper" }, unknown_setting: true });
            bind("Mod+Frobnicate", "close-window");
            bind("Mod+Q", "frobnicate");
            bind("Mod+W", { spawn: [] });
            bind("Mod+E", 42);
            bind("Mod+M", { minimize_window: false });
            bind("Mod+N", { restore_window: 0 });
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
        assert!(
            text.contains(r#"keybinds["Mod+M"].minimize_window: expected true or a window id"#),
            "{text}"
        );
        assert!(
            text.contains(r#"keybinds["Mod+N"].restore_window: window ids start at 1"#),
            "{text}"
        );
    }

    #[test]
    fn unknown_settings_are_warnings() {
        let (_, result) = expect_config(
            "warnings",
            r#"config({ unknown_setting: true, general: { color: "red" } });"#,
        );
        let text = warning_text(&result);
        assert_eq!(text.len(), 2, "{text:?}");
        assert!(text[0].starts_with("unknown_setting: "), "{text:?}");
        assert!(text[1].starts_with("general.color: "), "{text:?}");
    }

    #[test]
    fn script_errors_carry_a_stack() {
        let err = expect_errors(
            "script-error",
            "const x = null;\nconfig({ general: { gap: x.field } });",
        );
        let text = error_text(&err);
        assert!(text.contains("config.js:2"), "{text}");
        assert!(text.contains("TypeError"), "{text}");
        assert!(text.contains("at "), "{text}");
    }

    #[test]
    fn a_returned_object_is_rejected() {
        // The old Lua form returned the configuration table; the JavaScript API is
        // a set of helper calls, and silently accepting a returned object
        // would hide a configuration that does nothing.
        let err = expect_errors("returned-object", "({ general: { gap: 4 } });");
        let text = error_text(&err);
        assert!(text.contains("must call config("), "{text}");
    }

    #[test]
    fn a_lua_file_is_parsed_as_javascript() {
        // The hard switch: there is no Lua engine to fall back to, and a file
        // written for the old syntax must fail loudly rather than load half of
        // its settings.
        let dir = TempDir::new("lua-file");
        let path = dir.write("config.lua", "return { general = { gap = 4 } }");
        let result = load_file(&path);
        let err = match result.config {
            Ok(_) => panic!("Lua syntax was accepted"),
            Err(err) => err,
        };
        assert!(
            error_text(&err).contains("config.lua"),
            "{}",
            error_text(&err)
        );
    }

    #[test]
    fn includes_resolve_relative_and_are_tracked() {
        let dir = TempDir::new("include");
        let root = dir.write(
            "config.js",
            r#"
            const bindings = include("bindings.js");
            config({ general: { gap: bindings.gap } });
            "#,
        );
        let included = dir.write("bindings.js", "({ gap: 21 });");

        let result = load_file(&root);
        let config = match &result.config {
            Ok(config) => config.clone(),
            Err(err) => panic!("config failed to load: {err}"),
        };
        assert_eq!(config.general.gap, 21);
        assert_eq!(
            result.includes,
            vec![
                fs::canonicalize(&root).unwrap(),
                fs::canonicalize(&included).unwrap(),
            ]
        );
    }

    #[test]
    fn include_cycles_are_reported_with_the_chain() {
        let dir = TempDir::new("cycle");
        let root = dir.write("config.js", r#"include("b.js");"#);
        dir.write("b.js", r#"include("config.js");"#);

        let err = match load_file(&root).config {
            Ok(_) => panic!("cycle went unnoticed"),
            Err(err) => err,
        };
        let text = error_text(&err);
        assert!(text.contains("include cycle"), "{text}");
        assert!(text.contains("config.js -> "), "{text}");
    }

    #[test]
    fn missing_include_is_an_error() {
        let err = expect_errors("missing", r#"include("nope.js");"#);
        let text = error_text(&err);
        assert!(text.contains("cannot read included file"), "{text}");
        assert!(text.contains("nope.js"), "{text}");
    }

    #[test]
    fn read_file_returns_contents() {
        let dir = TempDir::new("read-file");
        let root = dir.write(
            "config.js",
            r#"
            const data = read_file("words.txt");
            config({ general: { gap: Number(data) } });
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
    fn a_missing_data_file_is_an_error() {
        let err = expect_errors(
            "read-file-missing",
            r#"config({ general: { gap: Number(read_file("nope.txt")) } });"#,
        );
        let text = error_text(&err);
        assert!(text.contains("cannot read"), "{text}");
        assert!(text.contains("nope.txt"), "{text}");
    }

    #[test]
    fn runaway_configs_are_stopped() {
        let result = load_text("runaway", "while (true) {}");
        let err = match result.config {
            Ok(_) => panic!("runaway config was allowed"),
            Err(err) => err,
        };
        let text = error_text(&err);
        assert!(text.contains("ran too long"), "{text}");
    }

    #[test]
    fn include_errors_carry_the_config_position() {
        let dir = TempDir::new("include-position");
        let root = dir.write(
            "config.js",
            "const bindings = include(\"bindings.js\");\nconfig({ general: { gap: bindings.gap } });",
        );
        let result = load_file(&root);
        let err = match result.config {
            Ok(_) => panic!("missing include went unnoticed"),
            Err(err) => err,
        };
        let text = error_text(&err);
        assert!(text.contains("cannot read included file"), "{text}");
        // The position is the `include` call in the root file (line 1).
        assert!(text.contains("config.js:1"), "{text}");
    }

    #[test]
    fn parses_output_display_settings() {
        let (config, result) = expect_config(
            "outputs",
            r##"
            output("eDP-1", {
                mode: "2560x1600@60",
                scale: 1.5,
                transform: "90",
                position: { x: 0, y: 0 },
                vrr: true,
                focus: true,
                background: "#101010",
                layout_options: { master_ratio: 0.7 },
            });
            output("DP-*", { mode: "preferred", scale: "auto", position: "auto" });
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

        // Exact names win over globs.
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
            output("*", { scale: 2.0 });
            output("DP-?", { scale: 1.5 });
            output("DP-1", { scale: 1.0 });
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
            output("*", {
                scale: 2.0,
                vrr: true,
                background: "#000000",
                layout_options: { master_ratio: 0.5, view: "rows" },
            });
            output("eDP-1", {
                scale: 1.0,
                background: "#101010",
                layout_options: { master_ratio: 0.7 },
            });
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
            r##"
            output("eDP-1", {
                mode: "big",
                scale: 9.0,
                transform: "sideways",
                position: { x: 0 },
                background: "rebeccapurple",
            });
            "##,
        );
        let text = error_text(&err);
        for expected in [
            r#"outputs["eDP-1"].mode: invalid mode"#,
            r#"outputs["eDP-1"].scale: invalid scale"#,
            r#"outputs["eDP-1"].transform: invalid transform"#,
            r#"outputs["eDP-1"].position: invalid position"#,
            r#"outputs["eDP-1"].background: invalid color"#,
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
            output("eDP-1", { mode: "1920x1080" });
            output("eDP-2", { mode: "PREFERRED" });
            output("eDP-3", { mode: "1920x1080@59.94" });
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

    #[test]
    fn cursor_theme_and_size() {
        // Unset means "ask the environment", which is `Cursor::theme_name`'s job.
        let (config, _) = expect_config("cursor-default", "config({});");
        assert_eq!(config.general.cursor, Cursor::default());

        let (config, _) = expect_config(
            "cursor-configured",
            r#"
            config({ general: { cursor: { theme: "Adwaita", size: 32 } } });
            "#,
        );
        assert_eq!(config.general.cursor.theme.as_deref(), Some("Adwaita"));
        assert_eq!(config.general.cursor.size, Some(32));
        assert_eq!(config.general.cursor.theme_name(), "Adwaita");
        assert_eq!(config.general.cursor.size_px(), 32);
    }

    #[test]
    fn cursor_rejects_out_of_range_values() {
        let err = expect_errors(
            "cursor-bad-size",
            r#"config({ general: { cursor: { size: 0 } } });"#,
        );
        let text = error_text(&err);
        assert!(
            text.contains("general.cursor.size: value must be between 1 and 256"),
            "unexpected error: {text}"
        );

        let err = expect_errors(
            "cursor-bad-theme",
            r#"config({ general: { cursor: { theme: "  " } } });"#,
        );
        let text = error_text(&err);
        assert!(
            text.contains("general.cursor.theme: theme name must not be empty"),
            "unexpected error: {text}"
        );

        // Unknown keys are warnings, not errors, like everywhere else.
        let (_, result) = expect_config(
            "cursor-unknown-key",
            r#"config({ general: { cursor: { colour: "red" } } });"#,
        );
        assert_eq!(result.warnings.len(), 1);
    }
}
