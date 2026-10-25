// Built-in include: `#include "poly/rounding.glsl"`.
//
// Anti-aliased coverage of a rounded rectangle, identical to the body appended
// by the compositor. Including this file is only necessary when a shader wants
// to call `poly_rounding_alpha()` itself.
#ifndef POLY_ROUNDING_ALPHA
#define POLY_ROUNDING_ALPHA

float poly_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius) {
    vec2 center;
    float radius;

    if (coords.x < corner_radius.x && coords.y < corner_radius.x) {
        radius = corner_radius.x;
        center = vec2(radius, radius);
    } else if (size.x - corner_radius.y < coords.x && coords.y < corner_radius.y) {
        radius = corner_radius.y;
        center = vec2(size.x - radius, radius);
    } else if (size.x - corner_radius.z < coords.x && size.y - corner_radius.z < coords.y) {
        radius = corner_radius.z;
        center = vec2(size.x - radius, size.y - radius);
    } else if (coords.x < corner_radius.w && size.y - corner_radius.w < coords.y) {
        radius = corner_radius.w;
        center = vec2(radius, size.y - radius);
    } else {
        return 1.0;
    }

    float dist = distance(coords, center);
    float t = clamp((dist - radius) * poly_scale + 0.5, 0.0, 1.0);
    return 1.0 - t * t * (3.0 - 2.0 * t);
}
#endif // POLY_ROUNDING_ALPHA
