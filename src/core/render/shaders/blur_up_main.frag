// Main body of the upsample pass; `upsample()` comes from the built-in or the
// user's shader file.
void main() {
    gl_FragColor = upsample(v_coords, half_pixel);
}
