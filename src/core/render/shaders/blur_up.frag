// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/blur_up.frag

// Dual Kawase upsample pass.
vec4 upsample(vec2 uv, vec2 half_pixel) {
    vec2 o = half_pixel * offset;

    vec4 sum = vec4(0.0);

    // Four edge centers.
    sum += texture2D(tex, uv + vec2(-o.x * 2.0, 0.0));
    sum += texture2D(tex, uv + vec2( o.x * 2.0, 0.0));
    sum += texture2D(tex, uv + vec2(0.0, -o.y * 2.0));
    sum += texture2D(tex, uv + vec2(0.0,  o.y * 2.0));

    // Four diagonal corners.
    sum += texture2D(tex, uv + vec2(-o.x,  o.y)) * 2.0;
    sum += texture2D(tex, uv + vec2( o.x,  o.y)) * 2.0;
    sum += texture2D(tex, uv + vec2(-o.x, -o.y)) * 2.0;
    sum += texture2D(tex, uv + vec2( o.x, -o.y)) * 2.0;

    return sum / 12.0;
}
