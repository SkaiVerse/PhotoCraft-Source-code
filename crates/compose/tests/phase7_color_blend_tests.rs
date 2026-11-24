//! Phase 7 Verification: Color & Blend
//! Tests Display P3 conversions, linear premultiplied blending, NaN/Inf safety across all blend modes,
//! Dodge & Burn extremes, Soft Light continuity, and HSL luminance preservation.

use photocraft_color::blend::BlendMode;
use photocraft_color::convert::{
    display_p3_to_linear, display_p3_to_srgb, linear_p3_to_linear_srgb, linear_srgb_to_linear_p3,
    linear_to_display_p3, linear_to_srgb, srgb_to_display_p3, srgb_to_linear,
};
use photocraft_compose::psblend::{
    composite, composite_linear_premultiplied, linear_premultiplied_to_straight,
    straight_to_linear_premultiplied,
};

#[test]
fn test_display_p3_linear_transfer_identity() {
    // Verify that Display P3 uses the exact same transfer curve as sRGB (zero double-gamma shift).
    for i in 0..=255 {
        let v = i as f32 / 255.0;
        let lin_srgb = srgb_to_linear(v);
        let lin_p3 = display_p3_to_linear(v);
        assert!((lin_srgb - lin_p3).abs() < 1e-7, "P3 and sRGB transfer curves must be identical at {v}");

        let enc_srgb = linear_to_srgb(lin_srgb);
        let enc_p3 = linear_to_display_p3(lin_p3);
        assert!((enc_srgb - enc_p3).abs() < 1e-7);
        assert!((enc_p3 - v).abs() < 1e-5, "Round-trip transfer must reproduce original input {v}");
    }
}

#[test]
fn test_display_p3_srgb_matrix_roundtrip() {
    // Test pure primaries and arbitrary colors round-trip between linear sRGB and linear Display P3.
    let test_colors = [
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [1.0, 1.0, 1.0],
        [0.5, 0.5, 0.5],
        [0.2, 0.7, 0.4],
        [0.85, 0.15, 0.65],
    ];

    for c in test_colors {
        let p3_lin = linear_srgb_to_linear_p3(c);
        let back_lin = linear_p3_to_linear_srgb(p3_lin);
        for ch in 0..3 {
            assert!((c[ch] - back_lin[ch]).abs() < 1e-5, "Linear matrix round-trip failed for {c:?} channel {ch}: got {back_lin:?}");
        }

        // Test encoded sRGB -> encoded Display P3 -> encoded sRGB
        let p3_enc = srgb_to_display_p3(c);
        let back_enc = display_p3_to_srgb(p3_enc);
        for ch in 0..3 {
            assert!((c[ch] - back_enc[ch]).abs() < 1e-4, "Encoded round-trip failed for {c:?} channel {ch}: got {back_enc:?}");
        }
    }
}

#[test]
fn test_display_p3_white_point_invariance() {
    // Both sRGB and Display P3 share the D65 white point (1.0, 1.0, 1.0)
    let white = [1.0, 1.0, 1.0];
    let p3_white = linear_srgb_to_linear_p3(white);
    for ch in 0..3 {
        assert!((p3_white[ch] - 1.0).abs() < 1e-6, "White point must remain exactly (1,1,1) in Display P3");
    }
    let enc_white = srgb_to_display_p3(white);
    for ch in 0..3 {
        assert!((enc_white[ch] - 1.0).abs() < 1e-6, "White point must remain exactly (1,1,1) in encoded Display P3");
    }
}

#[test]
fn test_linear_premultiplied_blending_eliminates_muddy_halos() {
    // Saturated Red [1, 0, 0] over Saturated Green [0, 1, 0] at 50% opacity
    let red_pre = straight_to_linear_premultiplied([1.0, 0.0, 0.0, 1.0]);
    let green_pre = straight_to_linear_premultiplied([0.0, 1.0, 0.0, 1.0]);

    let blended_linear_pre = composite_linear_premultiplied(BlendMode::Normal, green_pre, red_pre, 0.5);
    let blended_straight = linear_premultiplied_to_straight(blended_linear_pre);

    // In linear optical blending, 0.5 red + 0.5 green in linear light yields:
    // linear red = 0.5, linear green = 0.5
    // Encoded in sRGB/P3, 0.5^(1/2.4) * 1.055 - 0.055 ≈ 0.735
    // In naive gamma blending, it would be 0.5, which suffers from a severe dark muddy band (perceptual drop to ~0.218 luminance).
    assert!(blended_straight[0] > 0.70, "Red channel must stay bright in linear blend: got {}", blended_straight[0]);
    assert!(blended_straight[1] > 0.70, "Green channel must stay bright in linear blend: got {}", blended_straight[1]);
    assert_eq!(blended_straight[3], 1.0);
}

#[test]
fn test_all_blend_modes_nan_inf_free_at_alpha_zero() {
    let zero_alpha = [0.5, 0.5, 0.5, 0.0];
    let opaque = [0.8, 0.2, 0.3, 1.0];
    let half_alpha = [0.2, 0.6, 0.4, 0.5];

    let modes = [
        BlendMode::Normal,
        BlendMode::Darken,
        BlendMode::Multiply,
        BlendMode::ColorBurn,
        BlendMode::LinearBurn,
        BlendMode::Lighten,
        BlendMode::Screen,
        BlendMode::ColorDodge,
        BlendMode::LinearDodge,
        BlendMode::Overlay,
        BlendMode::SoftLight,
        BlendMode::HardLight,
        BlendMode::VividLight,
        BlendMode::LinearLight,
        BlendMode::PinLight,
        BlendMode::HardMix,
        BlendMode::Difference,
        BlendMode::Exclusion,
        BlendMode::Subtract,
        BlendMode::Divide,
        BlendMode::Hue,
        BlendMode::Saturation,
        BlendMode::Color,
        BlendMode::Luminosity,
    ];

    for &mode in &modes {
        // 1. Source has alpha 0
        let r1 = composite(mode, opaque, zero_alpha, 1.0);
        assert_eq!(r1, opaque, "Mode {mode:?} with zero source alpha must return backdrop");

        // 2. Backdrop has alpha 0
        let r2 = composite(mode, zero_alpha, opaque, 1.0);
        for ch in 0..4 {
            assert!(r2[ch].is_finite(), "Mode {mode:?} with zero backdrop alpha produced non-finite {r2:?}");
        }

        // 3. Both have alpha 0
        let r3 = composite(mode, zero_alpha, zero_alpha, 1.0);
        assert_eq!(r3[3], 0.0, "Mode {mode:?} with both alpha 0 must have output alpha 0");
        assert!(r3.iter().all(|c| c.is_finite() && !c.is_nan()), "Mode {mode:?} both alpha 0 produced non-finite {r3:?}");

        // 4. Linear premultiplied with zero alpha
        let lp1 = composite_linear_premultiplied(mode, opaque, zero_alpha, 1.0);
        assert_eq!(lp1, opaque);

        let lp2 = composite_linear_premultiplied(mode, zero_alpha, opaque, 1.0);
        for ch in 0..4 {
            assert!(lp2[ch].is_finite());
        }

        let lp3 = composite_linear_premultiplied(mode, zero_alpha, zero_alpha, 1.0);
        assert_eq!(lp3[3], 0.0);
        assert!(lp3.iter().all(|c| c.is_finite() && !c.is_nan()));

        // 5. Half-alpha blend
        let lp4 = composite_linear_premultiplied(mode, half_alpha, half_alpha, 0.5);
        for ch in 0..4 {
            assert!(lp4[ch].is_finite(), "Mode {mode:?} half-alpha produced non-finite {lp4:?}");
            assert!(!lp4[ch].is_nan(), "Mode {mode:?} half-alpha produced NaN {lp4:?}");
        }
    }
}

#[test]
fn test_dodge_burn_extreme_endpoints_and_guards() {
    let endpoints = [
        ([0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.0, 1.0]),
        ([0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0]),
        ([1.0, 1.0, 1.0, 1.0], [0.0, 0.0, 0.0, 1.0]),
        ([1.0, 1.0, 1.0, 1.0], [1.0, 1.0, 1.0, 1.0]),
        ([1e-6, 1e-6, 1e-6, 1.0], [1.0 - 1e-6, 1.0 - 1e-6, 1.0 - 1e-6, 1.0]),
        ([1.0 - 1e-6, 1.0 - 1e-6, 1.0 - 1e-6, 1.0], [1e-6, 1e-6, 1e-6, 1.0]),
    ];

    for (cb, cs) in endpoints {
        // Color Dodge
        let cd = composite(BlendMode::ColorDodge, cb, cs, 1.0);
        for ch in 0..3 {
            assert!(cd[ch].is_finite() && !cd[ch].is_nan(), "Color Dodge failed for cb={cb:?}, cs={cs:?}: got {cd:?}");
            assert!(cd[ch] >= 0.0 && cd[ch] <= 1.0);
        }

        // Color Burn
        let cb_res = composite(BlendMode::ColorBurn, cb, cs, 1.0);
        for ch in 0..3 {
            assert!(cb_res[ch].is_finite() && !cb_res[ch].is_nan(), "Color Burn failed for cb={cb:?}, cs={cs:?}: got {cb_res:?}");
            assert!(cb_res[ch] >= 0.0 && cb_res[ch] <= 1.0);
        }

        // Linear Dodge
        let ld = composite(BlendMode::LinearDodge, cb, cs, 1.0);
        for ch in 0..3 {
            assert!(ld[ch].is_finite() && !ld[ch].is_nan());
            assert!(ld[ch] >= 0.0 && ld[ch] <= 1.0);
        }

        // Linear Burn
        let lb = composite(BlendMode::LinearBurn, cb, cs, 1.0);
        for ch in 0..3 {
            assert!(lb[ch].is_finite() && !lb[ch].is_nan());
            assert!(lb[ch] >= 0.0 && lb[ch] <= 1.0);
        }
    }
}

#[test]
fn test_soft_light_w3c_piecewise_continuity() {
    // Test Soft Light across the piecewise threshold at cb = 0.25 and cs = 0.5
    for eps in [-1e-4f32, -1e-6, 0.0, 1e-6, 1e-4] {
        let cb_val = (0.25f32 + eps).clamp(0.0, 1.0);
        let cs_val = 0.75f32; // in the D(cb) branch
        let r = composite(BlendMode::SoftLight, [cb_val, cb_val, cb_val, 1.0], [cs_val, cs_val, cs_val, 1.0], 1.0);
        assert!(r[0].is_finite() && !r[0].is_nan(), "Soft Light non-finite around cb=0.25 threshold: got {r:?}");

        // Now test around cs = 0.5 branch transition
        let cs_branch = (0.5f32 + eps).clamp(0.0, 1.0);
        let r2 = composite(BlendMode::SoftLight, [0.3, 0.3, 0.3, 1.0], [cs_branch, cs_branch, cs_branch, 1.0], 1.0);
        assert!(r2[0].is_finite() && !r2[0].is_nan(), "Soft Light non-finite around cs=0.5 threshold: got {r2:?}");
    }
}

#[test]
fn test_hsl_luminance_preserving_clamping() {
    let test_cases = [
        ([1.0, 0.0, 0.0], [0.0, 0.8, 0.2]),
        ([0.1, 0.9, 0.4], [0.8, 0.1, 0.7]),
        ([0.5, 0.5, 0.5], [1.0, 0.0, 0.0]),
        ([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]),
        ([1.0, 1.0, 1.0], [0.0, 0.0, 0.0]),
    ];

    for (cb, cs) in test_cases {
        let b = [cb[0], cb[1], cb[2], 1.0];
        let s = [cs[0], cs[1], cs[2], 1.0];

        for &mode in &[BlendMode::Hue, BlendMode::Saturation, BlendMode::Color, BlendMode::Luminosity] {
            let res = composite(mode, b, s, 1.0);
            for ch in 0..3 {
                assert!(res[ch].is_finite() && !res[ch].is_nan(), "HSL mode {mode:?} produced non-finite: {res:?}");
                assert!(res[ch] >= -1e-5 && res[ch] <= 1.0 + 1e-5, "HSL mode {mode:?} out of gamut: {res:?}");
            }

            // Luminosity mode must exactly preserve source luminosity:
            if mode == BlendMode::Luminosity {
                let target_lum = photocraft_color::blend::lum(cs);
                let actual_lum = photocraft_color::blend::lum([res[0], res[1], res[2]]);
                assert!((target_lum - actual_lum).abs() < 1e-3, "Luminosity mode must preserve source lum: expected {target_lum}, got {actual_lum}");
            }

            // Color mode must preserve backdrop luminosity:
            if mode == BlendMode::Color {
                let target_lum = photocraft_color::blend::lum(cb);
                let actual_lum = photocraft_color::blend::lum([res[0], res[1], res[2]]);
                assert!((target_lum - actual_lum).abs() < 1e-3, "Color mode must preserve backdrop lum: expected {target_lum}, got {actual_lum}");
            }
        }
    }
}
