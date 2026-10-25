// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/rounding_alpha.frag
#ifndef POLY_ROUNDING_ALPHA
#define POLY_ROUNDING_ALPHA

// Anti-aliased coverage of a rounded rectangle.
//
// `coords` is the fragment position in the same units as `size`, both in
// logical pixels; `corner_radius` is `(top_left, top_right, bottom_right,
// bottom_left)`. Uses `poly_scale` so the smoothstep spans one physical pixel.
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

    // Manual smoothstep() between radius - half_px and radius + half_px to avoid
    // a division in clamp().
    float t = clamp((dist - radius) * poly_scale + 0.5, 0.0, 1.0);
    return 1.0 - t * t * (3.0 - 2.0 * t);
}
#endif // POLY_ROUNDING_ALPHA
