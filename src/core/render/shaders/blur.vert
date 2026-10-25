// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/blur.vert

// Full-screen triangle strip; `vert` runs from 0 to 1 over the target.
#version 100

attribute vec2 vert;
varying vec2 v_coords;

void main() {
    v_coords = vert;
    // vert goes from 0 to 1; position must be from -1 to 1.
    vec2 position = vert * 2.0 - 1.0;
    gl_Position = vec4(position, 1.0, 1.0);
}
