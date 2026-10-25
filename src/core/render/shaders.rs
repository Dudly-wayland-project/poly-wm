// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/mod.rs

//! Shader programs used by the compositor: window shading, blur passes, window
//! borders and drop shadows (`docs/plan.md` §10.3, §10.4).
//!
//! Everything is GLES2 / GLSL 100. User shader files are assembled from a
//! prelude (which declares the uniforms) plus the user's entry points, so a
//! custom shader only contains `window_shade()`, `downsample()`, `upsample()`
//! or `finish()`. `#include` directives are expanded here, in Rust.
//!
//! Compilation is defensive: a failing file never replaces a working program,
//! the error is collected for a Toast, and the built-in default keeps being
//! used.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use glam::Mat3;
use smithay::backend::renderer::gles::{
    GlesError, GlesPixelProgram, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
    UniformValue,
};
use tracing::warn;

use poly_config::Config;

use super::blur::BlurProgram;

/// Deepest `#include` nesting we accept.
const MAX_INCLUDE_DEPTH: usize = 8;

// --- Shader sources ---------------------------------------------------------

const WINDOW_PRELUDE: &str = include_str!("shaders/window_prelude.frag");
const WINDOW_MAIN: &str = include_str!("shaders/window_main.frag");
const WINDOW_DEFAULT: &str = include_str!("shaders/window_shade_default.frag");
const ROUNDING_ALPHA: &str = include_str!("shaders/rounding_alpha.frag");

const BLUR_PASS_PRELUDE: &str = include_str!("shaders/blur_pass_prelude.frag");
const BLUR_DOWN: &str = include_str!("shaders/blur_down.frag");
const BLUR_DOWN_MAIN: &str = include_str!("shaders/blur_down_main.frag");
const BLUR_UP: &str = include_str!("shaders/blur_up.frag");
const BLUR_UP_MAIN: &str = include_str!("shaders/blur_up_main.frag");
const BLUR_PRELUDE: &str = include_str!("shaders/blur_prelude.frag");
const BLUR_FINISH_DEFAULT: &str = include_str!("shaders/blur_finish_default.frag");
const BLUR_MAIN: &str = include_str!("shaders/blur_main.frag");

const BORDER: &str = include_str!("shaders/border.frag");
const SHADOW: &str = include_str!("shaders/shadow.frag");

/// Built-in include files, addressed as `#include "poly/<name>"`.
const BUILTIN_INCLUDES: &[(&str, &str)] = &[
    (
        "poly/rounding.glsl",
        include_str!("shaders/include/rounding.glsl"),
    ),
    (
        "poly/noise.glsl",
        include_str!("shaders/include/noise.glsl"),
    ),
    (
        "poly/dither.glsl",
        include_str!("shaders/include/dither.glsl"),
    ),
    (
        "poly/color.glsl",
        include_str!("shaders/include/color.glsl"),
    ),
];

// --- Uniform registries -----------------------------------------------------

const WINDOW_UNIFORMS: &[(&str, UniformType)] = &[
    ("poly_scale", UniformType::_1f),
    ("geo_size", UniformType::_2f),
    ("corner_radius", UniformType::_4f),
    ("input_to_geo", UniformType::Matrix3x3),
    ("poly_card_clip", UniformType::_4f),
    ("u_time", UniformType::_1f),
    ("u_size", UniformType::_2f),
    ("u_scale", UniformType::_1f),
    ("u_alpha", UniformType::_1f),
    ("u_radius", UniformType::_1f),
    ("u_border_width", UniformType::_1f),
    ("u_border_color", UniformType::_4f),
    ("u_focused", UniformType::_1f),
    ("u_window_id", UniformType::_1f),
    ("u_cursor", UniformType::_2f),
    ("noise", UniformType::_1f),
];

const BORDER_UNIFORMS: &[(&str, UniformType)] = &[
    ("poly_scale", UniformType::_1f),
    ("border_color", UniformType::_4f),
    ("border_width", UniformType::_1f),
    ("corner_radius", UniformType::_4f),
    ("poly_card_clip", UniformType::_4f),
];

const SHADOW_UNIFORMS: &[(&str, UniformType)] = &[
    ("shadow_color", UniformType::_4f),
    ("sigma", UniformType::_1f),
    ("poly_scale", UniformType::_1f),
    ("window_offset", UniformType::_2f),
    ("window_size", UniformType::_2f),
    ("window_corner_radius", UniformType::_4f),
    ("shadow_offset", UniformType::_2f),
    ("spread", UniformType::_1f),
];

fn uniform_names(list: &[(&'static str, UniformType)]) -> Vec<UniformName<'static>> {
    return list
        .iter()
        .map(|(name, type_)| return UniformName::new(*name, *type_))
        .collect();
}

/// Build a `mat3` uniform value.
pub fn mat3_uniform(name: &'static str, matrix: Mat3) -> Uniform<'static> {
    return Uniform::new(
        name,
        UniformValue::Matrix3x3 {
            matrices: vec![matrix.to_cols_array()],
            transpose: false,
        },
    );
}

// --- Include expansion ------------------------------------------------------

/// A user shader after `#include` expansion and directive hoisting.
#[derive(Debug, Default, Clone)]
struct UserSource {
    /// `#version`-stripped code (the prelude provides the version).
    code: String,
    /// `#extension` lines, hoisted into the prelude.
    extensions: Vec<String>,
    /// Whether the user code references `u_time`.
    uses_time: bool,
    /// Whether the user code references `u_cursor`.
    uses_cursor: bool,
}

impl UserSource {
    fn from_raw(raw: &str) -> UserSource {
        let mut source = UserSource::default();
        for line in raw.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("#version") {
                // The prelude owns the `#version` line.
                continue;
            }
            if trimmed.starts_with("#extension") {
                source.extensions.push(trimmed.to_owned());
                continue;
            }
            source.code.push_str(line);
            source.code.push('\n');
        }
        source.uses_time = source.code.contains("u_time");
        source.uses_cursor = source.code.contains("u_cursor");
        return source;
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    return fs::metadata(path)
        .ok()
        .and_then(|meta| return meta.modified().ok());
}

/// Parse `#include "name"` / `#include <name>`; returns the requested name.
fn include_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("#include")?;
    let rest = rest.trim_start();
    let (open, close) = if rest.starts_with('"') {
        ('"', '"')
    } else if rest.starts_with('<') {
        ('<', '>')
    } else {
        return None;
    };
    let rest = rest.strip_prefix(open)?;
    let end = rest.find(close)?;
    return Some(rest[..end].to_owned());
}

/// Read `path` and expand its includes relative to the file's directory.
///
/// `files` collects every file that was read (with its mtime) so the hot
/// reload timer can notice changes to included files too.
fn expand_includes(
    path: &Path,
    depth: usize,
    stack: &mut Vec<PathBuf>,
    files: &mut HashMap<PathBuf, Option<SystemTime>>,
) -> Result<String, String> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(format!(
            "include depth exceeded {MAX_INCLUDE_DEPTH} at {}",
            path.display()
        ));
    }
    let canonical = path
        .canonicalize()
        .unwrap_or_else(|_| return path.to_path_buf());
    if stack.contains(&canonical) {
        return Err(format!("include cycle at {}", canonical.display()));
    }
    let source = fs::read_to_string(&canonical)
        .map_err(|err| return format!("cannot read {}: {err}", canonical.display()))?;
    files.insert(canonical.clone(), mtime(&canonical));

    let directory = canonical
        .parent()
        .map(|parent| return parent.to_path_buf())
        .unwrap_or_else(|| return PathBuf::from("."));

    stack.push(canonical);
    let mut expanded = String::with_capacity(source.len());
    for line in source.lines() {
        let Some(name) = include_name(line) else {
            expanded.push_str(line);
            expanded.push('\n');
            continue;
        };
        match BUILTIN_INCLUDES
            .iter()
            .find(|(builtin, _)| return *builtin == name)
        {
            Some((_, source)) => {
                expanded.push_str(source);
                expanded.push('\n');
            }
            None => {
                let included = expand_includes(&directory.join(&name), depth + 1, stack, files)?;
                expanded.push_str(&included);
                expanded.push('\n');
            }
        }
    }
    stack.pop();
    return Ok(expanded);
}

/// Assemble a fragment shader from the prelude, the user entry point and the
/// main body, hoisting user `#extension` directives right after `#version`.
fn assemble(prelude: &str, source: &UserSource, main: &str, rounding: bool) -> String {
    let mut prelude = prelude.to_owned();
    if !source.extensions.is_empty() {
        prelude = prelude.replacen(
            "//_DEFINES_",
            &format!("//_DEFINES_\n{}", source.extensions.join("\n")),
            1,
        );
    }

    let mut shader = String::with_capacity(prelude.len() + source.code.len() + main.len() + 512);
    shader.push_str(&prelude);
    shader.push_str(&source.code);
    shader.push_str(main);
    // Always appended: `rounding_alpha.frag` carries the same include guard as
    // `#include "poly/rounding.glsl"`, so a user include simply wins.
    if rounding {
        shader.push_str(ROUNDING_ALPHA);
    }
    return shader;
}

// --- Compilation ------------------------------------------------------------

fn compile_window(
    renderer: &mut GlesRenderer,
    source: &UserSource,
) -> Result<GlesTexProgram, GlesError> {
    let shader = assemble(WINDOW_PRELUDE, source, WINDOW_MAIN, true);
    return renderer.compile_custom_texture_shader(shader, &uniform_names(WINDOW_UNIFORMS));
}

fn compile_blur_finish(
    renderer: &mut GlesRenderer,
    source: &UserSource,
) -> Result<GlesTexProgram, GlesError> {
    let shader = assemble(BLUR_PRELUDE, source, BLUR_MAIN, true);
    return renderer.compile_custom_texture_shader(shader, &uniform_names(WINDOW_UNIFORMS));
}

fn compile_blur_pass(prelude: &str, source: &UserSource, main: &str) -> String {
    return assemble(prelude, source, main, false);
}

/// The compiled programs. Rebuilt whenever the configuration generation or a
/// shader file changes; individual programs that fail to compile keep their
/// previous (working) version.
pub struct Shaders {
    /// Window programs by shader path; the empty key is the built-in default.
    windows: HashMap<String, Option<GlesTexProgram>>,
    /// Whether each window program references `u_time`.
    window_time: HashMap<String, bool>,
    /// Whether each window program references `u_cursor`.
    window_cursor: HashMap<String, bool>,
    /// The Kawase/blur program set.
    blur: Option<BlurProgram>,
    border: Option<GlesPixelProgram>,
    shadow: Option<GlesPixelProgram>,
    /// Generation this set was prepared for.
    generation: u64,
    /// Whether anything was compiled at least once.
    prepared: bool,
    /// Messages the UI should show as Toasts.
    errors: Vec<String>,
    /// Every file read while compiling, with the mtime we saw.
    files: HashMap<PathBuf, Option<SystemTime>>,
    /// A watched file changed; recompile on the next frame.
    dirty: bool,
    /// Whether a "using `u_time`" warning was already emitted.
    warned_time: HashSet<String>,
    /// Bumped whenever programs are actually (re)compiled, so that per-window
    /// decoration caches repaint after a shader edit.
    epoch: u64,
}

impl Default for Shaders {
    fn default() -> Self {
        return Self::new();
    }
}

impl Shaders {
    /// An empty set; programs are compiled by [`Shaders::prepare`].
    pub fn new() -> Shaders {
        return Shaders {
            windows: HashMap::new(),
            window_time: HashMap::new(),
            window_cursor: HashMap::new(),
            blur: None,
            border: None,
            shadow: None,
            generation: 0,
            prepared: false,
            errors: Vec::new(),
            files: HashMap::new(),
            dirty: false,
            warned_time: HashSet::new(),
            epoch: 0,
        };
    }

    /// Compile (or recompile) programs for `config`.
    pub fn prepare(&mut self, config: &Config, generation: u64, renderer: &mut GlesRenderer) {
        if self.prepared && self.generation == generation && !self.dirty {
            return;
        }
        self.prepared = true;
        self.generation = generation;
        self.dirty = false;
        self.errors.clear();
        self.files.clear();
        self.epoch = self.epoch.wrapping_add(1);

        self.prepare_windows(config, renderer);
        self.prepare_blur(config, renderer);
        self.prepare_decorations(renderer);

        if self.uses_time() {
            for path in self.window_time.keys() {
                if self.window_time.get(path).copied().unwrap_or(false)
                    && self.warned_time.insert(path.clone())
                {
                    warn!(
                        "shader {path:?} references u_time: the damage optimization is disabled \
                         and the output redraws continuously"
                    );
                }
            }
        }
    }

    fn prepare_windows(&mut self, config: &Config, renderer: &mut GlesRenderer) {
        // The built-in program is compiled once; if it somehow fails there is
        // nothing to fall back to, so remember the failure and move on.
        if !self.windows.contains_key("") {
            match compile_window(renderer, &UserSource::from_raw(WINDOW_DEFAULT)) {
                Ok(program) => {
                    self.windows.insert(String::new(), Some(program));
                    self.window_time.insert(String::new(), false);
                    self.window_cursor.insert(String::new(), false);
                }
                Err(err) => {
                    self.errors
                        .push(format!("built-in window shader failed to compile: {err}"));
                    self.windows.insert(String::new(), None);
                }
            }
        }

        let mut wanted: HashSet<String> = HashSet::new();
        if let Some(path) = &config.general.window_shader {
            wanted.insert(path.clone());
        }
        for rule in &config.window_rules {
            if let Some(path) = &rule.window_shader {
                wanted.insert(path.clone());
            }
        }
        self.windows
            .retain(|key, _| return key.is_empty() || wanted.contains(key));
        self.window_time
            .retain(|key, _| return key.is_empty() || wanted.contains(key));
        self.window_cursor
            .retain(|key, _| return key.is_empty() || wanted.contains(key));

        for path in wanted {
            let mut stack = Vec::new();
            let raw = match expand_includes(Path::new(&path), 0, &mut stack, &mut self.files) {
                Ok(raw) => raw,
                Err(err) => {
                    self.errors.push(format!("window shader {path}: {err}"));
                    self.windows.entry(path.clone()).or_insert(None);
                    continue;
                }
            };
            let source = UserSource::from_raw(&raw);
            match compile_window(renderer, &source) {
                Ok(program) => {
                    self.windows.insert(path.clone(), Some(program));
                    self.window_cursor.insert(path.clone(), source.uses_cursor);
                    self.window_time.insert(path, source.uses_time);
                }
                Err(err) => {
                    self.errors
                        .push(format!("window shader {path} failed to compile: {err}"));
                    // Keep a previously working program, if there is one.
                    self.windows.entry(path.clone()).or_insert(None);
                    self.window_cursor.entry(path.clone()).or_insert(false);
                    self.window_time.entry(path).or_insert(false);
                }
            }
        }
    }

    fn prepare_blur(&mut self, config: &Config, renderer: &mut GlesRenderer) {
        let shader = &config.general.blur.shader;
        // Error messages name the file, not just the pass: a user with three
        // custom passes otherwise cannot tell which one broke.
        let label = |name: &str, path: &Option<String>| {
            return match path {
                Some(path) => format!("blur {name} shader {path}"),
                None => format!("built-in blur {name} shader"),
            };
        };

        let load = |path: &Option<String>,
                    files: &mut HashMap<PathBuf, Option<SystemTime>>|
         -> Result<Option<UserSource>, String> {
            let Some(path) = path else {
                return Ok(None);
            };
            let mut stack = Vec::new();
            let raw = expand_includes(Path::new(path), 0, &mut stack, files)?;
            return Ok(Some(UserSource::from_raw(&raw)));
        };

        // Errors here are per pass: a bad `down` must not lose a good `up`.
        let read_error = |self_: &mut Self, name: &str, path: &Option<String>| {
            return match load(path, &mut self_.files) {
                Ok(source) => source,
                Err(err) => {
                    self_.errors.push(format!("{}: {err}", label(name, path)));
                    None
                }
            };
        };
        let down = read_error(self, "downsample", &shader.down);
        let up = read_error(self, "upsample", &shader.up);
        let finish = read_error(self, "finish", &shader.finish);

        let uses_time = down.as_ref().is_some_and(|source| return source.uses_time)
            || up.as_ref().is_some_and(|source| return source.uses_time)
            || finish
                .as_ref()
                .is_some_and(|source| return source.uses_time);

        let builtin_down = UserSource::from_raw(BLUR_DOWN);
        let builtin_up = UserSource::from_raw(BLUR_UP);
        let down_source = down.as_ref().unwrap_or(&builtin_down);
        let up_source = up.as_ref().unwrap_or(&builtin_up);

        let down_src = compile_blur_pass(BLUR_PASS_PRELUDE, down_source, BLUR_DOWN_MAIN);
        let up_src = compile_blur_pass(BLUR_PASS_PRELUDE, up_source, BLUR_UP_MAIN);

        let builtin_finish = UserSource::from_raw(BLUR_FINISH_DEFAULT);
        let finish_program = {
            let source = finish.as_ref().unwrap_or(&builtin_finish);
            match compile_blur_finish(renderer, source) {
                Ok(program) => Some(program),
                Err(err) => {
                    self.errors.push(format!(
                        "{} failed to compile: {err}",
                        label("finish", &shader.finish)
                    ));
                    // A reload must not throw away a working program because
                    // of one bad edit: keep what is on screen and let the user
                    // fix the file. Only the very first compile falls back.
                    if self.blur.is_some() {
                        return;
                    }
                    match compile_blur_finish(renderer, &builtin_finish) {
                        Ok(program) => Some(program),
                        Err(err) => {
                            self.errors
                                .push(format!("built-in blur finish shader failed: {err}"));
                            None
                        }
                    }
                }
            }
        };

        let Some(finish_program) = finish_program else {
            return;
        };

        match BlurProgram::compile(
            renderer,
            &down_src,
            &up_src,
            finish_program.clone(),
            uses_time,
        ) {
            Ok(program) => self.blur = Some(program),
            Err(err) => {
                self.errors
                    .push(format!("blur passes failed to compile: {err}"));
                if self.blur.is_none() {
                    // Try the fully built-in set so blur keeps working.
                    let down_src = compile_blur_pass(
                        BLUR_PASS_PRELUDE,
                        &UserSource::from_raw(BLUR_DOWN),
                        BLUR_DOWN_MAIN,
                    );
                    let up_src = compile_blur_pass(
                        BLUR_PASS_PRELUDE,
                        &UserSource::from_raw(BLUR_UP),
                        BLUR_UP_MAIN,
                    );
                    if let Ok(program) =
                        BlurProgram::compile(renderer, &down_src, &up_src, finish_program, false)
                    {
                        self.blur = Some(program);
                    }
                }
            }
        }
    }

    fn prepare_decorations(&mut self, renderer: &mut GlesRenderer) {
        if self.border.is_none() {
            // `concat!` cannot join a `const`, and `border.frag` calls
            // `poly_rounding_alpha()`, so append the shared helper.
            let source = format!("{BORDER}\n{ROUNDING_ALPHA}");
            match renderer.compile_custom_pixel_shader(&source, &uniform_names(BORDER_UNIFORMS)) {
                Ok(program) => self.border = Some(program),
                Err(err) => self
                    .errors
                    .push(format!("border shader failed to compile: {err}")),
            }
        }
        if self.shadow.is_none() {
            let source = format!("{SHADOW}\n{ROUNDING_ALPHA}");
            match renderer.compile_custom_pixel_shader(&source, &uniform_names(SHADOW_UNIFORMS)) {
                Ok(program) => self.shadow = Some(program),
                Err(err) => self
                    .errors
                    .push(format!("shadow shader failed to compile: {err}")),
            }
        }
    }

    /// The window program for `path`, falling back to the built-in default.
    pub fn window_program(&self, path: Option<&str>) -> Option<&GlesTexProgram> {
        let program = path
            .and_then(|path| return self.windows.get(path))
            .and_then(|program| return program.as_ref());
        return program.or_else(|| {
            return self
                .windows
                .get("")
                .and_then(|program| return program.as_ref());
        });
    }

    /// The blur program set, if it compiled.
    pub fn blur(&self) -> Option<&BlurProgram> {
        return self.blur.as_ref();
    }

    /// The border program.
    pub fn border(&self) -> Option<&GlesPixelProgram> {
        return self.border.as_ref();
    }

    /// The shadow program.
    pub fn shadow(&self) -> Option<&GlesPixelProgram> {
        return self.shadow.as_ref();
    }

    /// Whether the given window program reads `u_time`.
    pub fn window_uses_time(&self, path: Option<&str>) -> bool {
        let path = path.unwrap_or("");
        return self.window_time.get(path).copied().unwrap_or(false);
    }

    /// Whether the given window program reads `u_cursor`.
    pub fn window_uses_cursor(&self, path: Option<&str>) -> bool {
        let path = path.unwrap_or("");
        return self.window_cursor.get(path).copied().unwrap_or(false);
    }

    /// Whether the blur program reads `u_time`.
    pub fn blur_uses_time(&self) -> bool {
        return self
            .blur
            .as_ref()
            .is_some_and(|blur| return blur.uses_time());
    }

    /// Whether any active program reads `u_time`, which disables the damage
    /// optimization (`docs/plan.md` §10.5).
    pub fn uses_time(&self) -> bool {
        return self.window_time.values().any(|uses| return *uses) || self.blur_uses_time();
    }

    /// Bumped every time programs are recompiled.
    pub fn epoch(&self) -> u64 {
        return self.epoch;
    }

    /// Take the compile errors since the last call (for Toasts).
    pub fn take_errors(&mut self) -> Vec<String> {
        return std::mem::take(&mut self.errors);
    }

    /// Re-stat every shader file; sets the dirty flag when one changed.
    pub fn poll_files(&mut self) -> bool {
        if self.dirty {
            return true;
        }
        for (path, stamp) in &self.files {
            if mtime(path) != *stamp {
                self.dirty = true;
                return true;
            }
        }
        return false;
    }

    /// Force a recompile on the next frame (configuration reload).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}
