// Main body of the downsample pass; `downsample()` comes from the built-in or
// the user's shader file.
void main() {
    gl_FragColor = downsample(v_coords, half_pixel);
}
