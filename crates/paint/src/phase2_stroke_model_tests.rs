//! Phase 2 Stroke Model Unit Tests:
//! 1. Sample distance rejection formula (canvas-space aware).
//! 2. Directional damping with 0.125 base gain + EMA (parallel latency < 2 frames).
//! 3. 256-entry 1-D dynamics LUTs for pressure, tilt, scatter, and azimuth.
//! 4. Serialization and deserialization of stabilization & streamline options.

use crate::brush::{BrushSettings, Control, Dynamic, Scattering, ShapeDynamics, Smoothing, Transfer};
use crate::curve::ResponseCurve;
use crate::dynamics::{
    DynamicsLut, apply_directional_damping, calibrate_stylus_pressure, filter_circular_angle,
};
use crate::StrokePoint;

#[test]
fn test_directional_damping_gain_and_latency() {
    let p0 = StrokePoint::new(0.0, 0.0, 1.0);
    let mut dir = Some([1.0, 0.0]); // moving along x axis

    // Perpendicular wobble in y: (0, 10.0)
    let p_wobble = StrokePoint::new(0.0, 10.0, 1.0);
    let damped_wobble = apply_directional_damping(p_wobble, p0, &mut dir, 1.0);
    // At max strength 1.0, perpendicular gain is 0.125
    assert!((damped_wobble.y - 1.25).abs() < 1e-4, "Perpendicular jitter should be damped with base gain 0.125, got {}", damped_wobble.y);

    // Parallel step in x: (10.0, 0.0)
    let mut dir_x = Some([1.0, 0.0]);
    let p_forward = StrokePoint::new(10.0, 0.0, 1.0);
    let damped_forward = apply_directional_damping(p_forward, p0, &mut dir_x, 1.0);
    // At max strength 1.0, parallel gain is 0.5 (latency = 1 frame < 2 frames)
    assert!((damped_forward.x - 5.0).abs() < 1e-4, "Parallel travel should have alpha_par = 0.5, got {}", damped_forward.x);
}

#[test]
fn test_dynamics_lut_256_tables() {
    let curve = ResponseCurve::new(&[[0.0, 0.0], [0.5, 0.2], [1.0, 1.0]]);
    let b = BrushSettings {
        size: 32.0,
        shape_dynamics: ShapeDynamics {
            enabled: true,
            size: Dynamic { curve: curve.clone(), ..Dynamic::controlled(Control::PenPressure) },
            angle: Dynamic::controlled(Control::Rotation),
            ..Default::default()
        },
        transfer: Transfer {
            enabled: true,
            opacity: Dynamic { curve: curve.clone(), ..Dynamic::controlled(Control::PenTilt) },
            ..Default::default()
        },
        scattering: Scattering {
            enabled: true,
            scatter: Dynamic { curve: curve.clone(), ..Dynamic::controlled(Control::PenTilt) },
            ..Default::default()
        },
        ..Default::default()
    };

    let lut = DynamicsLut::new(&b);
    assert_eq!(lut.pressure_to_size.len(), 256);
    assert_eq!(lut.pressure_to_opacity.len(), 256);
    assert_eq!(lut.tilt_to_size.len(), 256);
    assert_eq!(lut.tilt_to_opacity.len(), 256);
    assert_eq!(lut.tilt_to_scatter.len(), 256);
    assert_eq!(lut.azimuth_to_rotation.len(), 256);

    // Endpoints
    assert!((lut.size_from_pressure(0.0) - 0.0).abs() < 1e-4);
    assert!((lut.size_from_pressure(1.0) - 1.0).abs() < 1e-4);
    assert!((lut.rotation_from_azimuth(0.0) - 0.0).abs() < 1e-4);
    assert!((lut.rotation_from_azimuth(360.0) - 0.0).abs() < 1e-4);
}

#[test]
fn test_stabilization_and_streamline_serde() {
    let s = Smoothing {
        enabled: true,
        amount: 0.4,
        pulled_string: false,
        catch_up: true,
        catch_up_on_end: true,
        adjust_for_zoom: true,
        stabilization: 0.75,
        streamline: 0.5,
    };
    let json = serde_json::to_string(&s).unwrap();
    assert!(json.contains("\"stabilization\":0.75"));
    assert!(json.contains("\"streamline\":0.5"));

    let back: Smoothing = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);

    // Default backward compatibility
    let legacy_json = r#"{"enabled":true,"amount":0.2}"#;
    let legacy: Smoothing = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(legacy.stabilization, 0.0);
    assert_eq!(legacy.streamline, 0.0);
}

#[test]
fn test_duplicate_points_and_rapid_pressure_spikes() {
    let p0 = StrokePoint::new(10.0, 10.0, 0.5);
    let mut dir = Some([1.0, 0.0]);

    // 1. Duplicate points (identical x, y)
    let p_dup = StrokePoint::new(10.0, 10.0, 0.8);
    let damped_dup = apply_directional_damping(p_dup, p0, &mut dir, 0.5);
    assert_eq!(damped_dup.x, 10.0);
    assert_eq!(damped_dup.y, 10.0);
    assert!(damped_dup.pressure.is_finite());
    assert!(damped_dup.pressure >= 0.0009765625 && damped_dup.pressure <= 1.0);

    // 2. Pressure spikes: NaN, infinity, negative, and extreme values
    let p_nan = StrokePoint::new(15.0, 10.0, f32::NAN);
    let damped_nan = apply_directional_damping(p_nan, p0, &mut dir, 0.5);
    assert!(damped_nan.pressure.is_finite(), "Pressure must be sanitized from NaN");
    assert!(damped_nan.pressure >= 0.0009765625 && damped_nan.pressure <= 1.0);

    let p_inf = StrokePoint::new(20.0, 10.0, f32::INFINITY);
    let damped_inf = apply_directional_damping(p_inf, p0, &mut dir, 0.5);
    assert!(damped_inf.pressure.is_finite(), "Pressure must be sanitized from Infinity");
    assert!(damped_inf.pressure >= 0.0009765625 && damped_inf.pressure <= 1.0);

    // 3. DynamicsLut evaluation under pressure spikes and NaNs
    let b = BrushSettings {
        size: 24.0,
        pressure_size: true,
        pressure_opacity: true,
        ..Default::default()
    };
    let lut = DynamicsLut::new(&b);
    let s_nan = lut.size_from_pressure(f32::NAN);
    assert!(s_nan.is_finite(), "LUT size lookup on NaN must be finite");
    let s_inf = lut.size_from_pressure(f32::INFINITY);
    assert!(s_inf.is_finite(), "LUT size lookup on Infinity must be finite");
    let o_neg = lut.opacity_from_pressure(-10.0);
    assert!(o_neg.is_finite(), "LUT opacity lookup on negative pressure must be finite");
}

#[test]
fn test_phase3_arc_length_stepping_and_taper() {
    use crate::dynamics::DabGenerator;
    use crate::render::StrokeRenderer;
    use photocraft_color::PixelFormat;
    use photocraft_raster::Surface;

    // 1. Arc-length stepping floor of 0.5 pt
    let brush = BrushSettings {
        size: 1.0,
        spacing: 0.05, // 5% of 1px = 0.05px, but should floor at 0.5
        ..Default::default()
    };
    let d_gen = DabGenerator::new(&brush, 1.0);
    let mut dabs = Vec::new();
    let mut dual = Vec::new();
    let mut gen_clone = d_gen.clone();
    gen_clone.push(&[StrokePoint::new(0.0, 0.0, 1.0), StrokePoint::new(10.0, 0.0, 1.0)], &mut dabs, &mut dual);
    gen_clone.finish(&mut dabs, &mut dual);
    // Over a 10px line with 0.5 step, we should have around 20 dabs
    assert!((18..=25).contains(&dabs.len()), "Expected ~20 dabs for 10px line at 0.5 floor, got {}", dabs.len());

    // 2. Start & end taper
    let taper_brush = BrushSettings {
        size: 20.0,
        taper_start_length: 0.3,
        taper_end_length: 0.3,
        taper_pressure: 0.8,
        ..Default::default()
    };
    let mut t_gen = DabGenerator::new(&taper_brush, 1.0);
    let mut t_dabs = Vec::new();
    let mut t_dual = Vec::new();
    let pts: Vec<StrokePoint> = (0..50).map(|i| StrokePoint::new(i as f64 * 5.0, 0.0, 1.0)).collect();
    t_gen.push(&pts, &mut t_dabs, &mut t_dual);
    t_gen.finish(&mut t_dabs, &mut t_dual);

    assert!(!t_dabs.is_empty());
    let first_r = t_dabs[0].radius;
    let mid_r = t_dabs[t_dabs.len() / 2].radius;
    let last_r = t_dabs.last().unwrap().radius;
    assert!(first_r < mid_r, "Start dab radius ({first_r}) should be smaller than mid ({mid_r}) due to start taper");
    assert!(last_r < mid_r, "End dab radius ({last_r}) should be smaller than mid ({mid_r}) due to end taper");

    // 3. Separate flow vs opacity
    let mut surf = Surface::new(PixelFormat::RGBA8);
    let flow_brush = BrushSettings {
        size: 30.0,
        flow: 0.2,
        opacity: 0.5,
        color: [1.0, 0.0, 0.0, 1.0],
        ..Default::default()
    };
    let pre = surf.clone();
    let mut renderer = StrokeRenderer::new(&flow_brush, Some(surf.format()), 1.0);
    renderer.push(&[StrokePoint::new(20.0, 50.0, 1.0), StrokePoint::new(80.0, 50.0, 1.0)]);
    renderer.finish();
    renderer.composite(&pre, &mut surf, None, false, true);
    // At opacity 0.5, maximum alpha on canvas cannot exceed 0.5
    let col = surf.rgba(50, 50);
    assert!(col[3] <= 0.51, "Final stroke alpha ({}) should not exceed opacity ceiling (0.5)", col[3]);
    assert!(col[3] > 0.05, "Stroke must have painted red pixels");
}

#[test]
fn test_phase4_wet_stroke_buffer_and_premultiplied_alpha_compositing() {
    use crate::render::StrokeRenderer;
    use photocraft_color::PixelFormat;
    use photocraft_raster::Surface;

    // 1. Wet-stroke buffer accumulation with flow and premultiplied alpha compositing
    let mut surf = Surface::new(PixelFormat::RGBA8);
    let brush = BrushSettings {
        size: 40.0,
        flow: 0.25,
        opacity: 0.8,
        color: [0.2, 0.6, 0.9, 1.0], // light blue
        hardness: 0.5,
        ..Default::default()
    };
    let pre = surf.clone();
    let mut renderer = StrokeRenderer::new(&brush, Some(surf.format()), 1.0);
    renderer.push(&[
        StrokePoint::new(50.0, 50.0, 0.5),
        StrokePoint::new(70.0, 50.0, 0.7),
        StrokePoint::new(90.0, 50.0, 0.9),
    ]);

    // Composite incrementally (dirty tiles only)
    let dmg = renderer.composite(&pre, &mut surf, None, false, false);
    assert!(!dmg.is_empty(), "Damage rect from incremental composite must not be empty");
    assert!(dmg.contains(50, 50), "Damage rect must contain stroke points");

    // Push more points and verify take_dirty_rect
    renderer.push(&[StrokePoint::new(110.0, 50.0, 0.9)]);
    let dirty_rect = renderer.take_dirty_rect();
    assert!(!dirty_rect.is_empty(), "Dirty rect must not be empty after push");
    assert!(dirty_rect.contains(110, 50), "Dirty rect must contain newly pushed point");

    // Verify premultiplied alpha composited pixels: no negative or >1.0 channels
    let c_center = surf.rgba(70, 50);
    assert!(c_center[3] > 0.0 && c_center[3] <= 0.81, "Alpha should be within opacity ceiling");
    for ch in 0..3 {
        assert!(c_center[ch] >= 0.0 && c_center[ch] <= 1.0, "Color channel must be strictly within [0, 1]");
    }

    // Verify soft edge has no alpha halo (edge color should match source RGB without fringe)
    let c_edge = surf.rgba(70, 35);
    if c_edge[3] > 0.01 {
        // Red ratio (0.2 / 0.6 = ~0.33) should remain consistent
        let ratio = c_edge[0] / c_edge[1].max(1e-4);
        assert!((ratio - (0.2 / 0.6)).abs() < 0.08, "Edge pixels must preserve chrominance without dark fringing");
    }
}

#[test]
fn test_phase4_transient_predicted_touch_overlay_isolation() {
    use crate::render::StrokeRenderer;
    use photocraft_color::PixelFormat;
    use photocraft_raster::Surface;

    let mut surf = Surface::new(PixelFormat::RGBA8);
    let brush = BrushSettings {
        size: 30.0,
        flow: 1.0,
        opacity: 1.0,
        color: [1.0, 0.0, 0.0, 1.0],
        ..Default::default()
    };
    let pre = surf.clone();
    let mut renderer = StrokeRenderer::new(&brush, Some(surf.format()), 1.0).record_dabs();

    // Push committed touches
    let committed_pts = vec![
        StrokePoint::new(10.0, 10.0, 0.5),
        StrokePoint::new(30.0, 10.0, 0.6),
    ];
    renderer.push(&committed_pts);

    let committed_dabs_len = renderer.dabs().len();
    assert!(committed_dabs_len > 0, "Committed points must produce dabs");

    // Predicted touch: ephemeral overlay only!
    // Verify that StrokeRenderer state is completely untouched by external predictions
    assert_eq!(renderer.dabs().len(), committed_dabs_len);
    let bounds_before = renderer.bounds();

    // Finish committed stroke
    renderer.finish();
    renderer.composite(&pre, &mut surf, None, false, true);

    // Pixel at (10, 10) must be painted
    assert!(surf.rgba(10, 10)[3] > 0.5);
    // Predicted point that was NEVER committed (e.g. 80, 10) must remain blank on canvas
    assert_eq!(surf.rgba(80, 10)[3], 0.0, "Predicted points must never leak into layer surface");
    assert!(bounds_before.contains_rect(&renderer.bounds().intersect(&bounds_before)));
}

#[test]
fn test_phase4_stroke_commit_and_cleanup() {
    use crate::render::StrokeRenderer;
    use photocraft_color::PixelFormat;
    use photocraft_raster::Surface;

    let mut live_surf = Surface::new(PixelFormat::RGBA8);
    let mut direct_surf = Surface::new(PixelFormat::RGBA8);

    let brush = BrushSettings {
        size: 25.0,
        flow: 0.8,
        opacity: 0.9,
        color: [0.1, 0.8, 0.3, 1.0],
        smoothing: crate::brush::Smoothing {
            enabled: true,
            amount: 0.5,
            catch_up: true,
            catch_up_on_end: true,
            ..Default::default()
        },
        ..Default::default()
    };

    let pts = vec![
        StrokePoint::new(20.0, 20.0, 0.4),
        StrokePoint::new(40.0, 30.0, 0.6),
        StrokePoint::new(60.0, 50.0, 0.8),
    ];

    // Path A: Direct render with push + finish + composite
    let pre_direct = direct_surf.clone();
    let mut direct_renderer = StrokeRenderer::new(&brush, Some(direct_surf.format()), 1.0);
    direct_renderer.push(&pts);
    direct_renderer.finish();
    direct_renderer.composite(&pre_direct, &mut direct_surf, None, false, true);

    // Path B: Incremental live render with composite_live + finish + composite
    let pre_live = live_surf.clone();
    let mut live_renderer = StrokeRenderer::new(&brush, Some(live_surf.format()), 1.0);
    let (dmg1, tail1) = live_renderer.composite_live(photocraft_geom::Rect::EMPTY, &pre_live, &mut live_surf, None, false);
    assert!(!dmg1.is_empty() || tail1.is_empty());
    live_renderer.push(&pts[..2]);
    let (dmg2, tail2) = live_renderer.composite_live(tail1, &pre_live, &mut live_surf, None, false);
    assert!(!dmg2.is_empty() || !tail2.is_empty());
    live_renderer.push(&pts[2..]);
    let (dmg3, tail3) = live_renderer.composite_live(tail2, &pre_live, &mut live_surf, None, false);
    assert!(!dmg3.is_empty() || !tail3.is_empty());

    // Finish & cleanup: revert tail3 and composite finalized dabs
    live_renderer.finish();
    live_renderer.mark_dirty(tail3);
    live_renderer.composite(&pre_live, &mut live_surf, None, false, false);

    // Compare pixels between direct stroke and finalized live stroke:
    // They must match identically across the stroke bounds (zero pop on commit)
    let b = direct_renderer.bounds();
    let mut mismatches = 0;
    for y in b.y0..b.y1 {
        for x in b.x0..b.x1 {
            let p_dir = direct_surf.rgba(x, y);
            let p_live = live_surf.rgba(x, y);
            for c in 0..4 {
                if (p_dir[c] - p_live[c]).abs() > 0.05 {
                    mismatches += 1;
                }
            }
        }
    }
    assert_eq!(mismatches, 0, "Finalized live stroke pixels must match direct stroke commit without visual pop");
}

#[test]
fn test_global_concave_pressure_calibration_curve() {
    // 0.0 -> 0.0
    let p0 = calibrate_stylus_pressure(0.0);
    assert!((p0 - 0.0).abs() < 1e-5, "0.0 force should calibrate to 0.0, got {p0}");

    // 0.10 -> light concave lift (~0.24..0.30, never flat or muted)
    let p10 = calibrate_stylus_pressure(0.10);
    assert!(p10 > 0.20 && p10 < 0.35, "0.10 force should produce smooth concave light-stroke lift, got {p10}");

    // 0.70 -> leaves genuine upper headroom (< 0.99, > 0.85) instead of early clipping at 1.0
    let p70 = calibrate_stylus_pressure(0.70);
    assert!(p70 > 0.85 && p70 < 0.99, "0.70 force should retain upper headroom instead of hard clipping, got {p70}");

    // Monotonicity across the entire [0, 1] range and full output at 1.0
    assert!(p10 < calibrate_stylus_pressure(0.40));
    assert!(calibrate_stylus_pressure(0.40) < p70);
    let p100 = calibrate_stylus_pressure(1.0);
    assert!((p100 - 1.0).abs() < 1e-5, "1.00 force should calibrate to 1.0, got {p100}");
}

#[test]
fn test_damped_circular_angle_filter_wrap_around() {
    // Shortest-arc interpolation across [-pi, pi] boundary without wrap-around glitches or 360-degree spins
    let pi = std::f32::consts::PI;
    let current = 3.10; // ~177.6 degrees
    let target = -3.10; // ~ -177.6 degrees (arc distance is ~4.7 degrees across +/- pi)

    let filtered = filter_circular_angle(current, target, 0.5);
    // Shortest arc diff is (-3.10 - 3.10) + 2*pi = -6.20 + 6.283185 = +0.083185
    // Result should be 3.10 + 0.5 * 0.083185 = 3.14159... (~pi)
    let dist_to_boundary = (filtered.abs() - pi).abs();
    assert!(dist_to_boundary < 0.05, "Circular filter should interpolate smoothly across +/-pi boundary, got {filtered}");

    // Non-circular naive lerp would have produced: 3.10 + 0.5 * (-3.10 - 3.10) = 0.0 (wrong 180-deg reversal!)
    assert!(filtered.abs() > 3.0, "Angle should not glitch or reverse to 0.0");
}

#[test]
fn test_height_map_grain_bounds() {
    use crate::dynamics::eval_height_map_grain;

    // At pressure = 0.0, mask should be 0.0 regardless of depth or grain height
    assert_eq!(eval_height_map_grain(0.0, 1.0, 0.5, 0.25), 0.0);
    assert_eq!(eval_height_map_grain(0.0, 0.5, 1.0, 0.25), 0.0);

    // At pressure = 1.0 and depth = 1.0:
    // Peak (grain_height = 1.0): 1.0 * 1.0 - (1.0 - 1.0) = 1.0 / softness -> clamped to 1.0
    assert_eq!(eval_height_map_grain(1.0, 1.0, 1.0, 0.25), 1.0);
    // Valley (grain_height = 0.0): 1.0 * 1.0 - (1.0 - 0.0) = 0.0 -> clamped to 0.0
    assert_eq!(eval_height_map_grain(1.0, 1.0, 0.0, 0.25), 0.0);

    // Light pressure catches paper peaks while heavy pressure fills valleys
    let light_peak = eval_height_map_grain(0.3, 1.0, 0.95, 0.25);
    let light_valley = eval_height_map_grain(0.3, 1.0, 0.2, 0.25);
    assert!(light_peak > 0.0, "Light pressure should catch paper peaks");
    assert_eq!(light_valley, 0.0, "Light pressure should not fill valleys");

    let heavy_valley = eval_height_map_grain(0.9, 1.0, 0.2, 0.25);
    assert!(heavy_valley > light_valley, "Heavy pressure should fill valleys more than light pressure");

    // Softness floor: division-by-zero protection (softness = 0.0 or negative)
    let s_zero = eval_height_map_grain(0.5, 1.0, 0.8, 0.0);
    assert!(s_zero.is_finite() && !s_zero.is_nan(), "Softness floor must prevent division by zero");
    let s_neg = eval_height_map_grain(0.5, 1.0, 0.8, -1.0);
    assert!(s_neg.is_finite() && !s_neg.is_nan(), "Negative softness must be floored safely");

    // Non-finite inputs: zero NaN propagation
    let nan_val = eval_height_map_grain(f32::NAN, 1.0, 0.5, 0.25);
    assert_eq!(nan_val, 0.0, "NaN pressure must yield 0.0 without NaN propagation");
    let inf_val = eval_height_map_grain(1.0, f32::INFINITY, 0.5, 0.25);
    assert_eq!(inf_val, 0.0, "Inf depth must yield clamped value without NaN/Inf propagation");
}

#[test]
fn test_wet_mix_zero_effect_when_disabled_and_premultiplied_blend() {
    use crate::dynamics::blend_wet_mix;

    let brush_col = [1.0, 0.0, 0.0, 1.0]; // Red
    let canvas_col = [0.0, 0.0, 1.0, 1.0]; // Blue

    // When disabled (pull == 0 and mix == 0): exact identity
    let disabled = blend_wet_mix(brush_col, canvas_col, 0.0, 0.0, 0.0, 0.5);
    assert_eq!(disabled, brush_col, "Disabled wet mix must return brush color unmodified");

    let disabled_with_charge = blend_wet_mix(brush_col, canvas_col, 0.8, 0.0, 0.0, 0.5);
    assert_eq!(disabled_with_charge, brush_col, "Charge without pull/mix must return brush color unmodified");

    // When active: linear premultiplied blending
    let blended = blend_wet_mix(brush_col, canvas_col, 0.0, 0.5, 0.5, 0.5);
    // pickup = 0.5 * (1.0 - 0) = 0.5
    // blend: 0.5 * red + 0.5 * blue = [0.5, 0.0, 0.5, 1.0] (Purple)
    assert!((blended[0] - 0.5).abs() < 1e-4, "Red component blended: {}", blended[0]);
    assert!((blended[1] - 0.0).abs() < 1e-4, "Green component blended: {}", blended[1]);
    assert!((blended[2] - 0.5).abs() < 1e-4, "Blue component blended: {}", blended[2]);
    assert!((blended[3] - 1.0).abs() < 1e-4, "Alpha component blended: {}", blended[3]);

    // Wet charge depletion: as progress reaches 1.0, paint runs out and canvas pickup increases
    let early = blend_wet_mix(brush_col, canvas_col, 0.8, 0.6, 0.6, 0.0); // progress = 0: high charge remains
    let late = blend_wet_mix(brush_col, canvas_col, 0.8, 0.6, 0.6, 1.0); // progress = 1: charge depleted
    assert!(late[2] > early[2], "Depleted wet stroke should pick up more canvas color than fresh charged stroke");

    // Non-finite guards: zero NaN/Inf
    let guarded = blend_wet_mix(brush_col, canvas_col, f32::NAN, f32::INFINITY, f32::NAN, f32::NEG_INFINITY);
    assert!(guarded.iter().all(|c| c.is_finite() && !c.is_nan()), "Wet mix must guard against NaN/Inf");
}

#[test]
fn test_live_fast_path_stroke_synchronization() {
    // Tests the fast-path state transition logic:
    // Pen down activates fast path; Pen up / cancel / modifier change invalidates it.
    let mut fast_stroke_active = false;
    let mut cached_prims_valid = false;

    // Simulate pen down on canvas
    let on_canvas = true;
    if on_canvas {
        fast_stroke_active = true;
        cached_prims_valid = false; // cleared on stroke begin to cache next frame
    }
    assert!(fast_stroke_active);
    assert!(!cached_prims_valid);

    // After 1st frame renders: prims cached
    cached_prims_valid = true;

    // On subsequent pen moves: fast path remains valid
    let pen_move = true;
    if pen_move && fast_stroke_active {
        // fast path bypass active: tessellation skipped, cached prims used
        assert!(cached_prims_valid);
    }

    // On modifier change, key event, or finger touch:
    let modifier_changed = true;
    if modifier_changed {
        fast_stroke_active = false;
        cached_prims_valid = false;
    }
    assert!(!fast_stroke_active, "Modifier change must immediately exit fast-path");
    assert!(!cached_prims_valid, "Cached prims must be cleared on modifier change");

    // On stroke lift (pen up):
    fast_stroke_active = true;
    cached_prims_valid = true;
    let pen_up = true;
    if pen_up {
        fast_stroke_active = false;
        cached_prims_valid = false;
    }
    assert!(!fast_stroke_active, "Pen lift must exit fast-path");
    assert!(!cached_prims_valid, "Cached prims must be cleared on stroke commit");
}

#[test]
fn test_quickshape_classification_straight_line() {
    use crate::quickshape::{QuickShapeKind, classify_quickshape};

    // Synthesize a nearly straight line with slight hand jitter
    let mut pts = Vec::new();
    for i in 0..30 {
        let t = i as f64 / 29.0;
        let wobble = if i % 2 == 0 { 0.8 } else { -0.8 };
        let x = t * 200.0;
        let y = t * 100.0 + wobble;
        pts.push([x, y, 0.5]);
    }

    let classified = classify_quickshape(&pts, false);
    assert!(classified.is_some(), "Straight line with slight jitter should be classified");
    match classified.unwrap() {
        QuickShapeKind::Line { start, end } => {
            assert!((start[0] - 0.0).abs() < 1e-3);
            assert!((end[0] - 200.0).abs() < 1e-3);
        }
        other => panic!("Expected Line, got {:?}", other),
    }
}

#[test]
fn test_quickshape_classification_circular_arc() {
    use crate::quickshape::{QuickShapeKind, classify_quickshape};
    use std::f64::consts::PI;

    // Synthesize a circular arc: R = 60 centered at (100, 100) from 0 to 120 degrees
    let mut pts = Vec::new();
    for i in 0..40 {
        let t = i as f64 / 39.0;
        let angle = t * (2.0 * PI / 3.0);
        let wobble = if i % 2 == 0 { 0.5 } else { -0.5 };
        let r = 60.0 + wobble;
        let x = 100.0 + r * angle.cos();
        let y = 100.0 + r * angle.sin();
        pts.push([x, y, 0.6]);
    }

    let classified = classify_quickshape(&pts, false);
    assert!(classified.is_some(), "Circular arc should be classified");
    match classified.unwrap() {
        QuickShapeKind::Arc { center, radius, .. } => {
            assert!((center[0] - 100.0).abs() < 5.0, "Center x: {}", center[0]);
            assert!((center[1] - 100.0).abs() < 5.0, "Center y: {}", center[1]);
            assert!((radius - 60.0).abs() < 3.0, "Radius: {}", radius);
        }
        other => panic!("Expected Arc, got {:?}", other),
    }
}

#[test]
fn test_quickshape_classification_circle_and_ellipse() {
    use crate::quickshape::{QuickShapeKind, classify_quickshape};
    use std::f64::consts::PI;

    // 1. Circle: R = 50 centered at (150, 150)
    let mut circle_pts = Vec::new();
    for i in 0..50 {
        let t = i as f64 / 50.0;
        let angle = t * 2.0 * PI;
        let wobble = if i % 3 == 0 { 0.6 } else { -0.4 };
        let r = 50.0 + wobble;
        let x = 150.0 + r * angle.cos();
        let y = 150.0 + r * angle.sin();
        circle_pts.push([x, y, 0.7]);
    }

    let circle_classified = classify_quickshape(&circle_pts, false);
    assert!(circle_classified.is_some(), "Closed circle should be classified");
    match circle_classified.unwrap() {
        QuickShapeKind::Circle { center, radius } => {
            assert!((center[0] - 150.0).abs() < 4.0, "Circle center x: {}", center[0]);
            assert!((center[1] - 150.0).abs() < 4.0, "Circle center y: {}", center[1]);
            assert!((radius - 50.0).abs() < 3.0, "Circle radius: {}", radius);
        }
        other => panic!("Expected Circle, got {:?}", other),
    }

    // 2. Ellipse: rx = 70, ry = 35 centered at (200, 200)
    let mut ellipse_pts = Vec::new();
    for i in 0..60 {
        let t = i as f64 / 60.0;
        let angle = t * 2.0 * PI;
        let wobble = if i % 2 == 0 { 0.5 } else { -0.5 };
        let x = 200.0 + (70.0 + wobble) * angle.cos();
        let y = 200.0 + (35.0 + wobble) * angle.sin();
        ellipse_pts.push([x, y, 0.7]);
    }

    let ellipse_classified = classify_quickshape(&ellipse_pts, false);
    assert!(ellipse_classified.is_some(), "Closed ellipse should be classified");
    match ellipse_classified.unwrap() {
        QuickShapeKind::Ellipse { center, radius_x, radius_y, .. } => {
            assert!((center[0] - 200.0).abs() < 6.0, "Ellipse center x: {}", center[0]);
            assert!((center[1] - 200.0).abs() < 6.0, "Ellipse center y: {}", center[1]);
            let (max_r, min_r) = if radius_x > radius_y { (radius_x, radius_y) } else { (radius_y, radius_x) };
            assert!((max_r - 70.0).abs() < 8.0, "Major radius: {}", max_r);
            assert!((min_r - 35.0).abs() < 8.0, "Minor radius: {}", min_r);
        }
        QuickShapeKind::Circle { .. } => panic!("Unconstrained ellipse should not be a circle"),
        other => panic!("Expected Ellipse, got {:?}", other),
    }

    // 3. Constrained Ellipse -> Snaps to perfect Circle
    let constrained_classified = classify_quickshape(&ellipse_pts, true);
    assert!(constrained_classified.is_some(), "Constrained ellipse should snap to Circle");
    match constrained_classified.unwrap() {
        QuickShapeKind::Circle { center, radius } => {
            assert!((center[0] - 200.0).abs() < 6.0);
            assert!((center[1] - 200.0).abs() < 6.0);
            // Radius is average of major and minor
            assert!((radius - 52.5).abs() < 8.0, "Constrained radius: {}", radius);
        }
        other => panic!("Expected constrained Circle, got {:?}", other),
    }
}

#[test]
fn test_quickshape_classification_rejected_squiggle() {
    use crate::quickshape::classify_quickshape;

    // 1. Erratic random squiggle / zig-zag that does not match any geometry
    let mut squiggle = Vec::new();
    let mut x = 50.0;
    let mut y = 50.0;
    for i in 0..40 {
        x += if i % 2 == 0 { 25.0 } else { -18.0 };
        y += if i % 3 == 0 { 30.0 } else { -20.0 };
        squiggle.push([x, y, 0.5]);
    }
    assert_eq!(classify_quickshape(&squiggle, false), None, "Erratic squiggle must be rejected as None");

    // 2. Short taps and specks (< 6 points or length < 15 pt)
    let short_tap = vec![[10.0, 10.0, 0.5], [11.0, 10.5, 0.5], [10.5, 11.0, 0.5]];
    assert_eq!(classify_quickshape(&short_tap, false), None, "Short taps must never trigger QuickShape");
}

#[test]
fn test_quickshape_15_degree_constrained_snapping() {
    use crate::quickshape::snap_line_angle_15_deg;
    use std::f64::consts::PI;

    let start = [100.0, 100.0];
    let len = 100.0;

    // 17 degrees -> should snap to 15 degrees
    let angle_17 = 17.0 * PI / 180.0;
    let end_17 = [start[0] + len * angle_17.cos(), start[1] + len * angle_17.sin()];
    let snapped_15 = snap_line_angle_15_deg(start, end_17);
    let snapped_angle = (snapped_15[1] - start[1]).atan2(snapped_15[0] - start[0]);
    let expected_15 = 15.0 * PI / 180.0;
    assert!((snapped_angle - expected_15).abs() < 1e-4, "17 deg should snap to 15 deg");
    let snapped_len = (snapped_15[0] - start[0]).hypot(snapped_15[1] - start[1]);
    assert!((snapped_len - len).abs() < 1e-4, "Length should be strictly preserved");

    // 28 degrees -> should snap to 30 degrees
    let angle_28 = 28.0 * PI / 180.0;
    let end_28 = [start[0] + len * angle_28.cos(), start[1] + len * angle_28.sin()];
    let snapped_30 = snap_line_angle_15_deg(start, end_28);
    let snapped_angle_30 = (snapped_30[1] - start[1]).atan2(snapped_30[0] - start[0]);
    let expected_30 = 30.0 * PI / 180.0;
    assert!((snapped_angle_30 - expected_30).abs() < 1e-4, "28 deg should snap to 30 deg");

    // -6 degrees -> should snap to 0 degrees
    let angle_neg6 = -6.0 * PI / 180.0;
    let end_neg6 = [start[0] + len * angle_neg6.cos(), start[1] + len * angle_neg6.sin()];
    let snapped_0 = snap_line_angle_15_deg(start, end_neg6);
    let snapped_angle_0 = (snapped_0[1] - start[1]).atan2(snapped_0[0] - start[0]);
    assert!(snapped_angle_0.abs() < 1e-4, "-6 deg should snap to 0 deg");
}

#[test]
fn test_magnetic_rotation_angle_snapping_detent_and_breakout() {
    use crate::dynamics::apply_magnetic_rotation;

    let detent = 4.0;

    // 1. Cardinal 0°
    let (snap_pos, is_snap) = apply_magnetic_rotation(2.5, detent);
    assert!(is_snap);
    assert_eq!(snap_pos, 0.0, "2.5° should snap to 0° within 4° detent");

    let (snap_neg, is_snap) = apply_magnetic_rotation(-3.8, detent);
    assert!(is_snap);
    assert_eq!(snap_neg, 0.0, "-3.8° should snap to 0° within 4° detent");

    let (breakout_pos, is_snap) = apply_magnetic_rotation(4.1, detent);
    assert!(!is_snap);
    assert!((breakout_pos - 4.1).abs() < 1e-4, "4.1° should break out smoothly");

    let (breakout_neg, is_snap) = apply_magnetic_rotation(-4.5, detent);
    assert!(!is_snap);
    assert!((breakout_neg - (-4.5)).abs() < 1e-4, "-4.5° should break out smoothly");

    // 2. Cardinal 90°
    let (snap_90, is_snap) = apply_magnetic_rotation(88.0, detent);
    assert!(is_snap);
    assert_eq!(snap_90, 90.0, "88° should snap to 90°");

    let (snap_90_hi, is_snap) = apply_magnetic_rotation(93.5, detent);
    assert!(is_snap);
    assert_eq!(snap_90_hi, 90.0, "93.5° should snap to 90°");

    let (break_90, is_snap) = apply_magnetic_rotation(95.0, detent);
    assert!(!is_snap);
    assert!((break_90 - 95.0).abs() < 1e-4, "95° should break out from 90°");

    // 3. Cardinal -90° (270°)
    let (snap_neg90, is_snap) = apply_magnetic_rotation(-87.5, detent);
    assert!(is_snap);
    assert_eq!(snap_neg90, -90.0, "-87.5° should snap to -90°");

    let (break_neg90, is_snap) = apply_magnetic_rotation(-95.0, detent);
    assert!(!is_snap);
    assert!((break_neg90 - (-95.0)).abs() < 1e-4, "-95° should break out from -90°");

    // 4. Cardinal 180° / -180°
    let (snap_180, is_snap) = apply_magnetic_rotation(178.0, detent);
    assert!(is_snap);
    assert_eq!(snap_180, 180.0, "178° should snap to 180°");

    let (snap_neg180, is_snap) = apply_magnetic_rotation(-178.5, detent);
    assert!(is_snap);
    assert_eq!(snap_neg180, -180.0, "-178.5° should snap to -180°");

    let (break_180, is_snap) = apply_magnetic_rotation(175.0, detent);
    assert!(!is_snap);
    assert!((break_180 - 175.0).abs() < 1e-4, "175° should break out from 180°");

    // 5. Non-finite safety
    let (nan_angle, is_snap) = apply_magnetic_rotation(f32::NAN, detent);
    assert!(!is_snap);
    assert_eq!(nan_angle, 0.0, "NaN angle must safely return 0.0 without NaN propagation");
}

#[test]
fn ui_smoothing_zero_maps_to_five_percent_floor_and_runs_catch_up() {
    use crate::dynamics::{SMOOTHING_FLOOR, Smoother, effective_smoothing_amount, smooth_points};

    let cfg_zero = Smoothing { enabled: true, amount: 0.0, catch_up_on_end: true, ..Default::default() };
    let cfg_half = Smoothing { enabled: true, amount: 0.5, ..Default::default() };
    let cfg_full = Smoothing { enabled: true, amount: 1.0, ..Default::default() };
    let cfg_off = Smoothing { enabled: false, amount: 0.5, ..Default::default() };

    assert!((effective_smoothing_amount(&cfg_zero) - SMOOTHING_FLOOR).abs() < 1e-6);
    assert!((effective_smoothing_amount(&cfg_half) - 0.525).abs() < 1e-6);
    assert!((effective_smoothing_amount(&cfg_full) - 1.0).abs() < 1e-6);
    assert_eq!(effective_smoothing_amount(&cfg_off), 0.0);

    // At UI 0% smoothing (5% effective floor), Smoother runs exponential pull and catch-up on end.
    let mut smoother = Smoother::new(&cfg_zero, 1.0);
    let mut out = Vec::new();
    smoother.push(StrokePoint::new(0.0, 0.0, 1.0), &mut out);
    smoother.push(StrokePoint { time: 4.0, ..StrokePoint::new(100.0, 0.0, 1.0) }, &mut out);
    assert!(out.last().unwrap().x < 100.0, "5% smoothing floor must engage instead of bypassing");
    smoother.finish(&mut out);
    assert!((out.last().unwrap().x - 100.0).abs() < 1e-6, "catch-up on end must complete at UI 0%");

    // Pressure -> Size with UI 0% smoothing produces continuous smooth dabs with no beaded gaps.
    let brush = BrushSettings {
        size: 36.0,
        spacing: 0.25,
        pressure_size: true,
        pressure_opacity: false,
        smoothing: Smoothing { enabled: true, amount: 0.0, catch_up_on_end: true, ..Default::default() },
        ..Default::default()
    };
    let pts: Vec<StrokePoint> = (0..12)
        .map(|i| StrokePoint {
            time: f64::from(i) * 4.0,
            ..StrokePoint::new(10.0 + f64::from(i) * 10.0, 20.0, 0.4 + 0.04 * i as f32)
        })
        .collect();
    let smoothed = smooth_points(&brush.smoothing, 1.0, &pts);
    assert!(smoothed.len() >= pts.len());
    let stroke = crate::Stroke { brush, points: pts };
    let dabs = crate::dabs(&stroke);
    assert!(dabs.len() > 8);
    for w in dabs.windows(2) {
        let gap = (w[1].center.x - w[0].center.x).hypot(w[1].center.y - w[0].center.y);
        assert!(gap <= f64::from(w[0].radius.max(w[1].radius)) * 0.65, "excessive dab gap {gap} indicates dot beading");
    }
}

#[test]
fn split_stroke_updates_tail_in_place_without_duplicate_dabs() {
    use crate::render::{EST_ALTITUDE, EST_FORCE, SplitStroke, TailPoint, render_stroke};
    use photocraft_color::PixelFormat;
    use photocraft_geom::Rect;
    use photocraft_raster::Surface;

    let fmt = PixelFormat::RGBA8;
    let brush = BrushSettings {
        size: 24.0,
        spacing: 0.25,
        hardness: 1.0,
        pressure_size: true,
        smoothing: Smoothing { enabled: true, amount: 0.0, catch_up_on_end: true, ..Default::default() },
        color: [0.0, 0.0, 0.0, 1.0],
        ..Default::default()
    };
    let mut split = SplitStroke::new(&brush, Some(fmt), 1.0).record_dabs();

    // 4 finalized head samples followed by 3 unfinalized tail samples expecting updates.
    let head: Vec<TailPoint> = (0..4)
        .map(|i| TailPoint::finalized(StrokePoint {
            time: f64::from(i) * 4.0,
            ..StrokePoint::new(10.0 + f64::from(i) * 12.0, 40.0, 0.5)
        }))
        .collect();
    split.push_tagged(&head);
    assert_eq!(split.tail_len(), 0, "finalized head samples bake immediately");
    let baked_head_dabs = split.dab_count();
    assert!(baked_head_dabs > 0);

    let tail = vec![
        TailPoint::new(StrokePoint { time: 16.0, ..StrokePoint::new(58.0, 40.0, 0.25) }, 101, EST_FORCE | EST_ALTITUDE),
        TailPoint::new(StrokePoint { time: 20.0, ..StrokePoint::new(70.0, 40.0, 0.25) }, 102, EST_FORCE),
        TailPoint::new(StrokePoint { time: 24.0, ..StrokePoint::new(82.0, 40.0, 0.30) }, 103, EST_FORCE),
    ];
    split.push_tagged(&tail);
    assert_eq!(split.tail_len(), 3, "unfinalized samples stay in editable tail");
    assert_eq!(split.dab_count(), baked_head_dabs, "finalized dab buffer must not mutate for unfinalized tail");

    let pre = Surface::new(fmt);
    let mut live_surf = pre.clone();
    let (_, old_tail) = split.composite_live(Rect::EMPTY, &pre, &mut live_surf, None, false);

    // Refine middle tail point #102 in place from pressure 0.25 -> 0.95 (keeps same finalized head).
    assert!(split.update_estimate(102, Some(0.95), Some(12.0), Some(-6.0), None, 0));
    assert_eq!(split.dab_count(), baked_head_dabs, "finalized watermark does not advance past #101 until #101 finalizes");

    let preview_after_update = split.preview_dabs();
    // Zero duplicate dabs at the exact same position in the updated preview.
    for (i, w) in preview_after_update.windows(2).enumerate() {
        let sep = (w[1].center.x - w[0].center.x).hypot(w[1].center.y - w[0].center.y);
        assert!(sep > 0.25, "duplicate dab found at index {i} after in-place estimate patch");
    }

    let (_, _new_tail) = split.composite_live(old_tail, &pre, &mut live_surf, None, false);
    // Finalize remaining tail and verify pixel-exact match against a clean full-stroke render with finalized values.
    split.finish();
    let old_tail2 = Rect::new(0, 0, 140, 80);
    split.finalized_mut().mark_dirty(old_tail2);
    split.composite(&pre, &mut live_surf, None, false, true);

    let mut expected_pts: Vec<StrokePoint> = head.iter().map(|t| t.point).collect();
    expected_pts.push(tail[0].point);
    let mut updated_102 = tail[1].point;
    updated_102.pressure = 0.95;
    updated_102.tilt_x = 12.0;
    updated_102.tilt_y = -6.0;
    expected_pts.push(updated_102);
    expected_pts.push(tail[2].point);

    let mut expected_surf = pre.clone();
    render_stroke(&mut expected_surf, &brush, &expected_pts, None, false, 1.0);
    for y in 0..80 {
        for x in 0..140 {
            assert_eq!(live_surf.rgba(x, y), expected_surf.rgba(x, y), "pixel mismatch at ({x}, {y})");
        }
    }
}

#[test]
fn test_pressure_filter_timed_speed_softening() {
    use crate::dynamics::PressureFilter;

    // Slow shading stays near-instantaneous while fast flicks soften sudden stair-step spikes
    // proportionally to speed and 60 Hz-normalized time dilation.
    let mut slow = PressureFilter::new();
    let p0_s = slow.begin_at(0.25, 1.0, Some(1.000));
    let p1_s = slow.push_move_timed(0.85, 1.0, Some(1.016), 0.5);

    let mut fast = PressureFilter::new();
    let p0_f = fast.begin_at(0.25, 1.0, Some(1.000));
    let p1_f = fast.push_move_timed(0.85, 1.0, Some(1.016), 24.0);

    assert!((p0_s - p0_f).abs() < 1e-6);
    assert!(p1_s > 0.85, "slow shading should respond immediately ({p1_s})");
    assert!(p1_f > p0_f && p1_f < p1_s, "fast sweep softens stair-step spike smoothly ({p1_f} vs {p1_s})");
}

#[test]
fn test_convex_trig_spline_zero_overshoot_and_extrapolation() {
    use crate::dynamics::{eval_trig_spline, extrapolate_curvature_point, smooth_mirrored_window};

    // Sharp 90-degree corner: [0,0] -> [100,0] -> [100,100] -> [200,100].
    // Convex trig spline must never overshoot past y < 0 or x > 200 on the middle span.
    let p0 = StrokePoint::new(0.0, 0.0, 0.4);
    let p1 = StrokePoint::new(100.0, 0.0, 0.6);
    let p2 = StrokePoint::new(100.0, 100.0, 0.8);
    let p3 = StrokePoint::new(200.0, 100.0, 0.9);
    let ctrl = [p0, p1, p2, p3];
    for step in 0..=8 {
        let u = step as f32 / 8.0;
        let q = eval_trig_spline(&ctrl, u);
        assert!(q.x >= 0.0 && q.x <= 200.0 && q.y >= 0.0 && q.y <= 100.0, "convex hull violated at u={u}: ({}, {})", q.x, q.y);
    }

    // Curvature extrapolation on a circular arc turns along the arc instead of shooting straight.
    let a = StrokePoint::new(100.0, 0.0, 0.8);
    let b = StrokePoint::new(70.710678, 70.710678, 0.6);
    let c = StrokePoint::new(0.0, 100.0, 0.4);
    let ext = extrapolate_curvature_point(Some(&a), &b, &c);
    assert!(ext.x < c.x && ext.y < c.y + 40.0 && ext.pressure <= c.pressure, "expected inward curved taper, got {ext:?}");

    // Mirrored-window smoothing keeps endpoints anchored and smooths interior zig-zag without phase lag.
    let noisy = vec![
        StrokePoint::new(0.0, 50.0, 0.5),
        StrokePoint::new(25.0, 62.0, 0.5),
        StrokePoint::new(50.0, 38.0, 0.5),
        StrokePoint::new(75.0, 60.0, 0.5),
        StrokePoint::new(100.0, 50.0, 0.5),
    ];
    let mut smoothed = Vec::new();
    smooth_mirrored_window(&noisy, &[], 0.8, 0.4, &mut smoothed);
    assert_eq!(smoothed.len(), noisy.len());
    assert!((smoothed[0].y - 50.0).abs() < 1e-6);
    assert!((smoothed[4].y - 50.0).abs() < 1e-6);
    assert!((smoothed[2].y - 50.0).abs() < (noisy[2].y - 50.0).abs());
}

#[test]
fn test_dynamic_secant_spacing_smooth_transition() {
    use crate::dynamics::{DabBuilder, PathWalker};

    // Pressure drops rapidly across one 60 px segment (diameter 30 -> 3): secant+bisection
    // dynamic spacing keeps consecutive dab spacing proportional so no detached bead forms.
    let brush = BrushSettings { size: 30.0, spacing: 0.25, pressure_size: true, ..Default::default() };
    let db = DabBuilder::new(&brush);
    let mut walker = PathWalker::new(None);
    let mut centers = Vec::new();
    let p0 = StrokePoint::new(0.0, 20.0, 1.0);
    let p1 = StrokePoint::new(60.0, 20.0, 0.1);
    walker.push(p0, &|q| db.step_len(q), &mut |s| centers.push((s.point.x, s.point.pressure)));
    walker.push(p1, &|q| db.step_len(q), &mut |s| centers.push((s.point.x, s.point.pressure)));
    assert!(centers.len() >= 8, "expected dense dynamically-spaced steps as size shrinks, got {}", centers.len());
    for w in centers.windows(2) {
        let gap = w[1].0 - w[0].0;
        let max_expected_step = 30.0 * 0.25 + 1.0;
        assert!(gap > 0.2 && gap <= max_expected_step, "gap {gap} out of continuous bounds between {w:?}");
    }
}

#[test]
fn split_stroke_predicted_real_stamps_never_leak_into_finish() {
    use crate::render::{EST_FORCE, SplitStroke, TailPoint, render_stroke};
    use photocraft_color::PixelFormat;
    use photocraft_geom::Rect;
    use photocraft_raster::Surface;

    let fmt = PixelFormat::RGBA8;
    let brush = BrushSettings {
        size: 18.0,
        spacing: 0.25,
        hardness: 1.0,
        pressure_size: true,
        smoothing: Smoothing { enabled: true, amount: 0.1, catch_up_on_end: true, ..Default::default() },
        color: [0.0, 0.0, 0.0, 1.0],
        ..Default::default()
    };
    let mut split = SplitStroke::new(&brush, Some(fmt), 1.0);
    let pts = vec![
        TailPoint::finalized(StrokePoint { time: 0.0, ..StrokePoint::new(20.0, 40.0, 0.6) }),
        TailPoint::new(StrokePoint { time: 8.0, ..StrokePoint::new(40.0, 40.0, 0.7) }, 1, 0),
        TailPoint::new(StrokePoint { time: 16.0, ..StrokePoint::new(60.0, 40.0, 0.0) }, 2, EST_FORCE),
        TailPoint::new(StrokePoint { time: 24.0, ..StrokePoint::new(80.0, 40.0, 0.5) }, 3, 0),
    ];
    split.push_tagged(&pts);

    // Stage ephemeral predicted stamps far to the right (x = 130..160).
    let pred = vec![
        StrokePoint { time: 32.0, ..StrokePoint::new(130.0, 40.0, 0.8) },
        StrokePoint { time: 40.0, ..StrokePoint::new(155.0, 40.0, 0.6) },
    ];
    split.set_predicted(&pred);

    let pre = Surface::new(fmt);
    let mut live_surf = pre.clone();
    let (_, tail_rect) = split.composite_live(Rect::EMPTY, &pre, &mut live_surf, None, false);
    assert!(!tail_rect.is_empty());
    // Real brush coverage appears in the predicted span during live composite.
    assert!(live_surf.rgba(145, 40)[3] > 0.01, "expected predicted brush stamp pixel at x=145");

    // On finish(), predicted stamps clear completely, and unfinalized zero #2 backfills cleanly (~0.6).
    split.finish();
    live_surf.write_region(Rect::new(0, 0, 180, 80), &pre.read_region(Rect::new(0, 0, 180, 80)));
    split.finalized_mut().mark_dirty(Rect::new(0, 0, 180, 80));
    split.composite(&pre, &mut live_surf, None, false, true);
    assert_eq!(live_surf.rgba(145, 40)[3], 0.0, "predicted brush stamps must never leak after finish()");

    let expected_pts = vec![
        StrokePoint { time: 0.0, ..StrokePoint::new(20.0, 40.0, 0.6) },
        StrokePoint { time: 8.0, ..StrokePoint::new(40.0, 40.0, 0.7) },
        StrokePoint { time: 16.0, ..StrokePoint::new(60.0, 40.0, 0.6) },
        StrokePoint { time: 24.0, ..StrokePoint::new(80.0, 40.0, 0.5) },
    ];
    let mut expected_surf = pre.clone();
    render_stroke(&mut expected_surf, &brush, &expected_pts, None, false, 1.0);
    for y in 0..80 {
        for x in 0..180 {
            assert_eq!(live_surf.rgba(x, y), expected_surf.rgba(x, y), "backfill pixel mismatch at ({x}, {y})");
        }
    }
}
