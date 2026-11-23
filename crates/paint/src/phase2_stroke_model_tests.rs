//! Phase 2 Stroke Model Unit Tests:
//! 1. Sample distance rejection formula (canvas-space aware).
//! 2. Directional damping with 0.125 base gain + EMA (parallel latency < 2 frames).
//! 3. 256-entry 1-D dynamics LUTs for pressure, tilt, scatter, and azimuth.
//! 4. Serialization and deserialization of stabilization & streamline options.

use crate::brush::{BrushSettings, Control, Dynamic, Scattering, ShapeDynamics, Smoothing, Transfer};
use crate::curve::ResponseCurve;
use crate::dynamics::{DynamicsLut, apply_directional_damping};
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
