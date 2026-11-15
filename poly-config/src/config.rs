//! The configuration types, filled in from JavaScript by [`crate::js`].
//!
//! Field names and defaults follow `docs/config.md`; the JavaScript layer in
//! [`crate::js::parse`] and [`crate::js::layout`] fills this model and
//! validates it. A configuration written for a newer build still loads after a
//! downgrade, as unknown settings are warnings rather than errors.

use crate::binds::Keybind;

/// The whole configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Miscellaneous settings.
    pub general: General,
    /// Input devices and the keyboard.
    pub input: Input,
    /// Key bindings, in file order (first match wins).
    pub keybinds: Vec<Keybind>,
    /// Interactive move/resize behaviour.
    pub window_drag: WindowDrag,
    /// Commands to run once the compositor is up.
    pub spawn_at_startup: Vec<StartupSpawn>,
    /// Extra environment variables for spawned children.
    pub environment: Vec<(String, String)>,
    /// Rules for layer surfaces (bars, notifications, launchers), first match
    /// wins.
    pub layer_rules: Vec<LayerRule>,
    /// Named scripted layouts, in configuration order.
    pub layouts: Vec<LayoutInfo>,
    /// The layout new outputs start with. Empty means the built-in fallback.
    pub default_layout: Option<String>,
    /// Rules for toplevel windows, first match wins.
    pub window_rules: Vec<WindowRule>,
    /// Per-output overrides, keyed by name or glob (`"eDP-1"`, `"DP-*"`).
    ///
    /// The list is sorted by specificity when it is parsed, so that the first
    /// matching entry is always the most specific one (`docs/config.md`).
    pub outputs: Vec<(String, OutputConfig)>,
    /// Fullscreen behaviour.
    pub fullscreen: Fullscreen,
    /// Appearance of the workspace overview.
    pub overview: Overview,
    /// Where configuration and runtime problems are reported.
    pub errors: Errors,
    /// X11 clients through `xwayland-satellite`.
    pub xwayland: Xwayland,
}

impl Default for Config {
    fn default() -> Self {
        return Self {
            general: General::default(),
            input: Input::default(),
            keybinds: Vec::new(),
            window_drag: WindowDrag::default(),
            spawn_at_startup: Vec::new(),
            environment: Vec::new(),
            layer_rules: Vec::new(),
            layouts: Vec::new(),
            default_layout: None,
            window_rules: Vec::new(),
            outputs: Vec::new(),
            fullscreen: Fullscreen::default(),
            overview: Overview::default(),
            errors: Errors::default(),
            xwayland: Xwayland::default(),
        };
    }
}

impl Config {
    /// The `outputs` settings that apply to the output called `name`.
    ///
    /// Every matching entry is merged, most specific last (see
    /// [`resolved_output`]), so `["*"]` can provide defaults that `["eDP-1"]`
    /// then overrides field by field.
    pub fn output_for(&self, name: &str) -> Option<OutputConfig> {
        return resolved_output(&self.outputs, name);
    }
}

/// Merge every `outputs` entry whose pattern matches `name` into one config.
///
/// `outputs` is sorted by specificity when it is parsed
/// ([`sort_output_patterns`]), so the more specific entry wins field by field:
/// a `["*"]` entry provides the defaults and `["eDP-1"]` overrides what it
/// cares about. Fields nobody sets stay `None`, which means "leave it to the
/// backend".
pub fn resolved_output(outputs: &[(String, OutputConfig)], name: &str) -> Option<OutputConfig> {
    let mut resolved: Option<OutputConfig> = None;
    // Most specific first, so walking backwards applies the least specific
    // defaults first and lets the specific entry overwrite them.
    for (pattern, config) in outputs.iter().rev() {
        if !crate::glob::glob_matches(pattern, name) {
            continue;
        }
        merge_output(resolved.get_or_insert_with(OutputConfig::default), config);
    }
    return resolved;
}

/// Overlay `source` onto `target`: every field `source` sets wins, and
/// `layout_options` are merged key by key.
fn merge_output(target: &mut OutputConfig, source: &OutputConfig) {
    if source.mode.is_some() {
        target.mode = source.mode;
    }
    if source.scale.is_some() {
        target.scale = source.scale;
    }
    if source.transform.is_some() {
        target.transform = source.transform;
    }
    if source.position.is_some() {
        target.position = source.position;
    }
    if source.vrr.is_some() {
        target.vrr = source.vrr;
    }
    if source.focus.is_some() {
        target.focus = source.focus;
    }
    if source.background.is_some() {
        target.background = source.background;
    }
    for (key, value) in &source.layout_options {
        match target
            .layout_options
            .iter_mut()
            .find(|(existing, _)| return existing == key)
        {
            Some(entry) => entry.1 = value.clone(),
            None => target.layout_options.push((key.clone(), value.clone())),
        }
    }
    // Keep the merged result ordered the way parsing leaves it.
    target
        .layout_options
        .sort_by(|left, right| return left.0.cmp(&right.0));
}

/// Order `outputs` patterns from most to least specific.
///
/// JavaScript object keys do not preserve the order the file wrote the entries
/// in for integer-like names, so "the most specific match wins" is only
/// meaningful once the patterns are sorted: exact names first, then globs by
/// how many literal characters they have, then alphabetically as a tie
/// breaker.
pub(crate) fn sort_output_patterns(outputs: &mut [(String, OutputConfig)]) {
    outputs.sort_by(|(left, _), (right, _)| {
        return specificity(left).cmp(&specificity(right));
    });
}

/// The sort key of [`sort_output_patterns`]: glob flag, literal length, text.
fn specificity(pattern: &str) -> (bool, std::cmp::Reverse<usize>, String) {
    let is_glob = pattern.contains('*') || pattern.contains('?');
    let literals = pattern
        .chars()
        .filter(|c| return *c != '*' && *c != '?')
        .count();
    return (is_glob, std::cmp::Reverse(literals), pattern.to_owned());
}

/// `general = { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct General {
    /// Which modifier `Mod` in a binding refers to. Defaults per backend:
    /// Super on udev/headless, Alt when nested in winit.
    pub mod_key: Option<ModKey>,
    /// Ask clients to draw no decorations (`prefer no CSD`, `docs/config.md`).
    pub prefer_no_csd: bool,
    /// Spacing in logical pixels, applied by the compositor: layouts get a
    /// work area inset by this much on every side (`ctx.output.work_area`), and
    /// layouts use `ctx.gap` for the spacing between windows. It therefore
    /// affects window width *and* height, and keeps windows clear of
    /// layer-shell bars.
    pub gap: i32,
    /// Geometry and open/close animations.
    pub animations: Animations,
    /// Window decorations drawn by the compositor.
    pub border: Border,
    /// Drop shadow drawn behind windows.
    pub shadow: Shadow,
    /// Background blur.
    pub blur: Blur,
    /// Default window opacity, `0.0 ..= 1.0`.
    pub opacity: f64,
    /// A GLSL fragment shader run for every window.
    /// `None` keeps the built-in shader.
    pub window_shader: Option<String>,
    /// The software cursor the compositor draws.
    pub cursor: Cursor,
}

impl Default for General {
    fn default() -> Self {
        return Self {
            mod_key: None,
            prefer_no_csd: true,
            gap: 8,
            animations: Animations::default(),
            border: Border::default(),
            shadow: Shadow::default(),
            blur: Blur::default(),
            opacity: 1.0,
            window_shader: None,
            cursor: Cursor::default(),
        };
    }
}

/// `general.cursor = { … }`: the pointer the compositor draws.
///
/// Both fields fall back to the environment, which is where every other
/// Wayland session keeps them: `theme` to `$XCURSOR_THEME` (`"default"` when
/// that is unset too) and `size` to `$XCURSOR_SIZE` (`24`). A value in the
/// configuration wins over the variable and is also pushed to clients, so
/// toolkits and X11 applications that draw their own cursor agree with the
/// compositor.
///
/// `size` is the nominal size in logical pixels; the icon is loaded at
/// `size × output scale`, exactly like a themed cursor under
/// `XCURSOR_SIZE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// Theme name, as found by the xcursor loader in its search path.
    pub theme: Option<String>,
    /// Nominal cursor size in logical pixels.
    pub size: Option<u32>,
}

impl Default for Cursor {
    fn default() -> Self {
        return Self {
            theme: None,
            size: None,
        };
    }
}

impl Cursor {
    /// The theme name to load, falling back to the environment and `default`.
    pub fn theme_name(&self) -> String {
        if let Some(theme) = &self.theme {
            return theme.clone();
        }
        return std::env::var("XCURSOR_THEME").unwrap_or_else(|_| return "default".to_owned());
    }

    /// The nominal size to load, falling back to the environment and `24`.
    ///
    /// A size of zero would draw nothing, so the result is never smaller than
    /// one pixel.
    pub fn size_px(&self) -> u32 {
        if let Some(size) = self.size {
            return size.max(1);
        }
        return std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|size| return size.parse().ok())
            .unwrap_or(24u32)
            .max(1);
    }
}

/// `general.border = { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Border {
    /// Border thickness in logical pixels. `0` disables the border.
    pub width: f64,
    /// Corner radius in logical pixels, applied to every corner.
    pub radius: f64,
    /// Border color while the window is focused (focused output only).
    pub active: Color,
    /// Border color for every other window.
    pub inactive: Color,
}

impl Default for Border {
    fn default() -> Self {
        return Self {
            width: 0.0,
            radius: 0.0,
            active: Color::rgb(0x4f, 0x8c, 0xff),
            inactive: Color::rgb(0x3a, 0x3a, 0x3a),
        };
    }
}

/// `general.shadow = { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Shadow {
    /// Whether windows get a drop shadow.
    pub enable: bool,
    /// Blur radius (standard deviation) in logical pixels.
    pub blur: f64,
    /// How far the shadow silhouette grows beyond the rectangle, in logical
    /// pixels (negative values shrink it). niri calls this `spread`.
    pub spread: f64,
    /// Horizontal offset in logical pixels.
    pub offset_x: i32,
    /// Vertical offset in logical pixels.
    pub offset_y: i32,
    /// Shadow color, including its opacity.
    pub color: Color,
}

impl Default for Shadow {
    fn default() -> Self {
        return Self {
            enable: false,
            blur: 24.0,
            spread: 0.0,
            offset_x: 0,
            offset_y: 6,
            color: Color::rgba(0, 0, 0, 160),
        };
    }
}

/// `general.blur = { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Blur {
    /// Whether windows matching the rule blur what is behind them.
    pub enable: bool,
    /// Number of Kawase down/up passes.
    pub passes: u32,
    /// Sampling offset of the blur passes, in pixels.
    pub offset: f64,
    /// Amount of noise added to the blur, `0.0 ..= 1.0`. Hides banding.
    pub noise: f64,
    /// Honour `ext-background-effect-v1` regions requested by clients.
    ///
    /// When a client asks for a blurred region, that region replaces the
    /// whole-window backdrop `enable` would otherwise draw. Turning this off
    /// ignores every client request and keeps the compositor in charge of what
    /// gets blurred.
    pub accept_client_regions: bool,
    /// Blur the backdrop of layer surfaces (bars, panels, docks) that ask for
    /// one with `ext-background-effect-v1`.
    ///
    /// Layer surfaces never get a whole-surface backdrop the way windows do:
    /// only the regions a client requests are blurred, so a bar that does not
    /// ask stays sharp. `enable` is still the master switch.
    pub layer_surfaces: bool,
    /// Blur only the wallpaper instead of everything below the blurred
    /// surface.
    ///
    /// With `xray` on, the compositor blurs the background layers once per
    /// output and every blurred window samples that shared copy. Windows and
    /// bars then never show each other inside their backdrop, and a moving or
    /// animating window does not have to re-run the blur: only a change of the
    /// wallpaper (or of the output geometry) does.
    pub xray: bool,
    /// Custom blur shaders; `None` uses the built-in Kawase ones.
    pub shader: BlurShader,
}

impl Default for Blur {
    fn default() -> Self {
        return Self {
            enable: false,
            passes: 3,
            offset: 3.0,
            noise: 0.0,
            accept_client_regions: true,
            layer_surfaces: true,
            xray: true,
            shader: BlurShader::default(),
        };
    }
}

/// `general.blur.shader = { … }`: replacement blur passes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlurShader {
    /// Downsample pass (`blur_down`).
    pub down: Option<String>,
    /// Upsample pass (`blur_up`).
    pub up: Option<String>,
    /// Final pass applied to the last upsample (`blur_finish`).
    pub finish: Option<String>,
}

/// A color written as `#rgb`, `#rrggbb` or `#rrggbbaa`.
///
/// Components are stored as-is (not premultiplied); the renderer converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha; `255` is opaque.
    pub a: u8,
}

impl Color {
    /// An opaque color.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        return Color { r, g, b, a: 255 };
    }

    /// A color with an explicit alpha.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        return Color { r, g, b, a };
    }

    /// Parse `#rgb`, `#rrggbb` or `#rrggbbaa` (case-insensitive, `#` required).
    pub fn parse(text: &str) -> Option<Color> {
        let hex = text.trim();
        let hex = hex.strip_prefix('#')?;
        let nibble = |index: usize| -> Option<u8> {
            let byte = hex.as_bytes().get(index)?;
            return (*byte as char).to_digit(16).map(|value| return value as u8);
        };
        return match hex.len() {
            3 => {
                let r = nibble(0)?;
                let g = nibble(1)?;
                let b = nibble(2)?;
                Some(Color::rgb(r * 17, g * 17, b * 17))
            }
            6 => {
                let r = nibble(0)? * 16 + nibble(1)?;
                let g = nibble(2)? * 16 + nibble(3)?;
                let b = nibble(4)? * 16 + nibble(5)?;
                Some(Color::rgb(r, g, b))
            }
            8 => {
                let r = nibble(0)? * 16 + nibble(1)?;
                let g = nibble(2)? * 16 + nibble(3)?;
                let b = nibble(4)? * 16 + nibble(5)?;
                let a = nibble(6)? * 16 + nibble(7)?;
                Some(Color::rgba(r, g, b, a))
            }
            _ => None,
        };
    }

    /// Components as `0.0 ..= 1.0` floats, in `r, g, b, a` order and straight
    /// (not premultiplied) alpha.
    pub fn to_floats(self) -> [f32; 4] {
        return [
            f32::from(self.r) / 255.0,
            f32::from(self.g) / 255.0,
            f32::from(self.b) / 255.0,
            f32::from(self.a) / 255.0,
        ];
    }

    /// Whether the color is fully transparent.
    pub fn is_transparent(self) -> bool {
        return self.a == 0;
    }
}

impl std::fmt::Display for Color {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        return write!(
            formatter,
            "#{:02x}{:02x}{:02x}{:02x}",
            self.r, self.g, self.b, self.a
        );
    }
}

/// `general.animations = { … }`.
///
/// The layout follows niri: every kind of WM animation has its own duration
/// and easing curve. `duration` is the legacy fallback for all categories, so
/// configurations written before the per-category settings existed keep
/// working unchanged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Animations {
    /// Master switch: when false every animation is instant.
    pub enable: bool,
    /// Fallback animation length in milliseconds for categories that do not
    /// specify their own `duration`.
    pub duration: u16,
    /// Multiplier applied to every animation duration (`1.0` is normal,
    /// `2.0` is twice as slow).
    pub slowdown: f64,
    /// Opening a window (fade + scale).
    pub window_open: AnimationOverride,
    /// Closing a window (fade + scale).
    pub window_close: AnimationOverride,
    /// Moving a window without changing its size.
    pub window_movement: AnimationOverride,
    /// Resizing a window.
    pub window_resize: AnimationOverride,
    /// Sliding between vertical workspaces.
    pub workspace_switch: AnimationOverride,
    /// Opening or closing the workspace overview.
    pub overview: AnimationOverride,
}

impl Default for Animations {
    fn default() -> Self {
        return Self {
            enable: true,
            duration: 180,
            slowdown: 1.0,
            window_open: AnimationOverride::default(),
            window_close: AnimationOverride::default(),
            window_movement: AnimationOverride::default(),
            window_resize: AnimationOverride::default(),
            workspace_switch: AnimationOverride::default(),
            overview: AnimationOverride::default(),
        };
    }
}

/// Per-category animation overrides.
///
/// Missing fields fall back to the global `general.animations.duration` and
/// the category's niri-inspired default curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnimationOverride {
    /// Duration in milliseconds, overriding `Animations::duration`.
    pub duration: Option<u16>,
    /// Easing curve, overriding the category default.
    pub curve: Option<AnimationCurve>,
}

/// The easing curves accepted by `general.animations.*.curve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationCurve {
    /// Constant speed.
    Linear,
    /// Fast start, gentle end.
    EaseOutQuad,
    /// Faster start than `ease-out-quad`, gentle end (the old Poly easing).
    EaseOutCubic,
    /// Very fast start, long gentle settle (niri's window-open default).
    EaseOutExpo,
}

impl AnimationCurve {
    /// Evaluate the curve at a normalized time in `0.0 ..= 1.0`.
    pub fn apply(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        return match self {
            AnimationCurve::Linear => t,
            AnimationCurve::EaseOutQuad => 1.0 - (1.0 - t) * (1.0 - t),
            AnimationCurve::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
            AnimationCurve::EaseOutExpo => {
                if t >= 1.0 {
                    1.0
                } else {
                    1.0 - 2.0f64.powf(-10.0 * t)
                }
            }
        };
    }

    /// Parse the name used in a configuration file.
    pub fn parse(name: &str) -> Option<AnimationCurve> {
        return match name {
            "linear" => Some(AnimationCurve::Linear),
            "ease-out-quad" => Some(AnimationCurve::EaseOutQuad),
            "ease-out-cubic" => Some(AnimationCurve::EaseOutCubic),
            "ease-out-expo" => Some(AnimationCurve::EaseOutExpo),
            _ => None,
        };
    }
}

/// A resolved animation: an absolute duration and the curve to run through it.
///
/// `poly-config` resolves this so the layout does not have to know which fields
/// were overridden and which fell back to the global duration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimationSpec {
    /// How long the animation runs. Zero means "do not animate".
    pub duration: std::time::Duration,
    /// The easing curve.
    pub curve: AnimationCurve,
}

impl AnimationSpec {
    /// The eased progress at `elapsed` since the animation started.
    pub fn progress(&self, elapsed: std::time::Duration) -> f64 {
        let total = self.duration.as_secs_f64();
        if total <= 0.0 {
            return 1.0;
        }
        return self
            .curve
            .apply(elapsed.as_secs_f64().clamp(0.0, total) / total);
    }

    /// Whether the animation is still running at `elapsed`.
    pub fn running(&self, elapsed: std::time::Duration) -> bool {
        return elapsed < self.duration;
    }
}

impl Animations {
    /// Resolve one category against the legacy global duration and `slowdown`.
    fn resolve(
        &self,
        overrides: AnimationOverride,
        default_curve: AnimationCurve,
    ) -> AnimationSpec {
        let milliseconds = overrides.duration.unwrap_or(self.duration);
        let seconds = f64::from(milliseconds) / 1000.0 * self.slowdown.max(0.0);
        let duration = if self.enable {
            std::time::Duration::from_secs_f64(seconds)
        } else {
            std::time::Duration::ZERO
        };
        return AnimationSpec {
            duration,
            curve: overrides.curve.unwrap_or(default_curve),
        };
    }

    /// Opening a window: niri scales from 50% to 100% while fading in.
    pub fn window_open(&self) -> AnimationSpec {
        return self.resolve(self.window_open, AnimationCurve::EaseOutExpo);
    }

    /// Closing a window: niri fades out while shrinking to 80%.
    pub fn window_close(&self) -> AnimationSpec {
        return self.resolve(self.window_close, AnimationCurve::EaseOutQuad);
    }

    /// Moving a window without resizing it.
    pub fn window_movement(&self) -> AnimationSpec {
        return self.resolve(self.window_movement, AnimationCurve::EaseOutCubic);
    }

    /// Resizing a window.
    pub fn window_resize(&self) -> AnimationSpec {
        return self.resolve(self.window_resize, AnimationCurve::EaseOutCubic);
    }

    /// Sliding between workspaces.
    pub fn workspace_switch(&self) -> AnimationSpec {
        return self.resolve(self.workspace_switch, AnimationCurve::EaseOutCubic);
    }

    /// Opening or closing the overview.
    pub fn overview(&self) -> AnimationSpec {
        return self.resolve(self.overview, AnimationCurve::EaseOutCubic);
    }
}

/// A modifier key usable as `Mod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModKey {
    /// Ctrl.
    Ctrl,
    /// Shift.
    Shift,
    /// Alt.
    Alt,
    /// Super / Windows key.
    Super,
    /// ISO level 3 shift (AltGr).
    IsoLevel3Shift,
    /// ISO level 5 shift.
    IsoLevel5Shift,
}

impl ModKey {
    /// The modifier bit matching this key.
    pub fn to_modifiers(self) -> crate::Modifiers {
        return match self {
            ModKey::Ctrl => crate::Modifiers::CTRL,
            ModKey::Shift => crate::Modifiers::SHIFT,
            ModKey::Alt => crate::Modifiers::ALT,
            ModKey::Super => crate::Modifiers::SUPER,
            ModKey::IsoLevel3Shift => crate::Modifiers::ISO_LEVEL3_SHIFT,
            ModKey::IsoLevel5Shift => crate::Modifiers::ISO_LEVEL5_SHIFT,
        };
    }
}

/// `input = { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    /// Keyboard layout and repeat settings.
    pub keyboard: Keyboard,
    /// Per-device libinput settings, matched by name glob (first match wins).
    pub devices: Vec<(String, Device)>,
}

impl Default for Input {
    fn default() -> Self {
        return Self {
            keyboard: Keyboard::default(),
            devices: Vec::new(),
        };
    }
}

/// `input.keyboard = { … }`, the xkb configuration and key repeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyboard {
    /// xkb rules file, empty for the default.
    pub rules: String,
    /// xkb model.
    pub model: String,
    /// xkb layout, e.g. `"us,cn"`.
    pub layout: String,
    /// xkb variant.
    pub variant: String,
    /// xkb options, e.g. `"grp:alt_shift_toggle"`.
    pub options: Option<String>,
    /// Key repeat rate in events per second, 0 disables repeating.
    pub repeat_rate: u8,
    /// Key repeat delay in milliseconds.
    pub repeat_delay: u16,
}

impl Default for Keyboard {
    fn default() -> Self {
        return Self {
            rules: String::new(),
            model: String::new(),
            layout: "us".to_owned(),
            variant: String::new(),
            options: None,
            repeat_rate: 25,
            repeat_delay: 600,
        };
    }
}

/// `input.devices["*glob*"] = { … }`, libinput device settings. Everything is
/// optional; unset fields keep the libinput default.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    /// Tap-to-click.
    pub tap: Option<bool>,
    /// Tap-and-drag.
    pub drag: Option<bool>,
    /// Keep dragging after the finger is lifted, until the next tap.
    pub drag_lock: Option<bool>,
    /// Disable the touchpad while typing.
    pub dwt: Option<bool>,
    /// Natural (inverted) scrolling.
    pub natural_scroll: Option<bool>,
    /// Swap left and right buttons.
    pub left_handed: Option<bool>,
    /// How a tap is turned into a click.
    pub click_method: Option<ClickMethod>,
    /// How two-finger scrolling is reported.
    pub scroll_method: Option<ScrollMethod>,
    /// Pointer acceleration profile.
    pub accel_profile: Option<AccelProfile>,
    /// Pointer acceleration speed, -1.0 ..= 1.0.
    pub accel_speed: Option<f64>,
    /// A 3x3 calibration matrix.
    pub calibration: Option<Vec<f32>>,
    /// Name of the output this device is mapped to.
    pub map_to_output: Option<String>,
}

impl Default for Device {
    fn default() -> Self {
        return Self {
            tap: None,
            drag: None,
            drag_lock: None,
            dwt: None,
            natural_scroll: None,
            left_handed: None,
            click_method: None,
            scroll_method: None,
            accel_profile: None,
            accel_speed: None,
            calibration: None,
            map_to_output: None,
        };
    }
}

/// `input.devices.*.click_method`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickMethod {
    /// Software buttons at the bottom of the touchpad.
    ButtonAreas,
    /// Clickfinger: number of fingers decides the button.
    Clickfinger,
}

/// `input.devices.*.scroll_method`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollMethod {
    /// No scrolling.
    NoScroll,
    /// Two-finger scrolling.
    TwoFinger,
    /// Scrolling along the edge.
    Edge,
    /// Scrolling while a button is held.
    OnButtonDown,
}

/// `input.devices.*.accel_profile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelProfile {
    /// Accelerate with speed (default for touchpads).
    Adaptive,
    /// No acceleration (default for mice).
    Flat,
}

/// `window_drag = { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowDrag {
    /// What dragging a tiled window does.
    pub tiled: TiledDrag,
    /// Pixels of movement before a press becomes a drag.
    pub threshold: f64,
    /// Modifier that locks the drag to one axis.
    pub axis_lock: Option<ModKey>,
    /// Whether Escape cancels a drag and restores the geometry.
    pub escape_cancels: bool,
    /// How the resize edges are picked from the pointer position.
    pub resize_pick: ResizePick,
    /// Whether an interactive resize waits for the client to acknowledge a
    /// size before asking for the next one.
    pub throttle_resize: bool,
}

impl Default for WindowDrag {
    fn default() -> Self {
        return Self {
            tiled: TiledDrag::Reorder,
            threshold: 8.0,
            axis_lock: Some(ModKey::Shift),
            escape_cancels: true,
            resize_pick: ResizePick::Thirds,
            throttle_resize: true,
        };
    }
}

/// `layer_rules[i]`: what to do with layer surfaces matching `match`.
///
/// Poly never draws decorations for layer surfaces, but it does decide who
/// gets the keyboard: a bar that asks for `on-demand` focus steals it from
/// windows on every click, which users of tiling setups often do not want.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LayerRule {
    /// Glob matched against the layer surface's namespace. `None` matches all.
    pub namespace: Option<String>,
    /// Override what the client asked for.
    pub keyboard_interactivity: Option<LayerInteractivity>,
}

impl LayerRule {
    /// Whether this rule applies to a layer surface namespace.
    pub fn matches(&self, namespace: &str) -> bool {
        return match &self.namespace {
            Some(pattern) => crate::glob::glob_matches(pattern, namespace),
            None => true,
        };
    }
}

/// `layer_rules[i].keyboard_interactivity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerInteractivity {
    /// Never take the keyboard.
    None,
    /// Take it when clicked (menus, launchers).
    OnDemand,
    /// Always hold it while mapped (lock screens, overlays).
    Exclusive,
}

/// `window_drag.tiled`: what happens when a tiled window is dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiledDrag {
    /// Move it inside the layout and reorder at the drop position (default).
    Reorder,
    /// Pull it out of the layout into a floating window.
    Float,
    /// Ignore the drag, the layout keeps the geometry.
    Ignore,
}

/// `window_drag.resize_pick`: how the resize edges are chosen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResizePick {
    /// Outer thirds of the window: the middle third is not resizable.
    Thirds,
    /// Pixels from the border.
    Pixels(f64),
}

/// One entry of `spawn_at_startup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupSpawn {
    /// Run the argv directly.
    Exec(Vec<String>),
    /// Run the line through `sh -c`.
    Shell(String),
}

/// A named scripted layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutInfo {
    /// The name used by `default_layout`, `set-layout` and `cycle-layout`.
    pub name: String,
    /// Whether the layout also positions fullscreen windows.
    pub handles_fullscreen: bool,
    /// Key bindings that only apply while a workspace uses this layout.
    ///
    /// Their actions are [`poly_ipc::Action::LayoutAction`] values, already
    /// checked against the layout's own `actions` table by the parser.
    pub keybinds: Vec<Keybind>,
}

/// `window_rules[i]`: state and appearance applied when a window maps.
///
/// The layout engine acts on `float`, `focus` and `open_on_output`; the visual
/// fields are resolved per window when it maps.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowRule {
    /// Glob matched against the window's `app_id`.
    pub app_id: Option<String>,
    /// Glob matched against the window title.
    pub title: Option<String>,
    /// Open the window floating.
    pub float: Option<bool>,
    /// Take focus when the window maps.
    pub focus: Option<bool>,
    /// Open on the first output whose name matches this glob.
    pub open_on_output: Option<String>,
    /// Blur behind the window.
    pub blur: Option<bool>,
    /// Corner radius in logical pixels.
    pub radius: Option<f64>,
    /// Draw a shadow.
    pub shadow: Option<bool>,
    /// Window opacity, 0.0 ..= 1.0.
    pub opacity: Option<f64>,
    /// Ask this window to use client-side decorations.
    pub no_csd: Option<bool>,
    /// A GLSL window shader for this window, overriding
    /// `general.window_shader`. Resolved like every other path.
    pub window_shader: Option<String>,
}

impl WindowRule {
    /// Whether the rule matches a window's app id and title.
    pub fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        if let Some(pattern) = &self.app_id {
            let Some(app_id) = app_id else {
                return false;
            };
            if !crate::glob::glob_matches(pattern, app_id) {
                return false;
            }
        }
        if let Some(pattern) = &self.title {
            let Some(title) = title else {
                return false;
            };
            if !crate::glob::glob_matches(pattern, title) {
                return false;
            }
        }
        return true;
    }
}

/// Per-output configuration (`docs/config.md`).
///
/// Every field is optional: what the file does not say is left to the backend
/// (the DRM backend keeps its preferred mode, the nested window keeps the
/// size the host gave it).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutputConfig {
    /// Which mode to drive the output with.
    pub mode: Option<OutputMode>,
    /// Scale factor, or `"auto"` to leave it to the backend.
    pub scale: Option<OutputScale>,
    /// Rotation or mirroring.
    pub transform: Option<OutputTransform>,
    /// Where the output sits in compositor space.
    pub position: Option<OutputPosition>,
    /// Variable refresh rate.
    pub vrr: Option<bool>,
    /// Whether this output is the focused one after the configuration is
    /// applied (`docs/config.md`).
    pub focus: Option<bool>,
    /// Solid color drawn behind every window of this output.
    pub background: Option<Color>,
    /// Values passed to scripted layouts in `ctx.options`.
    pub layout_options: Vec<(String, LayoutOptionValue)>,
}

/// `outputs.<name>.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// The mode the output itself advertises as preferred.
    Preferred,
    /// A resolution, optionally with a refresh rate.
    Size {
        /// Width in physical pixels.
        width: u16,
        /// Height in physical pixels.
        height: u16,
        /// Refresh rate in millihertz, when the file asked for one.
        refresh: Option<u32>,
    },
}

/// `outputs.<name>.scale`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputScale {
    /// Leave the scale to the backend (1.0 on DRM, the host's scale in winit).
    Auto,
    /// A fixed fractional scale factor.
    Fixed(f64),
}

/// `outputs.<name>.transform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTransform {
    /// Upright.
    Normal,
    /// Rotated 90° clockwise.
    Rotate90,
    /// Upside down.
    Rotate180,
    /// Rotated 270° clockwise (90° counter-clockwise).
    Rotate270,
    /// Mirrored horizontally.
    Flipped,
    /// Mirrored and rotated 90°.
    Flipped90,
    /// Mirrored and rotated 180° (mirrored vertically).
    Flipped180,
    /// Mirrored and rotated 270°.
    Flipped270,
}

impl OutputTransform {
    /// Parse the name used in the configuration.
    ///
    /// Accepts the `wl_output`/`wlr-randr` spelling (`normal`, `90`, `180`,
    /// `270`, `flipped`, `flipped-90`, …) as well as a `rotate`/`flip` prefix,
    /// because both are in the wild.
    pub fn parse(text: &str) -> Option<OutputTransform> {
        let normalized = text.trim().to_ascii_lowercase().replace('_', "-");
        return match normalized.as_str() {
            "normal" | "rotate0" | "rotate-0" | "0" => Some(OutputTransform::Normal),
            "90" | "rotate90" | "rotate-90" => Some(OutputTransform::Rotate90),
            "180" | "rotate180" | "rotate-180" => Some(OutputTransform::Rotate180),
            "270" | "rotate270" | "rotate-270" => Some(OutputTransform::Rotate270),
            "flipped" | "flip" | "flipped-0" => Some(OutputTransform::Flipped),
            "flipped-90" | "flip-90" => Some(OutputTransform::Flipped90),
            "flipped-180" | "flip-180" => Some(OutputTransform::Flipped180),
            "flipped-270" | "flip-270" => Some(OutputTransform::Flipped270),
            _ => None,
        };
    }

    /// The name to use in messages.
    pub fn as_str(self) -> &'static str {
        return match self {
            OutputTransform::Normal => "normal",
            OutputTransform::Rotate90 => "90",
            OutputTransform::Rotate180 => "180",
            OutputTransform::Rotate270 => "270",
            OutputTransform::Flipped => "flipped",
            OutputTransform::Flipped90 => "flipped-90",
            OutputTransform::Flipped180 => "flipped-180",
            OutputTransform::Flipped270 => "flipped-270",
        };
    }
}

/// `outputs.<name>.position`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputPosition {
    /// Let the compositor place the output (to the right of the others).
    Auto,
    /// A fixed location in compositor space.
    Fixed {
        /// Horizontal position.
        x: i32,
        /// Vertical position.
        y: i32,
    },
}

/// A primitive value forwarded from `outputs.<name>.layout_options`.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutOptionValue {
    /// Boolean.
    Bool(bool),
    /// Integer.
    Integer(i64),
    /// Floating point number.
    Number(f64),
    /// String.
    String(String),
}

/// `fullscreen = { … }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fullscreen {
    /// Whether a compositor-managed fullscreen window covers `Top` layers.
    pub cover_top_layer: bool,
}

impl Default for Fullscreen {
    fn default() -> Self {
        return Self {
            cover_top_layer: true,
        };
    }
}

/// `overview = { … }`.
///
/// The defaults follow niri: cards shrink to half size, sit on an opaque
/// `#262626` backdrop and cast a soft shadow. `zoom` is clamped to
/// `0.0001 ..= 0.75` like niri, so the overview can never zoom a card up.
#[derive(Debug, Clone, PartialEq)]
pub struct Overview {
    /// Card scale at full overview (`1.0` is the plain output).
    pub zoom: f64,
    /// Color of the desktop behind the cards.
    pub backdrop_color: Color,
    /// Background color of a workspace card.
    pub card_color: Color,
    /// Shadow drawn behind every card. `blur` is the niri "softness".
    pub shadow: Shadow,
}

impl Default for Overview {
    fn default() -> Self {
        return Self {
            zoom: 0.5,
            backdrop_color: Color::rgb(0x26, 0x26, 0x26),
            card_color: Color::rgb(0x40, 0x40, 0x40),
            shadow: Shadow {
                enable: true,
                blur: 40.0,
                spread: 10.0,
                offset_x: 0,
                offset_y: 10,
                color: Color::rgba(0, 0, 0, 0x50),
            },
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documentation() {
        let config = Config::default();
        assert_eq!(config.general.gap, 8);
        assert!(config.general.prefer_no_csd);
        assert_eq!(config.general.mod_key, None);
        assert_eq!(config.input.keyboard.layout, "us");
        assert_eq!(config.input.keyboard.repeat_rate, 25);
        assert_eq!(config.input.keyboard.repeat_delay, 600);
        assert_eq!(config.window_drag.threshold, 8.0);
        assert!(config.window_drag.escape_cancels);
        assert!(config.window_drag.throttle_resize);
        assert_eq!(config.window_drag.axis_lock, Some(ModKey::Shift));
        assert_eq!(config.window_drag.tiled, TiledDrag::Reorder);
        assert_eq!(config.window_drag.resize_pick, ResizePick::Thirds);
        assert_eq!(
            config.general.animations.window_open().duration,
            std::time::Duration::from_millis(180)
        );
        assert_eq!(
            config.general.animations.window_open().curve,
            AnimationCurve::EaseOutExpo
        );
        assert_eq!(
            config.general.animations.window_close().curve,
            AnimationCurve::EaseOutQuad
        );
    }
}

/// `errors = { … }`.
///
/// The compositor always draws its own Toasts; these settings only add another
/// channel on top of them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Errors {
    /// Also send problems to `org.freedesktop.Notifications`, so that a
    /// configuration typo reaches the user even when the Toasts are covered or
    /// the screen is not being watched.
    ///
    /// Needs a binary built with the `dbus` feature (`docs/config.md`).
    pub dbus: bool,
}

/// `xwayland = { … }`: X11 clients through `xwayland-satellite`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xwayland {
    /// Set up X11 at all. Off means no X11 sockets and no `DISPLAY` for
    /// children, i.e. X11-only programs cannot start.
    pub enable: bool,
    /// The program to run, as a name in `$PATH` or a path (`~` expands).
    pub satellite_path: String,
    /// Start the satellite when the first client connects, and again after it
    /// goes away. Off means "keep it running": started at startup, restarted
    /// when it exits.
    pub on_demand: bool,
    /// Also start the satellite at startup while `on_demand` is on, so the
    /// first X11 client does not wait for the compositor to notice it.
    pub spawn_at_startup: bool,
}

impl Default for Xwayland {
    fn default() -> Self {
        return Self {
            enable: true,
            satellite_path: "xwayland-satellite".to_owned(),
            on_demand: true,
            spawn_at_startup: false,
        };
    }
}
