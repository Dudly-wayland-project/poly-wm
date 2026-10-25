// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/blur_down.frag

// Dual Kawase downsample pass.
vec4 downsample(vec2 uv, vec2 half_pixel) {
    vec2 o = half_pixel * offset;

    vec4 sum = texture2D(tex, uv) * 4.0;
    sum += texture2D(tex, uv + vec2(-o.x, -o.y));
    sum += texture2D(tex, uv + vec2( o.x, -o.y));
    sum += texture2D(tex, uv + vec2(-o.x,  o.y));
    sum += texture2D(tex, uv + vec2( o.x,  o.y));

    return sum / 8.0;
}
