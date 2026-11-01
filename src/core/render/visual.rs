// Ported from niri (GPL-3.0-or-later): src/utils/mod.rs (CornerRadius),
// src/layout/mod.rs (window rules)

//! Per-window visual state: corner radius, opacity, border, shadow, blur and
//! the window shader. Resolved once from `general` plus the first matching
//! window rule, and re-resolved whenever the configuration is reloaded.

use smithay::utils::{Logical, Rectangle};

use poly_config::{Color, Config};

/// Corner radii of a window, in logical pixels.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct CornerRadius {
    /// Top left corner.
    pub top_left: f32,
    /// Top right corner.
    pub top_right: f32,
    /// Bottom right corner.
    pub bottom_right: f32,
    /// Bottom left corner.
    pub bottom_left: f32,
}

impl CornerRadius {
    /// Shrink any radius that does not fit into a `width × height` rectangle.
    pub fn fit_to(self, width: f32, height: f32) -> CornerRadius {
        let max_radius = width.min(height) / 2.0;
        let clamp = |radius: f32| return radius.clamp(0.0, max_radius);
        return CornerRadius {
            top_left: clamp(self.top_left),
            top_right: clamp(self.top_right),
            bottom_right: clamp(self.bottom_right),
            bottom_left: clamp(self.bottom_left),
        };
    }

    /// Whether every corner is zero (no clipping needed).
    pub fn is_zero(&self) -> bool {
        return self.top_left == 0.0
            && self.top_right == 0.0
            && self.bottom_right == 0.0
            && self.bottom_left == 0.0;
    }

    /// The largest corner radius.
    pub fn max(&self) -> f32 {
        return self
            .top_left
            .max(self.top_right)
            .max(self.bottom_right)
            .max(self.bottom_left);
    }

    /// The radius of the corresponding *inner* rectangle when a border of
    /// `width` is drawn around the window.
    pub fn inner(self, width: f32) -> CornerRadius {
        let shrink = |radius: f32| return (radius - width).max(0.0);
        return CornerRadius {
            top_left: shrink(self.top_left),
            top_right: shrink(self.top_right),
            bottom_right: shrink(self.bottom_right),
            bottom_left: shrink(self.bottom_left),
        };
    }
}

impl From<f32> for CornerRadius {
    fn from(radius: f32) -> CornerRadius {
        return CornerRadius {
            top_left: radius,
            top_right: radius,
            bottom_right: radius,
            bottom_left: radius,
        };
    }
}

impl From<[f32; 4]> for CornerRadius {
    fn from(radius: [f32; 4]) -> CornerRadius {
        // Order matches the GLSL uniform: (tl, tr, br, bl).
        return CornerRadius {
            top_left: radius[0],
            top_right: radius[1],
            bottom_right: radius[2],
            bottom_left: radius[3],
        };
    }
}

impl From<CornerRadius> for [f32; 4] {
    fn from(radius: CornerRadius) -> [f32; 4] {
        return [
            radius.top_left,
            radius.top_right,
            radius.bottom_right,
            radius.bottom_left,
        ];
    }
}

impl From<CornerRadius> for f64 {
    fn from(radius: CornerRadius) -> f64 {
        return radius.max() as f64;
    }
}

/// Shadow parameters of one window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowParams {
    /// Blur sigma in logical pixels.
    pub sigma: f32,
    /// Shadow spread in logical pixels (negative shrinks the silhouette).
    pub spread: f32,
    /// Horizontal offset in logical pixels.
    pub offset_x: f32,
    /// Vertical offset in logical pixels.
    pub offset_y: f32,
    /// Straight shadow color (already converted to 0..1 floats).
    pub color: [f32; 4],
}

/// The blur settings of one window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlurParams {
    /// Number of Kawase pass pairs.
    pub passes: u32,
    /// Sampling offset in pixels.
    pub offset: f64,
    /// Dithering noise amount, `0.0 ..= 1.0`.
    pub noise: f32,
}

/// Everything visual about a window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowVisuals {
    /// Corner radius in logical pixels.
    pub radius: CornerRadius,
    /// Window opacity, multiplied with the animation alpha.
    pub opacity: f32,
    /// Border width in logical pixels; `0` disables the border.
    pub border_width: f32,
    /// Border color while the window is focused.
    pub border_active: [f32; 4],
    /// Border color for unfocused windows.
    pub border_inactive: [f32; 4],
    /// Drop shadow, if enabled.
    pub shadow: Option<ShadowParams>,
    /// Background blur, if enabled.
    pub blur: Option<BlurParams>,
    /// Absolute path of the window shader, `None` for the built-in one.
    pub shader_path: Option<String>,
    /// Whether the window should be asked to use CSD.
    pub no_csd: bool,
}

impl Default for WindowVisuals {
    fn default() -> Self {
        return Self {
            radius: CornerRadius::default(),
            opacity: 1.0,
            border_width: 0.0,
            border_active: [0.31, 0.55, 1.0, 1.0],
            border_inactive: [0.23, 0.23, 0.23, 1.0],
            shadow: None,
            blur: None,
            shader_path: None,
            no_csd: true,
        };
    }
}

fn color_to_float(color: Color) -> [f32; 4] {
    return [
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        f32::from(color.a) / 255.0,
    ];
}

impl WindowVisuals {
    /// Resolve the visuals for a window from `general` and the window rules.
    pub fn resolve(config: &Config, app_id: Option<&str>, title: Option<&str>) -> WindowVisuals {
        let general = &config.general;
        let rule = config
            .window_rules
            .iter()
            .find(|rule| return rule.matches(app_id, title));

        let radius = rule
            .and_then(|rule| return rule.radius)
            .unwrap_or(general.border.radius);
        let opacity = rule
            .and_then(|rule| return rule.opacity)
            .unwrap_or(general.opacity);
        let blur_enabled = rule
            .and_then(|rule| return rule.blur)
            .unwrap_or(general.blur.enable);
        let shadow_enabled = rule
            .and_then(|rule| return rule.shadow)
            .unwrap_or(general.shadow.enable);
        let no_csd = rule
            .and_then(|rule| return rule.no_csd)
            .unwrap_or(general.prefer_no_csd);
        let shader_path = rule
            .and_then(|rule| return rule.window_shader.clone())
            .or_else(|| return general.window_shader.clone());

        return WindowVisuals {
            radius: CornerRadius::from(radius as f32),
            opacity: opacity.clamp(0.0, 1.0) as f32,
            border_width: general.border.width.max(0.0) as f32,
            border_active: color_to_float(general.border.active),
            border_inactive: color_to_float(general.border.inactive),
            shadow: shadow_enabled.then(|| {
                return ShadowParams {
                    sigma: general.shadow.blur.max(0.0) as f32,
                    spread: general.shadow.spread as f32,
                    offset_x: general.shadow.offset_x as f32,
                    offset_y: general.shadow.offset_y as f32,
                    color: color_to_float(general.shadow.color),
                };
            }),
            blur: blur_enabled.then(|| {
                return BlurParams {
                    passes: general.blur.passes.clamp(1, 16),
                    offset: general.blur.offset,
                    noise: general.blur.noise.clamp(0.0, 1.0) as f32,
                };
            }),
            shader_path,
            no_csd,
        };
    }

    /// The border color for the current focus state.
    pub fn border_color(&self, focused: bool) -> [f32; 4] {
        return if focused {
            self.border_active
        } else {
            self.border_inactive
        };
    }

    /// The visuals of the same window drawn at `zoom`.
    ///
    /// The overview rescales a whole workspace, so radius, border width, shadow
    /// blur and shadow offset have to shrink with it or the thumbnail would
    /// look bolder than the real window. Blur is dropped: an offscreen blur
    /// pass per thumbnail is unusable on a software renderer and niri does not
    /// blur the overview either.
    pub fn scaled(&self, zoom: f64) -> WindowVisuals {
        let factor = zoom.clamp(0.0001, 1.0) as f32;
        let mut visuals = self.clone();
        visuals.radius = CornerRadius {
            top_left: self.radius.top_left * factor,
            top_right: self.radius.top_right * factor,
            bottom_right: self.radius.bottom_right * factor,
            bottom_left: self.radius.bottom_left * factor,
        };
        visuals.border_width = self.border_width * factor;
        visuals.shadow = self.shadow.map(|shadow| {
            return ShadowParams {
                sigma: shadow.sigma * factor,
                spread: shadow.spread * factor,
                offset_x: shadow.offset_x * factor,
                offset_y: shadow.offset_y * factor,
                color: shadow.color,
            };
        });
        visuals.blur = None;
        return visuals;
    }

    /// The visuals of a window drawn at an animation scale.
    ///
    /// Unlike [`WindowVisuals::scaled`] this keeps the blur: an opening window
    /// is still a real window composited over the wallpaper, so dropping its
    /// backdrop would make blur pop in at the end of the animation. The blur
    /// sampling offset shrinks with the window so the softened region stays
    /// proportional.
    pub fn scaled_animation(&self, scale: f64) -> WindowVisuals {
        let factor = scale.clamp(0.0001, 1.0) as f32;
        let mut visuals = self.clone();
        visuals.radius = CornerRadius {
            top_left: self.radius.top_left * factor,
            top_right: self.radius.top_right * factor,
            bottom_right: self.radius.bottom_right * factor,
            bottom_left: self.radius.bottom_left * factor,
        };
        visuals.border_width = self.border_width * factor;
        visuals.shadow = self.shadow.map(|shadow| {
            return ShadowParams {
                sigma: shadow.sigma * factor,
                spread: shadow.spread * factor,
                offset_x: shadow.offset_x * factor,
                offset_y: shadow.offset_y * factor,
                color: shadow.color,
            };
        });
        visuals.blur = self.blur.map(|blur| {
            return BlurParams {
                passes: blur.passes,
                offset: blur.offset * f64::from(factor),
                noise: blur.noise,
            };
        });
        return visuals;
    }

    /// The rectangle a shadow element has to cover for a `rect` window.
    ///
    /// The element is the window rectangle expanded by `3 * sigma` (plus a
    /// pixel of anti-aliasing) and by the shadow offsets, so the shader can
    /// draw the whole blurred silhouette.
    pub fn shadow_rect(&self, rect: Rectangle<i32, Logical>) -> Option<Rectangle<i32, Logical>> {
        let shadow = self.shadow?;
        return shadow_rect(
            rect,
            shadow.sigma,
            shadow.spread.round() as i32,
            shadow.offset_x.round() as i32,
            shadow.offset_y.round() as i32,
        );
    }
}

/// The rectangle a shadow element has to cover: `rect` expanded by
/// `3 * sigma` plus a pixel of anti-aliasing, by the spread and by the
/// (rounded) offsets.
pub fn shadow_rect(
    rect: Rectangle<i32, Logical>,
    sigma: f32,
    spread: i32,
    offset_x: i32,
    offset_y: i32,
) -> Option<Rectangle<i32, Logical>> {
    if rect.size.w <= 0 || rect.size.h <= 0 {
        return None;
    }
    let padding = (sigma.max(0.0) * 3.0).ceil() as i32 + 2;

    let left = padding + (spread - offset_x).max(0);
    let right = padding + (offset_x + spread).max(0);
    let top = padding + (spread - offset_y).max(0);
    let bottom = padding + (offset_y + spread).max(0);

    return Some(Rectangle::new(
        (rect.loc.x - left, rect.loc.y - top).into(),
        (rect.size.w + left + right, rect.size.h + top + bottom).into(),
    ));
}
