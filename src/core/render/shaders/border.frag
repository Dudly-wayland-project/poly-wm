// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/border.frag

// Window border. The element area is the window rectangle expanded by
// `border_width` on every side, so `size` (built-in uniform, logical pixels)
// is the outer rectangle and the window starts at `(border_width, border_width)`.
precision highp float;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

// Injected by Poly.
uniform float poly_scale;
uniform vec4 border_color;
uniform float border_width;
uniform vec4 corner_radius;
// The workspace card in window geometry space; discard the border outside it
// so a floating window cannot draw a stray line over the neighbouring workspace.
uniform vec4 poly_card_clip;

float poly_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius);

void main() {
    vec2 coords = v_coords * size;
    vec2 window_size = max(size - vec2(border_width * 2.0), vec2(1.0));
    vec2 window_coords = (coords - vec2(border_width)) / window_size;
    if (window_coords.x < poly_card_clip.x || window_coords.y < poly_card_clip.y ||
            poly_card_clip.z < window_coords.x || poly_card_clip.w < window_coords.y) {
        discard;
    }

    // Outer edge, including the anti-aliased rounded corners.
    float outer = poly_rounding_alpha(coords, size, corner_radius);

    // Cut out the window area (inner rounded rectangle).
    vec2 inner_coords = coords - vec2(border_width);
    vec2 inner_size = size - vec2(border_width * 2.0);
    float inner = 0.0;
    if (0.0 <= inner_coords.x && inner_coords.x <= inner_size.x
            && 0.0 <= inner_coords.y && inner_coords.y <= inner_size.y) {
        vec4 inner_radius = max(corner_radius - vec4(border_width), 0.0);
        inner = poly_rounding_alpha(inner_coords, inner_size, inner_radius);
    }

    // Premultiplied output (GL blending is ONE, ONE_MINUS_SRC_ALPHA).
    float a = border_color.a * (outer - inner) * alpha;
    vec4 color = vec4(border_color.rgb * a, a);

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
