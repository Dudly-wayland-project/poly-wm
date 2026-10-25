// Built-in include: `#include "poly/color.glsl"`.
//
// Color-space helpers for window shaders (grayscale, saturation, tinting).
// All functions operate on premultiplied colors unless noted otherwise.
#ifndef POLY_COLOR_GLSL
#define POLY_COLOR_GLSL

// Rec. 709 luminance of a straight (non-premultiplied) color.
float poly_luminance(vec3 color) {
    return dot(color, vec3(0.2126, 0.7152, 0.0722));
}

// Scale saturation of a premultiplied color; `0.0` is grayscale.
vec4 poly_saturate(vec4 color, float amount) {
    float luma = poly_luminance(color.rgb);
    return vec4(mix(vec3(luma), color.rgb, amount), color.a);
}

// Mix `color` towards `tint` (both premultiplied) by `amount`.
vec4 poly_tint(vec4 color, vec4 tint, float amount) {
    return mix(color, tint * color.a, amount);
}

// Apply a gamma curve to the straight color of a premultiplied color.
vec4 poly_gamma(vec4 color, float gamma) {
    vec3 straight = color.rgb / max(color.a, 0.0001);
    return vec4(pow(straight, vec3(gamma)) * color.a, color.a);
}
#endif // POLY_COLOR_GLSL
