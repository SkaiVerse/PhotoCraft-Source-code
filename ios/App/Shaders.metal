#include <metal_stdlib>
using namespace metal;

// PhotoCraft Phase 7: Dedicated Metal Fragment Shaders for Non-Fixed-Function Blend Modes
// All shaders operate on half4 / float4 in linear working space with premultiplied alpha.
// Strictly guarded against alpha == 0.0, division-by-zero, and NaN/Inf propagation.

constant half EDGE = 1e-4h;

// ---- Separable Blend Modes ----

inline half color_burn(half cb, half cs) {
    if (cb >= 1.0h - EDGE) { return 1.0h; }
    if (cs <= EDGE) { return 0.0h; }
    half d = max(cs, 1e-4h);
    return 1.0h - min((1.0h - cb) / d, 1.0h);
}

inline half color_dodge(half cb, half cs) {
    if (cb <= EDGE) { return 0.0h; }
    if (cs >= 1.0h - EDGE) { return 1.0h; }
    half d = max(1.0h - cs, 1e-4h);
    return min(cb / d, 1.0h);
}

inline half linear_burn(half cb, half cs) {
    return max(cb + cs - 1.0h, 0.0h);
}

inline half linear_dodge(half cb, half cs) {
    return min(cb + cs, 1.0h);
}

inline half hard_light(half cb, half cs) {
    if (cs <= 0.5h) {
        return 2.0h * cb * cs;
    } else {
        return 1.0h - 2.0h * (1.0h - cb) * (1.0h - cs);
    }
}

inline half overlay(half cb, half cs) {
    return hard_light(cs, cb);
}

// Photoshop / W3C Soft Light formula:
// D(cb) is sqrt(cb) above 0.25 and cubic below 0.25
inline half soft_light(half cb, half cs) {
    if (cs <= 0.5h) {
        return 2.0h * cb * cs + cb * cb * (1.0h - 2.0h * cs);
    } else {
        half b = max(cb, 0.0h);
        half d = (b <= 0.25h) ? (((16.0h * b - 12.0h) * b + 4.0h) * b) : sqrt(b);
        return 2.0h * cb * (1.0h - cs) + d * (2.0h * cs - 1.0h);
    }
}

inline half vivid_light(half cb, half cs) {
    if (cs <= 0.5h) {
        if (cs <= 0.0h) { return 0.0h; }
        return 1.0h - min((1.0h - cb) / max(2.0h * cs, 1e-4h), 1.0h);
    } else {
        if (cs >= 1.0h) { return 1.0h; }
        return min(cb / max(2.0h * (1.0h - cs), 1e-4h), 1.0h);
    }
}

inline half linear_light(half cb, half cs) {
    return clamp(cb + 2.0h * cs - 1.0h, 0.0h, 1.0h);
}

inline half pin_light(half cb, half cs) {
    if (cs <= 0.5h) {
        return min(cb, 2.0h * cs);
    } else {
        return max(cb, 2.0h * cs - 1.0h);
    }
}

inline half hard_mix(half cb, half cs) {
    half vl = (cs <= 0.5h)
        ? ((cb >= 1.0h - EDGE) ? 1.0h : ((cs <= 0.0h) ? 0.0h : (1.0h - min((1.0h - cb) / max(2.0h * cs, 1e-4h), 1.0h))))
        : ((cb <= EDGE) ? 0.0h : ((cs >= 1.0h) ? 1.0h : min(cb / max(2.0h * (1.0h - cs), 1e-4h), 1.0h)));
    return (vl >= 0.5h - 1e-4h) ? 1.0h : 0.0h;
}

// ---- HSL & Non-Separable Modes ----

inline half lum(half3 c) {
    return 0.3h * c.r + 0.59h * c.g + 0.11h * c.b;
}

inline half sat(half3 c) {
    return max(max(c.r, c.g), c.b) - min(min(c.r, c.g), c.b);
}

inline half3 clip_color(half3 c) {
    half l = lum(c);
    half n = min(min(c.r, c.g), c.b);
    half x = max(max(c.r, c.g), c.b);
    half3 out = c;
    if (n < 0.0h) {
        half d = l - n;
        if (l > 0.0h && d > 1e-4h) {
            out = l + (out - l) * l / d;
        } else {
            out = half3(l);
        }
    }
    if (x > 1.0h) {
        half d = x - l;
        if (l < 1.0h && d > 1e-4h) {
            out = l + (out - l) * (1.0h - l) / d;
        } else {
            out = half3(l);
        }
    }
    return clamp(out, half3(0.0h), half3(1.0h));
}

inline half3 set_lum(half3 c, half l) {
    return clip_color(c + (l - lum(c)));
}

inline half3 set_sat(half3 c, half s) {
    half mx = max(max(c.r, c.g), c.b);
    half mn = min(min(c.r, c.g), c.b);
    if (mx - mn > 1e-4h) {
        return (c - mn) * s / (mx - mn);
    }
    return half3(0.0h);
}

inline half3 blend_hsl(int mode, half3 cb, half3 cs) {
    switch (mode) {
        case 24: return set_lum(set_sat(cs, sat(cb)), lum(cb)); // Hue
        case 25: return set_lum(set_sat(cb, sat(cs)), lum(cb)); // Saturation
        case 26: return set_lum(cs, lum(cb));                  // Color
        case 27: return set_lum(cb, lum(cs));                  // Luminosity
        default: return cs;
    }
}

// Non-separable / separable dispatcher
inline half3 blend_mode_color(int mode, half3 cb, half3 cs) {
    switch (mode) {
        case 5:  return half3(color_burn(cb.r, cs.r), color_burn(cb.g, cs.g), color_burn(cb.b, cs.b));
        case 6:  return half3(linear_burn(cb.r, cs.r), linear_burn(cb.g, cs.g), linear_burn(cb.b, cs.b));
        case 10: return half3(color_dodge(cb.r, cs.r), color_dodge(cb.g, cs.g), color_dodge(cb.b, cs.b));
        case 11: return half3(linear_dodge(cb.r, cs.r), linear_dodge(cb.g, cs.g), linear_dodge(cb.b, cs.b));
        case 13: return half3(overlay(cb.r, cs.r), overlay(cb.g, cs.g), overlay(cb.b, cs.b));
        case 14: return half3(soft_light(cb.r, cs.r), soft_light(cb.g, cs.g), soft_light(cb.b, cs.b));
        case 15: return half3(hard_light(cb.r, cs.r), hard_light(cb.g, cs.g), hard_light(cb.b, cs.b));
        case 16: return half3(vivid_light(cb.r, cs.r), vivid_light(cb.g, cs.g), vivid_light(cb.b, cs.b));
        case 17: return half3(linear_light(cb.r, cs.r), linear_light(cb.g, cs.g), linear_light(cb.b, cs.b));
        case 18: return half3(pin_light(cb.r, cs.r), pin_light(cb.g, cs.g), pin_light(cb.b, cs.b));
        case 19: return half3(hard_mix(cb.r, cs.r), hard_mix(cb.g, cs.g), hard_mix(cb.b, cs.b));
        case 24: // Hue
        case 25: // Saturation
        case 26: // Color
        case 27: // Luminosity
            return blend_hsl(mode, cb, cs);
        default:
            return cs;
    }
}

// Master composite function:
// Un-premultiplies inputs, evaluates blend mode, re-premultiplies output.
// Guarded against alpha == 0, division by zero, and NaN/Inf.
inline half4 composite_blend(int mode, half4 backdrop_pre, half4 source_pre, half opacity) {
    half ab = clamp(backdrop_pre.a, 0.0h, 1.0h);
    half as = clamp(source_pre.a * opacity, 0.0h, 1.0h);
    if (as <= 0.0h) {
        return backdrop_pre;
    }
    if (ab <= 0.0h) {
        return half4(source_pre.rgb * opacity, as);
    }
    half ao = clamp(as + ab * (1.0h - as), 0.0h, 1.0h);
    if (ao <= 1e-4h) {
        return half4(0.0h);
    }
    // Un-premultiply straight colors safely
    half3 cb = backdrop_pre.rgb / max(ab, 1e-4h);
    half3 cs = source_pre.rgb / max(source_pre.a, 1e-4h);
    cb = clamp(cb, half3(0.0h), half3(1.0h));
    cs = clamp(cs, half3(0.0h), half3(1.0h));

    half3 b = blend_mode_color(mode, cb, cs);
    b = clamp(b, half3(0.0h), half3(1.0h));

    // W3C compositing in premultiplied space:
    // C_out_pre = (1 - as) * C_b_pre + (1 - ab) * C_s_pre + as * ab * B
    half3 out_pre = (1.0h - as) * backdrop_pre.rgb + (1.0h - ab) * (source_pre.rgb * opacity) + as * ab * b;
    return half4(clamp(out_pre, half3(0.0h), half3(ao)), ao);
}

// Fragment shader entry points for Metal render passes
struct VertexOut {
    float4 position [[position]];
    float2 texCoord;
};

fragment half4 fs_composite_blend_mode(
    VertexOut in [[stage_in]],
    texture2d<half> backdropTex [[texture(0)]],
    texture2d<half> sourceTex [[texture(1)]],
    constant int &mode [[buffer(0)]],
    constant half &opacity [[buffer(1)]]
) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    half4 b = backdropTex.sample(s, in.texCoord);
    half4 src = sourceTex.sample(s, in.texCoord);
    return composite_blend(mode, b, src, opacity);
}

// PhotoCraft Procreate-Feel Pass 2: Height-Map "Paper Tooth" Grain & Wet Mix Fast Path

// Height-map paper tooth grain modulation (World-Locked)
// Evaluates paper tooth coverage driven by calibrated pressure and grainDepth:
// mask = clamp((pressure * depth - (1.0 - grainHeight)) / softness, 0.0, 1.0)
inline half eval_height_map_grain(half pressure, half depth, half grain_height, half softness) {
    half s = max(softness, 1e-4h);
    half p = clamp(pressure, 0.0h, 1.0h);
    half d = clamp(depth, 0.0h, 1.0h);
    half gh = clamp(grain_height, 0.0h, 1.0h);
    return clamp((p * d - (1.0h - gh)) / s, 0.0h, 1.0h);
}

// Wet Mix / Color Pickup (Painterly Brushes)
// Samples underlying canvas color and blends into active brush color using linear premultiplied math.
// Completely on-GPU: zero CPU readbacks (no getBytes).
inline half4 eval_wet_mix(
    half4 brush_color_pre,
    half4 canvas_color_pre,
    half charge,
    half pull,
    half mix_ratio,
    half progress
) {
    if (pull <= 0.0h && mix_ratio <= 0.0h) {
        return brush_color_pre;
    }
    half effective_pickup = clamp(max(pull, mix_ratio) * (1.0h - charge * clamp(1.0h - progress, 0.0h, 1.0h)), 0.0h, 1.0h);
    return mix(brush_color_pre, canvas_color_pre, effective_pickup);
}

fragment half4 fs_paint_dab(
    VertexOut in [[stage_in]],
    texture2d<half> canvasTex [[texture(0)]],
    texture2d<half> grainTex [[texture(1)]],
    constant float2 &worldCoord [[buffer(0)]],
    constant float &textureScale [[buffer(1)]],
    constant half &pressure [[buffer(2)]],
    constant half &depth [[buffer(3)]],
    constant half &softness [[buffer(4)]],
    constant half4 &dabColor [[buffer(5)]],
    constant half4 &wetMixParams [[buffer(6)]] // x: charge, y: pull, z: mix, w: progress
) {
    constexpr sampler sRepeat(address::repeat, filter::linear);
    constexpr sampler sClamp(address::clamp_to_edge, filter::linear);

    // World-locked paper grain UV: invariant under canvas pan, zoom, and rotation
    float2 worldUV = worldCoord / max(textureScale, 0.01f);
    half grainHeight = grainTex.sample(sRepeat, worldUV).r;

    half grainMask = (depth > 0.0h)
        ? eval_height_map_grain(pressure, depth, grainHeight, softness)
        : 1.0h;

    // Linear premultiplied color
    half4 baseColor = half4(dabColor.rgb * dabColor.a, dabColor.a);

    // Wet mix color pickup from underlying canvas without CPU readbacks
    half4 finalColor = baseColor;
    if (wetMixParams.y > 0.0h || wetMixParams.z > 0.0h) {
        half4 canvasColor = canvasTex.sample(sClamp, in.texCoord);
        finalColor = eval_wet_mix(baseColor, canvasColor, wetMixParams.x, wetMixParams.y, wetMixParams.z, wetMixParams.w);
    }

    half finalAlpha = finalColor.a * grainMask;
    return half4(finalColor.rgb * grainMask, finalAlpha);
}
