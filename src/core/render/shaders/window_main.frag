// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/clipped_surface.frag

// Main body of the window shader. Prepended by `window_prelude.frag`, followed
// by the (optional) user `window_shade()` implementation and
// `rounding_alpha.frag`.
void main() {
    vec3 coords_geo = input_to_geo * vec3(v_coords, 1.0);

    // Keep the window inside its workspace card: a floating window may stick
    // out of the workspace, and during the overview that would paint over the
    // gap or the neighbouring card.
    if (coords_geo.x < poly_card_clip.x || coords_geo.y < poly_card_clip.y ||
            poly_card_clip.z < coords_geo.x || poly_card_clip.w < coords_geo.y) {
        discard;
    }

    vec2 uv = clamp(coords_geo.xy, 0.0, 1.0);

    // Sample the window texture.
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    // User (or default) shading: tinting, gradients, `u_time` effects.
    color = window_shade(uv, color);

    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x || coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        // Clip outside the window geometry.
        color = vec4(0.0);
    } else {
        // Apply corner rounding inside the geometry.
        color = color * poly_rounding_alpha(coords_geo.xy * geo_size, geo_size, corner_radius);
    }

    // Apply the element alpha, which already carries window opacity times the
    // open/close animation alpha.
    color = color * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
