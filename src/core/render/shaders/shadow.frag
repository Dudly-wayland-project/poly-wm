// Ported from niri (GPL-3.0-or-later): src/render_helpers/shaders/shadow.frag

// Analytic gaussian drop shadow behind a rounded window.
//
// The element rectangle is the window rectangle padded by `ceil(3 * sigma)`
// plus the shadow offset; `window_offset`/`window_size` describe where the
// window sits inside it, so the shadow of the *window box* is drawn and the
// window itself is cut out (the content is composited on top anyway, but this
// keeps translucent windows from being darkened twice).
precision highp float;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

// Injected by Poly.
uniform vec4 shadow_color;
uniform float sigma;
uniform float spread;
uniform float poly_scale;
uniform vec2 window_offset;
uniform vec2 window_size;
uniform vec4 window_corner_radius;
uniform vec2 shadow_offset;

// Based on: https://madebyevan.com/shaders/fast-rounded-rectangle-shadows/
//
// License: CC0 (http://creativecommons.org/publicdomain/zero/1.0/)

// A standard gaussian function, used for weighting samples.
float gaussian(float x, float sigma) {
    const float pi = 3.141592653589793;
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (sqrt(2.0 * pi) * sigma);
}

// This approximates the error function, needed for the gaussian integral.
vec2 erf(vec2 x) {
    vec2 s = sign(x), a = abs(x);
    x = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    x *= x;
    return s - s / (x * x);
}

// Return the blurred mask along the x dimension.
float roundedBoxShadowX(float x, float y, float sigma, float corner, vec2 halfSize) {
    float delta = min(halfSize.y - corner - abs(y), 0.0);
    float curved = halfSize.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    vec2 integral = 0.5 + 0.5 * erf((x + vec2(-curved, curved)) * (sqrt(0.5) / sigma));
    return integral.y - integral.x;
}

// Return the mask for the shadow of a box from lower to upper.
float roundedBoxShadow(vec2 lower, vec2 upper, vec2 point, float sigma, float corner) {
    // Center everything to make the math easier.
    vec2 center = (lower + upper) * 0.5;
    vec2 halfSize = (upper - lower) * 0.5;
    point -= center;

    // The signal is only non-zero in a limited range, so don't waste samples.
    float low = point.y - halfSize.y;
    float high = point.y + halfSize.y;
    float start = clamp(-3.0 * sigma, low, high);
    float end = clamp(3.0 * sigma, low, high);

    // Accumulate samples (we can get away with surprisingly few samples).
    float step = (end - start) / 4.0;
    float y = start + step * 0.5;
    float value = 0.0;
    for (int i = 0; i < 4; i++) {
        value += roundedBoxShadowX(point.x, point.y - y, sigma, corner, halfSize) * gaussian(y, sigma) * step;
        y += step;
    }

    return value;
}

float poly_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius);

void main() {
    vec2 coords = v_coords * size;

    // Shadow of the window rectangle, displaced by `shadow_offset` and grown
    // by `spread` (CSS `box-shadow` semantics: negative spread shrinks it).
    vec2 win_min = window_offset;
    vec2 win_max = window_offset + window_size;
    vec2 spread_vec = vec2(spread);
    vec2 shadow_min = win_min + shadow_offset - spread_vec;
    vec2 shadow_max = win_max + shadow_offset + spread_vec;
    // A negative spread can shrink the silhouette below zero; keep the math
    // well-defined (the result is invisible either way).
    vec2 shadow_size = max(shadow_max - shadow_min, vec2(0.001));
    shadow_max = shadow_min + shadow_size;
    vec4 shadow_radius = max(window_corner_radius + vec4(spread), vec4(0.0));

    float shadow_value;
    if (sigma < 0.5) {
        // With low enough sigma the shadow degenerates into a hard-edged
        // (offset) rounded rectangle.
        vec2 local = coords - shadow_min;
        shadow_value = poly_rounding_alpha(local, shadow_size, shadow_radius);
    } else {
        shadow_value = roundedBoxShadow(
            shadow_min,
            shadow_max,
            coords,
            sigma,
            // FIXME: figure out how to blur with different corner radii.
            shadow_radius.x
        );
    }

    // Cut out the window itself.
    vec2 win_coords = coords - win_min;
    if (0.0 <= win_coords.x && win_coords.x <= window_size.x
            && 0.0 <= win_coords.y && win_coords.y <= window_size.y) {
        shadow_value *= 1.0 - poly_rounding_alpha(win_coords, window_size, window_corner_radius);
    }

    // Premultiplied output.
    float a = shadow_color.a * shadow_value * alpha;
    vec4 color = vec4(shadow_color.rgb * a, a);

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
