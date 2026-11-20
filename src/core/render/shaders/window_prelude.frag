// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/clipped_surface.frag

#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

// `alpha` is the total element alpha: window opacity x open/close animation
// alpha. The main body applies it after `window_shade()`.
uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// --- Injected by Poly --------------------------------------------------------

// Output scale (physical pixels per logical pixel).
uniform float poly_scale;
// Window size in logical pixels.
uniform vec2 geo_size;
// Per-corner radii `(top_left, top_right, bottom_right, bottom_left)`, logical.
uniform vec4 corner_radius;
// Window texture coordinates -> `[0, 1]` window geometry coordinates.
uniform mat3 input_to_geo;
// Workspace card in window geometry space (`x0, y0, x1, y1`). Fragments
// outside are discarded, which keeps floating windows inside their workspace
// during a slide. `(0, 0, 1, 1)` disables the clip.
uniform vec4 poly_card_clip;

// --- Stable aliases for custom shaders --------------------------------------

// Seconds since the compositor started. Referencing `u_time` disables the
// damage optimization: the compositor then redraws continuously, and a warning
// is logged once per shader.
uniform float u_time;
// Window size in logical pixels (same value as `geo_size`).
uniform vec2 u_size;
// Output scale (same value as `poly_scale`).
uniform float u_scale;
// Effective window alpha (same value as `alpha`).
uniform float u_alpha;
// Uniform corner radius in logical pixels (`corner_radius.x`).
uniform float u_radius;
// Border width in logical pixels; 0 when the window has no border.
uniform float u_border_width;
// Border color `(r, g, b, a)`, straight (not premultiplied). Its alpha is 0
// when the window has no border.
uniform vec4 u_border_color;
// 1.0 when this window currently holds keyboard focus, else 0.0.
uniform float u_focused;
// Small numeric window id, stable for the lifetime of the window.
uniform float u_window_id;
// Cursor position in window geometry coordinates (logical pixels); both
// components are negative when the pointer is outside the window.
uniform vec2 u_cursor;

// Texture coordinates are called `v_texcoord` in the public contract, while
// the built-in body below uses the historical `v_coords` name.
#define v_texcoord v_coords

// Anti-aliased coverage of a rounded rectangle (`rounding_alpha.frag`).
float poly_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius);

// The user entry point: transform the sampled window
// color, which is premultiplied. `uv` is in `[0, 1]` over the window geometry.
vec4 window_shade(vec2 uv, vec4 color);
