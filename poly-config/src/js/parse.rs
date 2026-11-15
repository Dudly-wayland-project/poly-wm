//! Turning the collected value tree into [`Config`].
//!
//! The parser is engine-independent: `js::builder` converts every `config()`,
//! `bind()`, `output()` … call into [`Value`]/[`Table`], and this module applies
//! the schema on top. Every problem is collected with a field path instead of
//! failing fast, so one load reports everything that is wrong.

use std::path::Path;

use poly_ipc::Action;

use crate::binds::{Key, Keybind};
use crate::config::{
    AccelProfile, AnimationCurve, AnimationOverride, Animations, Blur, Border, ClickMethod, Color,
    Config, Cursor, Device, Fullscreen, General, Input, Keyboard, LayerInteractivity, LayerRule,
    LayoutOptionValue, ModKey, OutputConfig, OutputMode, OutputPosition, OutputScale,
    OutputTransform, Overview, ResizePick, ScrollMethod, Shadow, StartupSpawn, TiledDrag,
    WindowDrag, WindowRule, Xwayland,
};
use crate::error::{Errors, path_index, path_key};
use crate::js::ext as ex;
use crate::js::path::resolve_path;
use crate::js::value::{Table, Value};

/// Every top-level setting the loader understands.
const ROOT_KEYS: &[&str] = &[
    "general",
    "input",
    "keybinds",
    "window_drag",
    "spawn_at_startup",
    "environment",
    "layer_rules",
    "default_layout",
    "window_rules",
    "outputs",
    "fullscreen",
    "overview",
    "errors",
    "xwayland",
];

/// Parse the collected root object into a [`Config`].
pub(crate) fn parse_config(root: &Table, path: &Path, errors: &mut Errors) -> Config {
    let mut config = Config::default();
    ex::warn_unknown(root, "", ROOT_KEYS, errors);

    if let Some(t) = ex::table(root, "general", "", errors) {
        config.general = parse_general(&t, path, errors);
    }
    if let Some(t) = ex::table(root, "input", "", errors) {
        config.input = parse_input(&t, errors);
    }
    if let Some(t) = ex::table(root, "keybinds", "", errors) {
        config.keybinds = parse_keybinds(&t, errors);
    }
    if let Some(t) = ex::table(root, "window_drag", "", errors) {
        config.window_drag = parse_window_drag(&t, errors);
    }
    if let Some(t) = ex::table(root, "spawn_at_startup", "", errors) {
        config.spawn_at_startup = parse_spawn_at_startup(&t, errors);
    }
    if let Some(t) = ex::table(root, "environment", "", errors) {
        config.environment = parse_environment(&t, errors);
    }
    if let Some(t) = ex::table(root, "layer_rules", "", errors) {
        config.layer_rules = parse_layer_rules(&t, errors);
    }
    if let Some(t) = ex::table(root, "window_rules", "", errors) {
        config.window_rules = parse_window_rules(&t, path, errors);
    }
    if let Some(t) = ex::table(root, "outputs", "", errors) {
        config.outputs = parse_outputs(&t, errors);
    }
    if let Some(t) = ex::table(root, "fullscreen", "", errors) {
        config.fullscreen = parse_fullscreen(&t, errors);
    }
    if let Some(t) = ex::table(root, "overview", "", errors) {
        config.overview = parse_overview(&t, errors);
    }
    if let Some(t) = ex::table(root, "errors", "", errors) {
        config.errors = parse_errors(&t, errors);
    }
    if let Some(t) = ex::table(root, "xwayland", "", errors) {
        config.xwayland = parse_xwayland(&t, errors);
    }
    if let Some(name) = ex::string(root, "default_layout", "", errors) {
        config.default_layout = Some(name);
    }
    return config;
}

/// The `errors.dbus` flag, read without reporting anything.
///
/// A configuration with a typo is exactly when a desktop notification helps,
/// so this is read best-effort from whatever the script collected before it
/// failed.
pub(crate) fn error_notifications(root: &Table) -> bool {
    let Some(errors) = root
        .get("errors")
        .and_then(|value| return value.as_table().cloned())
    else {
        return false;
    };
    return match errors.get("dbus") {
        Some(Value::Boolean(value)) => value,
        _ => false,
    };
}

fn parse_fullscreen(table: &Table, errors: &mut Errors) -> Fullscreen {
    let mut fullscreen = Fullscreen::default();
    ex::warn_unknown(table, "fullscreen", &["policy", "cover_top_layer"], errors);
    if let Some(value) = ex::string(table, "policy", "fullscreen", errors) {
        if value != "auto" {
            errors.warn(
                path_key("fullscreen", "policy"),
                format!("unsupported fullscreen policy {value:?}, using auto"),
            );
        }
    }
    if let Some(value) = ex::boolean(table, "cover_top_layer", "fullscreen", errors) {
        fullscreen.cover_top_layer = value;
    }
    return fullscreen;
}

fn parse_general(table: &Table, root: &Path, errors: &mut Errors) -> General {
    let mut general = General::default();
    ex::warn_unknown(
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
            "cursor",
        ],
        errors,
    );

    if let Some(value) = ex::string(table, "mod_key", "general", errors) {
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
    if let Some(value) = ex::boolean(table, "prefer_no_csd", "general", errors) {
        general.prefer_no_csd = value;
    }
    if let Some(value) = ex::integer(table, "gap", "general", 0, 512, errors) {
        general.gap = value as i32;
    }
    if let Some(t) = ex::table(table, "animations", "general", errors) {
        general.animations = parse_animations(&t, errors);
    }
    if let Some(t) = ex::table(table, "border", "general", errors) {
        general.border = parse_border(&t, errors);
    }
    if let Some(t) = ex::table(table, "shadow", "general", errors) {
        general.shadow = parse_shadow(&t, "general.shadow", Shadow::default(), errors);
    }
    if let Some(t) = ex::table(table, "blur", "general", errors) {
        general.blur = parse_blur(&t, root, errors);
    }
    if let Some(value) = ex::number(table, "opacity", "general", errors) {
        if (0.0..=1.0).contains(&value) {
            general.opacity = value;
        } else {
            errors.error(
                path_key("general", "opacity"),
                "value must be between 0.0 and 1.0",
            );
        }
    }
    if let Some(value) = ex::string(table, "window_shader", "general", errors) {
        general.window_shader = Some(resolve_path(Some(root), &value).display().to_string());
    }
    if let Some(t) = ex::table(table, "cursor", "general", errors) {
        general.cursor = parse_cursor(&t, errors);
    }
    return general;
}

/// Parse `general.cursor = { theme: …, size: … }`.
fn parse_cursor(table: &Table, errors: &mut Errors) -> Cursor {
    let mut cursor = Cursor::default();
    ex::warn_unknown(table, "general.cursor", &["theme", "size"], errors);
    if let Some(value) = ex::string(table, "theme", "general.cursor", errors) {
        let value = value.trim();
        if value.is_empty() {
            errors.error(
                path_key("general.cursor", "theme"),
                "theme name must not be empty",
            );
        } else {
            cursor.theme = Some(value.to_owned());
        }
    }
    if let Some(value) = ex::integer(table, "size", "general.cursor", 1, 256, errors) {
        cursor.size = Some(value as u32);
    }
    return cursor;
}

/// Parse `errors = { dbus = false }`.
fn parse_errors(table: &Table, errors: &mut Errors) -> crate::config::Errors {
    let mut result = crate::config::Errors::default();
    ex::warn_unknown(table, "errors", &["dbus"], errors);
    if let Some(value) = ex::boolean(table, "dbus", "errors", errors) {
        result.dbus = value;
    }
    return result;
}

/// Parse `xwayland = { enable, satellite_path, on_demand, spawn_at_startup }`.
fn parse_xwayland(table: &Table, errors: &mut Errors) -> Xwayland {
    let mut xwayland = Xwayland::default();
    ex::warn_unknown(
        table,
        "xwayland",
        &["enable", "satellite_path", "on_demand", "spawn_at_startup"],
        errors,
    );
    if let Some(value) = ex::boolean(table, "enable", "xwayland", errors) {
        xwayland.enable = value;
    }
    if let Some(value) = ex::string(table, "satellite_path", "xwayland", errors) {
        if value.is_empty() {
            errors.error(
                path_key("xwayland", "satellite_path"),
                "must name the xwayland-satellite program or a path to it",
            );
        } else {
            xwayland.satellite_path = value;
        }
    }
    if let Some(value) = ex::boolean(table, "on_demand", "xwayland", errors) {
        xwayland.on_demand = value;
    }
    if let Some(value) = ex::boolean(table, "spawn_at_startup", "xwayland", errors) {
        xwayland.spawn_at_startup = value;
    }
    return xwayland;
}

fn parse_overview(table: &Table, errors: &mut Errors) -> Overview {
    let mut overview = Overview::default();
    ex::warn_unknown(
        table,
        "overview",
        &["zoom", "backdrop_color", "card_color", "shadow"],
        errors,
    );
    if let Some(value) = ex::number(table, "zoom", "overview", errors) {
        if (0.0001..=0.75).contains(&value) {
            overview.zoom = value;
        } else {
            errors.error(
                path_key("overview", "zoom"),
                "value must be between 0.0001 and 0.75 (niri clamps it the same way)",
            );
        }
    }
    if let Some(value) = ex::string(table, "backdrop_color", "overview", errors) {
        match Color::parse(&value) {
            Some(color) => overview.backdrop_color = color,
            None => errors.error(
                path_key("overview", "backdrop_color"),
                format!("invalid color: {value} (expected #rgb, #rrggbb or #rrggbbaa)"),
            ),
        }
    }
    if let Some(value) = ex::string(table, "card_color", "overview", errors) {
        match Color::parse(&value) {
            Some(color) => overview.card_color = color,
            None => errors.error(
                path_key("overview", "card_color"),
                format!("invalid color: {value} (expected #rgb, #rrggbb or #rrggbbaa)"),
            ),
        }
    }
    if let Some(t) = ex::table(table, "shadow", "overview", errors) {
        overview.shadow = parse_shadow(&t, "overview.shadow", overview.shadow, errors);
    }
    return overview;
}

fn parse_border(table: &Table, errors: &mut Errors) -> Border {
    let mut border = Border::default();
    ex::warn_unknown(
        table,
        "general.border",
        &["width", "radius", "active", "inactive"],
        errors,
    );
    if let Some(value) = ex::number(table, "width", "general.border", errors) {
        if (0.0..=256.0).contains(&value) {
            border.width = value;
        } else {
            errors.error(
                path_key("general.border", "width"),
                "value must be between 0 and 256",
            );
        }
    }
    if let Some(value) = ex::number(table, "radius", "general.border", errors) {
        if (0.0..=1024.0).contains(&value) {
            border.radius = value;
        } else {
            errors.error(
                path_key("general.border", "radius"),
                "value must be between 0 and 1024",
            );
        }
    }
    if let Some(value) = ex::string(table, "active", "general.border", errors) {
        match Color::parse(&value) {
            Some(color) => border.active = color,
            None => errors.error(
                path_key("general.border", "active"),
                format!("invalid color: {value} (expected #rgb, #rrggbb or #rrggbbaa)"),
            ),
        }
    }
    if let Some(value) = ex::string(table, "inactive", "general.border", errors) {
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
    ex::warn_unknown(
        table,
        path,
        &["enable", "blur", "spread", "offset", "color"],
        errors,
    );
    if let Some(value) = ex::boolean(table, "enable", path, errors) {
        shadow.enable = value;
    }
    if let Some(value) = ex::number(table, "blur", path, errors) {
        if (0.0..=512.0).contains(&value) {
            shadow.blur = value;
        } else {
            errors.error(path_key(path, "blur"), "value must be between 0 and 512");
        }
    }
    if let Some(value) = ex::number(table, "spread", path, errors) {
        if (-1024.0..=1024.0).contains(&value) {
            shadow.spread = value;
        } else {
            errors.error(
                path_key(path, "spread"),
                "value must be between -1024 and 1024",
            );
        }
    }
    if let Some(offset) = ex::number_list(table, "offset", path, errors) {
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
    if let Some(value) = ex::string(table, "color", path, errors) {
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
    ex::warn_unknown(
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
    if let Some(value) = ex::boolean(table, "enable", "general.blur", errors) {
        blur.enable = value;
    }
    if let Some(value) = ex::integer(table, "passes", "general.blur", 0, 8, errors) {
        blur.passes = value as u32;
    }
    if let Some(value) = ex::number(table, "offset", "general.blur", errors) {
        if (0.0..=64.0).contains(&value) {
            blur.offset = value;
        } else {
            errors.error(
                path_key("general.blur", "offset"),
                "value must be between 0 and 64",
            );
        }
    }
    if let Some(value) = ex::number(table, "noise", "general.blur", errors) {
        if (0.0..=1.0).contains(&value) {
            blur.noise = value;
        } else {
            errors.error(
                path_key("general.blur", "noise"),
                "value must be between 0.0 and 1.0",
            );
        }
    }
    if let Some(value) = ex::boolean(table, "accept_client_regions", "general.blur", errors) {
        blur.accept_client_regions = value;
    }
    if let Some(value) = ex::boolean(table, "layer_surfaces", "general.blur", errors) {
        blur.layer_surfaces = value;
    }
    if let Some(t) = ex::table(table, "shader", "general.blur", errors) {
        let path = "general.blur.shader";
        ex::warn_unknown(&t, path, &["down", "up", "finish"], errors);
        blur.shader.down = ex::string(&t, "down", path, errors)
            .map(|value| return resolve_path(Some(root), &value).display().to_string());
        blur.shader.up = ex::string(&t, "up", path, errors)
            .map(|value| return resolve_path(Some(root), &value).display().to_string());
        blur.shader.finish = ex::string(&t, "finish", path, errors)
            .map(|value| return resolve_path(Some(root), &value).display().to_string());
    }
    return blur;
}

fn parse_animations(table: &Table, errors: &mut Errors) -> Animations {
    return parse_animations_at(table, "general.animations", errors);
}

fn parse_animations_at(table: &Table, base: &str, errors: &mut Errors) -> Animations {
    let mut animations = Animations::default();
    ex::warn_unknown(
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
    if let Some(value) = ex::boolean(table, "enable", base, errors) {
        animations.enable = value;
    }
    if let Some(value) = ex::integer(table, "duration", base, 0, 10_000, errors) {
        animations.duration = value as u16;
    }
    if let Some(value) = ex::number(table, "slowdown", base, errors) {
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
        if let Some(animation) = ex::table(table, key, base, errors) {
            *target = parse_animation_override(&animation, &path_key(base, key), errors);
        }
    }
    return animations;
}

fn parse_animation_override(table: &Table, base: &str, errors: &mut Errors) -> AnimationOverride {
    let mut animation = AnimationOverride::default();
    ex::warn_unknown(table, base, &["duration", "curve"], errors);
    if let Some(value) = ex::integer(table, "duration", base, 0, 10_000, errors) {
        animation.duration = Some(value as u16);
    }
    if let Some(value) = ex::string(table, "curve", base, errors) {
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
    ex::warn_unknown(table, "input", &["keyboard", "devices"], errors);

    if let Some(t) = ex::table(table, "keyboard", "input", errors) {
        input.keyboard = parse_keyboard(&t, errors);
    }
    if let Some(t) = ex::table(table, "devices", "input", errors) {
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
    ex::warn_unknown(table, base, KEYS, errors);

    if let Some(value) = ex::string(table, "rules", base, errors) {
        keyboard.rules = value;
    }
    if let Some(value) = ex::string(table, "model", base, errors) {
        keyboard.model = value;
    }
    if let Some(value) = ex::string(table, "layout", base, errors) {
        keyboard.layout = value;
    }
    if let Some(value) = ex::string(table, "variant", base, errors) {
        keyboard.variant = value;
    }
    if let Some(value) = ex::string(table, "options", base, errors) {
        keyboard.options = Some(value);
    }
    if let Some(value) = ex::integer(table, "repeat_rate", base, 0, 255, errors) {
        keyboard.repeat_rate = value as u8;
    }
    if let Some(value) = ex::integer(table, "repeat_delay", base, 0, 10_000, errors) {
        keyboard.repeat_delay = value as u16;
    }
    return keyboard;
}

fn parse_devices(table: &Table, errors: &mut Errors) -> Vec<(String, Device)> {
    let mut devices = Vec::new();
    for (glob, value) in ex::string_entries(table, "input.devices", errors) {
        let path = path_key("input.devices", &glob);
        let Value::Table(t) = value else {
            errors.error(
                &path,
                format!("expected an object, got {}", value.type_name()),
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
    ex::warn_unknown(table, base, KEYS, errors);

    if let Some(value) = ex::boolean(table, "tap", base, errors) {
        device.tap = Some(value);
    }
    if let Some(value) = ex::boolean(table, "drag", base, errors) {
        device.drag = Some(value);
    }
    if let Some(value) = ex::boolean(table, "drag_lock", base, errors) {
        device.drag_lock = Some(value);
    }
    if let Some(value) = ex::boolean(table, "dwt", base, errors) {
        device.dwt = Some(value);
    }
    // `disable_while_typing` is libinput's name for the same setting.
    if let Some(value) = ex::boolean(table, "disable_while_typing", base, errors) {
        device.dwt = Some(value);
    }
    if let Some(value) = ex::boolean(table, "natural_scroll", base, errors) {
        device.natural_scroll = Some(value);
    }
    if let Some(value) = ex::boolean(table, "left_handed", base, errors) {
        device.left_handed = Some(value);
    }
    if let Some(value) = ex::string(table, "click_method", base, errors) {
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
    if let Some(value) = ex::string(table, "scroll_method", base, errors) {
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
    // `accel` is accepted as an alias for libinput's `accel_profile`;
    // `accel_profile` wins when both are set.
    let accel_profile = match ex::string(table, "accel_profile", base, errors) {
        Some(profile) => Some(profile),
        None => ex::string(table, "accel", base, errors),
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
    if let Some(value) = ex::number(table, "accel_speed", base, errors) {
        if (-1.0..=1.0).contains(&value) {
            device.accel_speed = Some(value);
        } else {
            errors.error(
                path_key(base, "accel_speed"),
                "value must be between -1.0 and 1.0",
            );
        }
    }
    if let Some(value) = ex::number_list(table, "calibration", base, errors) {
        if value.len() == 6 {
            device.calibration = Some(value);
        } else {
            errors.error(
                path_key(base, "calibration"),
                format!("expected 6 numbers (a 2x3 matrix), got {}", value.len()),
            );
        }
    }
    if let Some(value) = ex::string(table, "map_to_output", base, errors) {
        device.map_to_output = Some(value);
    }
    return device;
}

fn parse_keybinds(table: &Table, errors: &mut Errors) -> Vec<Keybind> {
    let mut keybinds = Vec::new();
    for (name, value) in ex::string_entries(table, "keybinds", errors) {
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

/// A binding value: an action name, an action object, or an object carrying the
/// action plus binding options (`repeat`).
fn parse_bind(value: &Value, path: &str, errors: &mut Errors) -> Option<(Action, bool)> {
    return match value {
        Value::Table(table) => {
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
                "minimize_window",
                "restore_window",
                "layout_action",
            ];
            ex::warn_unknown(table, path, KEYS, errors);
            let repeat = ex::boolean(table, "key_repeat", path, errors)
                .or_else(|| return ex::boolean(table, "repeat", path, errors))
                .unwrap_or(true);
            let action = if let Some(value) = ex::field(table, "action") {
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

/// An action value: a bare name (`"close-window"`) or an action object.
fn parse_action_value(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    return match value {
        Value::String(name) => match name.as_str().parse::<Action>() {
            Ok(action) => Some(action),
            Err(err) => {
                let message = match err.hint {
                    Some(hint) => format!("invalid action: {} ({hint})", err.name),
                    None => format!("invalid action: {}", err.name),
                };
                errors.error(path, message);
                None
            }
        },
        Value::Table(table) => parse_inline_action(table, path, errors),
        Value::Function => {
            errors.error(
                path,
                "JavaScript functions as actions are not supported yet",
            );
            None
        }
        other => {
            errors.error(
                path,
                format!(
                    "expected an action (a name string or an object), got {}",
                    other.type_name()
                ),
            );
            None
        }
    };
}

/// The object form of an action: `{ spawn: […] }`, `{ spawn_shell: "…" }`,
/// `{ set_layout: "name" }`, `{ focus_workspace: 2 }`.
fn parse_inline_action(table: &Table, path: &str, errors: &mut Errors) -> Option<Action> {
    const KEYS: &[&str] = &[
        "spawn",
        "spawn_shell",
        "set_layout",
        "focus_workspace",
        "focus_window",
        "move_window_to_workspace",
        "minimize_window",
        "restore_window",
        "layout_action",
    ];

    let mut action = None;
    let mut found_any = false;
    for key in KEYS {
        let Some(value) = ex::field(table, key) else {
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
            "minimize_window" => parse_optional_window_id(&value, &field_path, errors)
                .map(|id| return Action::MinimizeWindow { id }),
            "restore_window" => parse_optional_window_id(&value, &field_path, errors)
                .map(|id| return Action::RestoreWindow { id }),
            "layout_action" => parse_layout_action(&value, &field_path, errors),
            _ => parse_workspace_index(&value, &field_path, errors, false),
        };
    }

    if !found_any {
        errors.error(
            path,
            "expected an action, e.g. \"close-window\" or { spawn: [\"foot\"] }",
        );
    }
    return action;
}

fn parse_spawn(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    let command = ex::string_list_value(value, path, errors)?;
    if command.is_empty() {
        errors.error(path, "the command must not be empty");
        return None;
    }
    return Some(Action::Spawn { command });
}

fn parse_spawn_shell(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    return match value {
        Value::String(line) => Some(Action::SpawnShell {
            command: line.as_str().to_owned(),
        }),
        other => {
            errors.error(
                path,
                format!("expected a shell command string, got {}", other.type_name()),
            );
            None
        }
    };
}

/// `{ layout_action: "focus-left" }` or
/// `{ layout_action: ["resize-column", "0.5"] }`.
fn parse_layout_action(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    match value {
        Value::String(name) => {
            if name.as_str().is_empty() {
                errors.error(path, "layout action names must not be empty");
                return None;
            }
            Some(Action::LayoutAction {
                name: name.as_str().to_owned(),
                args: Vec::new(),
            })
        }
        Value::Table(table) => {
            let mut values = Vec::new();
            for (index, item) in table.sequence_values().iter().enumerate() {
                let item_path = path_index(path, index + 1);
                let Value::String(item) = item else {
                    errors.error(
                        item_path,
                        format!(
                            "expected a layout action name or argument string, got {}",
                            item.type_name()
                        ),
                    );
                    return None;
                };
                values.push(item.as_str().to_owned());
            }
            let Some(name) = values.first().cloned() else {
                errors.error(
                    path,
                    "expected a layout action name, e.g. { layout_action: \"focus-left\" }",
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
                    other.type_name()
                ),
            );
            None
        }
    }
}

fn parse_set_layout(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    return match value {
        Value::String(name) => Some(Action::SetLayout {
            name: name.as_str().to_owned(),
        }),
        other => {
            errors.error(
                path,
                format!("expected a layout name, got {}", other.type_name()),
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
    // workspace is `{ focus_workspace: 1 }`, not 0.
    const MAX_WORKSPACE: i64 = 512;
    let Some(index) = value.as_i64() else {
        errors.error(
            path,
            format!("expected a workspace number, got {}", value.type_name()),
        );
        return None;
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

/// `{ focus_window: 3 }`.
///
/// Window ids only exist while the compositor runs, so this is mostly for
/// scripts; a configuration that binds it targets whatever window happened to
/// get that id.
fn parse_window_id(value: &Value, path: &str, errors: &mut Errors) -> Option<Action> {
    let id = parse_window_id_number(value, path, errors)?;
    return Some(Action::FocusWindow { id });
}

/// A window id from a binding value, shared by every action that takes one.
fn parse_window_id_number(value: &Value, path: &str, errors: &mut Errors) -> Option<u64> {
    let Some(id) = value.as_i64() else {
        errors.error(
            path,
            format!("expected a window id, got {}", value.type_name()),
        );
        return None;
    };
    if id < 1 {
        errors.error(path, "window ids start at 1");
        return None;
    }
    return Some(id as u64);
}

/// `{ minimize_window: true }` or `{ restore_window: 3 }`.
///
/// `true` means the focused window (minimize) or the most recently minimized
/// window of the active workspace (restore); a number names a window, which is
/// only meaningful from a script because ids do not survive a restart.
fn parse_optional_window_id(value: &Value, path: &str, errors: &mut Errors) -> Option<Option<u64>> {
    return match value {
        Value::Boolean(true) => Some(None),
        Value::Boolean(false) => {
            errors.error(path, "expected true or a window id");
            None
        }
        other => Some(Some(parse_window_id_number(other, path, errors)?)),
    };
}

fn parse_window_drag(table: &Table, errors: &mut Errors) -> WindowDrag {
    let mut drag = WindowDrag::default();
    const KEYS: &[&str] = &[
        "tiled",
        "threshold",
        "axis_lock",
        "escape_cancels",
        "resize_pick",
        "throttle_resize",
    ];
    let base = "window_drag";
    ex::warn_unknown(table, base, KEYS, errors);

    if let Some(value) = ex::string(table, "tiled", base, errors) {
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
    if let Some(value) = ex::number(table, "threshold", base, errors) {
        if value >= 0.0 {
            drag.threshold = value;
        } else {
            errors.error(path_key(base, "threshold"), "value must not be negative");
        }
    }
    if let Some(value) = ex::field(table, "axis_lock") {
        match value {
            Value::Boolean(false) => drag.axis_lock = None,
            Value::String(name) => match parse_mod_key(name.as_str()) {
                Some(mod_key) => drag.axis_lock = Some(mod_key),
                None => errors.error(
                    path_key(base, "axis_lock"),
                    "expected a modifier name (ctrl, shift, alt, super) or false",
                ),
            },
            other => errors.error(
                path_key(base, "axis_lock"),
                format!(
                    "expected a modifier name or false, got {}",
                    other.type_name()
                ),
            ),
        }
    }
    if let Some(value) = ex::boolean(table, "escape_cancels", base, errors) {
        drag.escape_cancels = value;
    }
    if let Some(value) = ex::boolean(table, "throttle_resize", base, errors) {
        drag.throttle_resize = value;
    }
    if let Some(value) = ex::field(table, "resize_pick") {
        match value {
            Value::Integer(pixels) if pixels >= 0 => {
                drag.resize_pick = ResizePick::Pixels(pixels as f64);
            }
            Value::Number(pixels) if pixels >= 0.0 => {
                drag.resize_pick = ResizePick::Pixels(pixels);
            }
            Value::String(name) if name.as_str() == "thirds" => {
                drag.resize_pick = ResizePick::Thirds
            }
            other => errors.error(
                path_key(base, "resize_pick"),
                format!(
                    "expected \"thirds\" or a pixel distance, got {}",
                    other.type_name()
                ),
            ),
        }
    }
    return drag;
}

fn parse_spawn_at_startup(table: &Table, errors: &mut Errors) -> Vec<StartupSpawn> {
    let mut spawns = Vec::new();
    for (index, value) in table.sequence_values().iter().enumerate() {
        let path = path_index("spawn_at_startup", index + 1);
        match value {
            // A bare string is a shell command, the common case.
            Value::String(line) => spawns.push(StartupSpawn::Shell(line.as_str().to_owned())),
            Value::Table(t) => {
                if let Some(spawn) = parse_startup_entry(t, &path, errors) {
                    spawns.push(spawn);
                }
            }
            other => errors.error(
                &path,
                format!(
                    "expected a shell command string or an object, got {}",
                    other.type_name()
                ),
            ),
        }
    }
    return spawns;
}

fn parse_startup_entry(table: &Table, path: &str, errors: &mut Errors) -> Option<StartupSpawn> {
    const KEYS: &[&str] = &["exec", "shell", "spawn", "spawn_shell"];
    ex::warn_unknown(table, path, KEYS, errors);

    if let Some(argv) = ex::string_list(table, "exec", path, errors) {
        return check_argv(argv, &path_key(path, "exec"), errors)
            .map(|command| return StartupSpawn::Exec(command));
    }
    if let Some(line) = ex::string(table, "shell", path, errors) {
        return Some(StartupSpawn::Shell(line));
    }
    // The same action spellings as in `keybinds`.
    if let Some(value) = ex::field(table, "spawn") {
        return match parse_spawn(&value, &path_key(path, "spawn"), errors) {
            Some(Action::Spawn { command }) => Some(StartupSpawn::Exec(command)),
            _ => None,
        };
    }
    if let Some(value) = ex::field(table, "spawn_shell") {
        return match parse_spawn_shell(&value, &path_key(path, "spawn_shell"), errors) {
            Some(Action::SpawnShell { command }) => Some(StartupSpawn::Shell(command)),
            _ => None,
        };
    }
    errors.error(path, "expected { exec: […] } or { shell: \"…\" }");
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
    for (index, value) in table.sequence_values().iter().enumerate() {
        let path = path_index("layer_rules", index + 1);
        match value {
            Value::Table(entry) => {
                if let Some(rule) = parse_layer_rule(entry, &path, errors) {
                    rules.push(rule);
                }
            }
            other => errors.error(
                &path,
                format!("expected an object, got {}", other.type_name()),
            ),
        }
    }
    return rules;
}

fn parse_layer_rule(table: &Table, path: &str, errors: &mut Errors) -> Option<LayerRule> {
    const KEYS: &[&str] = &["match", "keyboard_interactivity"];
    ex::warn_unknown(table, path, KEYS, errors);

    let mut rule = LayerRule::default();
    if let Some(t) = ex::table(table, "match", path, errors) {
        let match_path = path_key(path, "match");
        ex::warn_unknown(&t, &match_path, &["namespace"], errors);
        rule.namespace = ex::string(&t, "namespace", &match_path, errors);
    }
    if let Some(value) = ex::string(table, "keyboard_interactivity", path, errors) {
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
    for (name, value) in ex::string_entries(table, "environment", errors) {
        let path = path_key("environment", &name);
        match value {
            Value::String(text) => environment.push((name, text.as_str().to_owned())),
            other => errors.error(
                path,
                format!(
                    "expected a string value, got {} (use String(…) for numbers)",
                    other.type_name()
                ),
            ),
        }
    }
    return environment;
}

/// Parse `window_rules` into the configuration.
fn parse_window_rules(table: &Table, root: &Path, errors: &mut Errors) -> Vec<WindowRule> {
    let mut rules = Vec::new();
    for (index, value) in table.sequence_values().iter().enumerate() {
        let path = path_index("window_rules", index + 1);
        match value {
            Value::Table(entry) => {
                if let Some(rule) = parse_window_rule(entry, &path, root, errors) {
                    rules.push(rule);
                }
            }
            other => errors.error(
                &path,
                format!("expected an object, got {}", other.type_name()),
            ),
        }
    }
    return rules;
}

fn parse_window_rule(
    table: &Table,
    path: &str,
    root: &Path,
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
    ex::warn_unknown(table, path, KEYS, errors);

    let mut rule = WindowRule::default();
    if let Some(matcher) = ex::table(table, "match", path, errors) {
        let match_path = path_key(path, "match");
        ex::warn_unknown(&matcher, &match_path, &["app_id", "title"], errors);
        rule.app_id = ex::string(&matcher, "app_id", &match_path, errors);
        rule.title = ex::string(&matcher, "title", &match_path, errors);
    }
    rule.float = ex::boolean(table, "float", path, errors);
    rule.focus = ex::boolean(table, "focus", path, errors);
    rule.open_on_output = ex::string(table, "open_on_output", path, errors);
    rule.blur = ex::boolean(table, "blur", path, errors);
    rule.radius = ex::number(table, "radius", path, errors);
    rule.shadow = ex::boolean(table, "shadow", path, errors);
    rule.opacity = ex::number(table, "opacity", path, errors);
    rule.no_csd = ex::boolean(table, "no_csd", path, errors);
    if let Some(value) = ex::string(table, "window_shader", path, errors) {
        rule.window_shader = Some(resolve_path(Some(root), &value).display().to_string());
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
fn parse_outputs(table: &Table, errors: &mut Errors) -> Vec<(String, OutputConfig)> {
    let mut outputs = Vec::new();
    for (pattern, value) in ex::string_entries(table, "outputs", errors) {
        let path = path_key("outputs", &pattern);
        let Value::Table(entry) = value else {
            errors.error(
                &path,
                format!("expected an object, got {}", value.type_name()),
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
        ex::warn_unknown(&entry, &path, KEYS, errors);

        let mut output = OutputConfig::default();
        parse_output_display(&entry, &path, &mut output, errors);
        if let Some(options) = ex::table(&entry, "layout_options", &path, errors) {
            let options_path = path_key(&path, "layout_options");
            for (key, value) in ex::string_entries(&options, &options_path, errors) {
                let option = match value {
                    Value::Boolean(value) => LayoutOptionValue::Bool(value),
                    Value::Integer(value) => LayoutOptionValue::Integer(value),
                    Value::Number(value) if value.is_finite() => LayoutOptionValue::Number(value),
                    Value::String(value) => LayoutOptionValue::String(value.as_str().to_owned()),
                    other => {
                        errors.error(
                            path_key(&options_path, &key),
                            format!(
                                "expected a boolean, number or string, got {}",
                                other.type_name()
                            ),
                        );
                        continue;
                    }
                };
                output.layout_options.push((key, option));
            }
        }
        // Objects have no guaranteed order; sort so that a configuration means
        // the same thing on every run.
        output
            .layout_options
            .sort_by(|left, right| return left.0.cmp(&right.0));
        outputs.push((pattern, output));
    }
    // "First match wins" needs an order: exact names before globs, more
    // literal characters before fewer.
    crate::config::sort_output_patterns(&mut outputs);
    return outputs;
}

/// Parse the display settings of one `outputs.<pattern>` object.
fn parse_output_display(entry: &Table, path: &str, output: &mut OutputConfig, errors: &mut Errors) {
    if let Some(value) = ex::string(entry, "mode", path, errors) {
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
    if let Some(value) = ex::field(entry, "scale") {
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
    if let Some(value) = ex::string(entry, "transform", path, errors) {
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
    if let Some(value) = ex::field(entry, "position") {
        match parse_output_position(&value) {
            Some(position) => output.position = Some(position),
            None => errors.error(
                path_key(path, "position"),
                format!(
                    "invalid position {} (expected \"auto\" or an object {{ x: 0, y: 0 }})",
                    describe_value(&value)
                ),
            ),
        }
    }
    if let Some(value) = ex::boolean(entry, "vrr", path, errors) {
        output.vrr = Some(value);
    }
    if let Some(value) = ex::boolean(entry, "focus", path, errors) {
        output.focus = Some(value);
    }
    if let Some(value) = ex::string(entry, "background", path, errors) {
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
            if text.as_str().trim().eq_ignore_ascii_case("auto") {
                return Some(OutputScale::Auto);
            }
            text.as_str().trim().parse::<f64>().ok()?
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

/// Parse `"auto"` or `{ x: 0, y: 0 }` / `[0, 0]`.
fn parse_output_position(value: &Value) -> Option<OutputPosition> {
    let Value::Table(table) = value else {
        if let Value::String(text) = value {
            if text.as_str().trim().eq_ignore_ascii_case("auto") {
                return Some(OutputPosition::Auto);
            }
        }
        return None;
    };
    let x = position_component(table, "x", 1)?;
    let y = position_component(table, "y", 2)?;
    return Some(OutputPosition::Fixed { x, y });
}

/// One coordinate of a position object, by name or by array index.
fn position_component(table: &Table, key: &str, index: usize) -> Option<i32> {
    let value = match ex::field(table, key) {
        Some(value) => value,
        None => table.get_index(index)?,
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
        Value::String(text) => format!("{:?}", text.as_str()),
        Value::Integer(number) => number.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Table(_) => "an object".to_owned(),
        other => other.type_name().to_owned(),
    };
}
