// Default `finish()` used when no custom blur shader is configured: the
// blurred backdrop is composited unchanged.
vec4 finish(vec2 uv, vec4 blurred) {
    return blurred;
}
