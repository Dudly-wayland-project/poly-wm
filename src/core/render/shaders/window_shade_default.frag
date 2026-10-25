// Default `window_shade()` used when no custom window shader is configured:
// the window content is drawn as-is.
vec4 window_shade(vec2 uv, vec4 color) {
    return color;
}
