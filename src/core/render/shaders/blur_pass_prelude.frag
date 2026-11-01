// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/blur_down.frag, blur_up.frag

#version 100

precision highp float;

varying vec2 v_coords;

uniform sampler2D tex;
uniform vec2 half_pixel;
uniform float offset;

// Seconds since the compositor started; referenced only by custom passes.
uniform float u_time;

// The entry points a custom blur pass may override.
// `uv` is in `[0, 1]` over the source texture, `half_pixel` is `0.5 / size`.
vec4 downsample(vec2 uv, vec2 half_pixel);
vec4 upsample(vec2 uv, vec2 half_pixel);
