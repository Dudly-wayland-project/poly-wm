// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/postprocess.frag

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
// Blurred-texture coordinates -> `[0, 1]` window geometry coordinates.
uniform mat3 input_to_geo;
// Dithering amount that hides banding in the blurred result, `0.0 ..= 1.0`.
uniform float noise;

// Seconds since the compositor started. Referencing `u_time` disables the
// damage optimization (the compositor then redraws continuously).
uniform float u_time;
uniform vec2 u_size;
uniform float u_scale;

float poly_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius);

// The user entry point for custom blur shaders: post-process the blurred
// backdrop before it is clipped to the window shape. `uv` is in `[0, 1]` over
// the blurred region. The default returns the color unchanged.
vec4 finish(vec2 uv, vec4 blurred);
