// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/postprocess.frag

// Sin-less white noise by David Hoskins (MIT License).
// https://www.shadertoy.com/view/4djSRW
//
// `sin()`-based hashes (`fract(sin(dot(p, ...)) * 43758.5453)`) are unreliable
// on some drivers: with `highp` fragment precision the argument to `sin()`
// grows large and the result collapses to a constant (observed with llvmpipe,
// which turned the whole blur into black). This variant only uses `fract()`.
//
// Guarded because a custom `finish()` may `#include "poly/noise.glsl"`, which
// defines the very same function (`docs/plan.md` §10.3).
#ifndef POLY_NOISE_GLSL
#define POLY_NOISE_GLSL
float poly_hash12(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}
#endif // POLY_NOISE_GLSL

// Main body of the blur composite shader: samples the blurred backdrop as a
// texture, runs the user (or default) `finish()`, adds dithering noise, clips
// to the window's rounded rectangle and applies the element alpha.
void main() {
    vec3 coords_geo = input_to_geo * vec3(v_coords, 1.0);

    // Premultiplied color of the blurred backdrop.
    vec4 color = finish(v_coords, texture2D(tex, v_coords));

    // Dither to break up banding in low-contrast blurred areas.
    float poly_hash = poly_hash12(gl_FragCoord.xy);
    color.rgb += (poly_hash - 0.5) * noise * color.a;
    color.rgb = max(color.rgb, vec3(0.0));

    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x || coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        color = vec4(0.0);
    } else {
        color = color * poly_rounding_alpha(coords_geo.xy * geo_size, geo_size, corner_radius);
    }

    color = color * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
