// Built-in include: `#include "poly/noise.glsl"`.
//
// Cheap hash-based noise helpers for shader effects.
#ifndef POLY_NOISE_GLSL
#define POLY_NOISE_GLSL

// Sin-less white noise by David Hoskins (MIT License).
// https://www.shadertoy.com/view/4djSRW
//
// Do **not** use `fract(sin(dot(p, ...)) * 43758.5453)` here: with `highp`
// fragment precision the `sin()` argument becomes large and the hash collapses
// to a constant on some drivers (llvmpipe renders it as black), which turns
// every effect built on top of it into a flat color.
float poly_hash12(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

// 1D value noise in `[0, 1]`.
float poly_noise(vec2 p) {
    return poly_hash12(p);
}

// Two-dimensional value noise in `[0, 1]`, smooth enough for dithering.
float poly_noise2(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    vec2 u = f * f * (3.0 - 2.0 * f);
    float a = poly_noise(i);
    float b = poly_noise(i + vec2(1.0, 0.0));
    float c = poly_noise(i + vec2(0.0, 1.0));
    float d = poly_noise(i + vec2(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}
#endif // POLY_NOISE_GLSL
