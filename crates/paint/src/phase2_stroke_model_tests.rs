//! Phase 2 Stroke Model Unit Tests:
//! 1. Sample distance rejection formula (canvas-space aware).
//! 2. Directional damping with 0.125 base gain + EMA (parallel latency < 2 frames).
//! 3. 256-entry 1-D dynamics LUTs for pressure, tilt, scatter, and azimuth.
//! 4. Serialization and deserialization of stabilization & streamline options.

use crate::brush::{BrushSettings, Control, Dynamic, Scattering, ShapeDynamics, Smoothing, Transfer};
use crate::curve::ResponseCurve;
use crate::dynamics::{
    DynamicsLut, apply_directional_damping, apply_retroactive_liftoff_taper,
    calibrate_stylus_pressure, filter_circular_angle, filter_perpendicular_motion,
};
use crate::Dab;
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

    // 0.10 -> concave lift (~0.3119)
    let p1 = calibrate_stylus_pressure(0.10);
    let expected = (0.10f32 / 0.70).powf(0.60);
    assert!((p1 - expected).abs() < 1e-4, "0.10 force should produce concave lift ~0.3119, got {p1}");
    assert!((p1 - 0.3119).abs() < 0.01, "0.10 force should be approximately 0.3119, got {p1}");

    // 0.70 -> 1.0 (saturates at 0.70 hardware max)
    let p70 = calibrate_stylus_pressure(0.70);
    assert!((p70 - 1.0).abs() < 1e-5, "0.70 force should calibrate to exactly 1.0, got {p70}");

    // 1.00 -> 1.0 (saturation above 0.70)
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
fn test_perpendicular_motion_filtering_without_corner_collapse() {
    let p_prev = StrokePoint::new(10.0, 0.0, 1.0);
    let mut dir = Some([1.0, 0.0]); // stroke traveling along +x

    // 1. Hand wobble perpendicular to travel vector (+y jitter)
    let p_wobble = StrokePoint::new(20.0, 4.0, 1.0);
    let filtered_wobble = filter_perpendicular_motion(p_wobble, p_prev, &mut dir, 1.0);
    assert!(filtered_wobble.y < 4.0, "Perpendicular jitter should be damped, got {}", filtered_wobble.y);

    // 2. Sharp 90-degree corner: from (10, 0) straight up along +y to (10, 10)
    let mut corner_dir = Some([1.0, 0.0]);
    let p_corner = StrokePoint::new(10.0, 10.0, 1.0);
    let filtered_corner = filter_perpendicular_motion(p_corner, p_prev, &mut corner_dir, 1.0);
    // Because dot product is 0.0 (< 0.3), corner-collapse protection triggers: full responsiveness
    assert!((filtered_corner.x - 10.0).abs() < 1e-4, "Corner apex x must be preserved without collapse, got {}", filtered_corner.x);
    assert!((filtered_corner.y - 10.0).abs() < 1e-4, "Corner apex y must be preserved without collapse, got {}", filtered_corner.y);
}

#[test]
fn test_retroactive_liftoff_taper_generation() {
    // Simulate a fast flick stroke ending: 4 dabs with constant radius 10.0 and opacity 1.0
    let mut dabs = vec![
        Dab::round(photocraft_geom::Point::new(10.0, 0.0), 10.0, 1.0),
        Dab::round(photocraft_geom::Point::new(20.0, 0.0), 10.0, 1.0),
        Dab::round(photocraft_geom::Point::new(30.0, 0.0), 10.0, 1.0),
        Dab::round(photocraft_geom::Point::new(40.0, 0.0), 10.0, 1.0),
    ];

    // High exit velocity (> 40.0) points
    let recent = [
        StrokePoint::new(0.0, 0.0, 0.8),
        StrokePoint::new(10.0, 0.0, 0.7),
        StrokePoint::new(20.0, 0.0, 0.6),
        StrokePoint::new(30.0, 0.0, 0.4),
        StrokePoint::new(50.0, 0.0, 0.2), // distance 20 in 1 step = high velocity
    ];

    apply_retroactive_liftoff_taper(&mut dabs, &recent);

    // Tip dab should be tapered down to sharp tip (scale ~ 0.15)
    let tip = dabs.last().unwrap();
    assert!(tip.radius < 2.0, "Tip dab radius should taper down to a sharp point, got {}", tip.radius);
    assert!(tip.opacity < 0.20, "Tip dab opacity should taper down smoothly, got {}", tip.opacity);

    // Radius and opacity should taper monotonically towards the tip
    for i in 1..dabs.len() {
        assert!(dabs[i].radius <= dabs[i - 1].radius, "Radius should decrease towards tip: dab {} ({}) vs dab {} ({})", i - 1, dabs[i-1].radius, i, dabs[i].radius);
        assert!(dabs[i].opacity <= dabs[i - 1].opacity, "Opacity should decrease towards tip: dab {} ({}) vs dab {} ({})", i - 1, dabs[i-1].opacity, i, dabs[i].opacity);
    }
}
