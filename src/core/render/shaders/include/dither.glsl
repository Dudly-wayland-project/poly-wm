// Built-in include: `#include "poly/dither.glsl"`.
//
// Ordered dithering to hide banding in low-contrast gradients and blurred
// areas. All functions operate on premultiplied colors.
#ifndef POLY_DITHER_GLSL
#define POLY_DITHER_GLSL

// 4x4 ordered dither threshold in `[-0.5, 0.5]`.
float poly_bayer4(vec2 coords) {
    vec2 p = mod(floor(coords), 4.0);
    float index = p.x + p.y * 4.0;
    // The classic 4x4 Bayer matrix, flattened row by row.
    float m[16];
    m[0] = 0.0;  m[1] = 8.0;  m[2] = 2.0;  m[3] = 10.0;
    m[4] = 12.0; m[5] = 4.0;  m[6] = 14.0; m[7] = 6.0;
    m[8] = 3.0;  m[9] = 11.0; m[10] = 1.0; m[11] = 9.0;
    m[12] = 15.0; m[13] = 7.0; m[14] = 13.0; m[15] = 5.0;

    float value = 0.0;
    for (int i = 0; i < 16; i++) {
        if (float(i) == index) {
            value = m[i];
        }
    }
    return value / 16.0 - 0.5;
}

// Add `amount` of ordered dithering to `color`, scaled by its alpha.
vec3 poly_dither(vec3 color, float amount, vec2 coords) {
    return color + poly_bayer4(coords) * amount * max(color.r, max(color.g, color.b));
}
#endif // POLY_DITHER_GLSL
