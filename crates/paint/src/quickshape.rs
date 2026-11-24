//! QuickShape (Draw-and-Hold Geometry Snap):
//! Classifies raw brush strokes into geometric primitives (Line, Arc, Circle, Ellipse, Polygon),
//! fits clean geometry with least-squares / chord-deviation thresholds, and generates smoothed,
//! pressure-interpolated stroke paths.

use std::f64::consts::PI;

#[derive(Clone, Debug, PartialEq)]
pub enum QuickShapeKind {
    Line {
        start: [f64; 2],
        end: [f64; 2],
    },
    Arc {
        center: [f64; 2],
        radius: f64,
        start_angle: f64,
        end_angle: f64,
    },
    Circle {
        center: [f64; 2],
        radius: f64,
    },
    Ellipse {
        center: [f64; 2],
        radius_x: f64,
        radius_y: f64,
        rotation: f64,
    },
    Polygon {
        vertices: Vec<[f64; 2]>,
        closed: bool,
    },
}

/// Snaps the line angle to the nearest multiple of 15 degrees (PI / 12 rad).
pub fn snap_line_angle_15_deg(start: [f64; 2], end: [f64; 2]) -> [f64; 2] {
    let dx = end[0] - start[0];
    let dy = end[1] - start[1];
    let len = dx.hypot(dy);
    if !len.is_finite() || len < 1e-4 {
        return end;
    }
    let angle = dy.atan2(dx);
    let step = PI / 12.0; // 15 degrees
    let snapped_angle = (angle / step).round() * step;
    [
        start[0] + len * snapped_angle.cos(),
        start[1] + len * snapped_angle.sin(),
    ]
}

/// Compute total Euclidean arc length along a sequence of points.
pub fn path_length(pts: &[[f64; 3]]) -> f64 {
    if pts.len() < 2 {
        return 0.0;
    }
    let mut total = 0.0;
    for i in 1..pts.len() {
        let dx = pts[i][0] - pts[i - 1][0];
        let dy = pts[i][1] - pts[i - 1][1];
        total += dx.hypot(dy);
    }
    total
}

/// Perpendicular distance from point p to segment ab.
fn perpendicular_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return (p[0] - a[0]).hypot(p[1] - a[1]);
    }
    ((dx * (a[1] - p[1]) - (a[0] - p[0]) * dy).abs()) / len
}

/// Classify raw stroke points into the closest geometric primitive.
/// Returns None if deviation exceeds error thresholds (leaving stroke freehand).
pub fn classify_quickshape(raw_points: &[[f64; 3]], constrain: bool) -> Option<QuickShapeKind> {
    if raw_points.len() < 6 {
        return None;
    }
    let total_len = path_length(raw_points);
    if total_len < 15.0 {
        return None;
    }

    let p_start = [raw_points.first()?[0], raw_points.first()?[1]];
    let p_end = [raw_points.last()?[0], raw_points.last()?[1]];
    let chord_len = (p_end[0] - p_start[0]).hypot(p_end[1] - p_start[1]);

    // 1. Straight Line Check:
    if chord_len >= 12.0 {
        let mut max_dev = 0.0f64;
        let mut sum_sq_dev = 0.0f64;
        for p in raw_points {
            let d = perpendicular_distance([p[0], p[1]], p_start, p_end);
            if d > max_dev {
                max_dev = d;
            }
            sum_sq_dev += d * d;
        }
        let rms_dev = (sum_sq_dev / raw_points.len() as f64).sqrt();

        // Must stay close to the straight chord
        let chord_ratio = max_dev / chord_len;
        if chord_ratio < 0.10 && rms_dev < 12.0 && max_dev < 24.0 {
            let end = if constrain {
                snap_line_angle_15_deg(p_start, p_end)
            } else {
                p_end
            };
            return Some(QuickShapeKind::Line { start: p_start, end });
        }
    }

    let is_closed = chord_len <= 0.25 * total_len || (chord_len <= 35.0 && total_len > 70.0);

    // 2. Closed Shape Check: Circle or Ellipse
    if is_closed {
        let n = raw_points.len() as f64;
        let mut cx = 0.0f64;
        let mut cy = 0.0f64;
        for p in raw_points {
            cx += p[0];
            cy += p[1];
        }
        cx /= n;
        cy /= n;

        // Radii from centroid
        let mut sum_r = 0.0f64;
        let mut sum_sq_r = 0.0f64;
        for p in raw_points {
            let r = (p[0] - cx).hypot(p[1] - cy);
            sum_r += r;
            sum_sq_r += r * r;
        }
        let mean_r = sum_r / n;
        let var_r = (sum_sq_r / n - mean_r * mean_r).max(0.0);
        let std_r = var_r.sqrt();
        let rel_circle_err = std_r / mean_r.max(1e-4);

        if rel_circle_err < 0.12 && mean_r >= 6.0 {
            return Some(QuickShapeKind::Circle {
                center: [cx, cy],
                radius: mean_r,
            });
        }

        // Ellipse fitting via central second moments
        let mut m_xx = 0.0f64;
        let mut m_yy = 0.0f64;
        let mut m_xy = 0.0f64;
        for p in raw_points {
            let dx = p[0] - cx;
            let dy = p[1] - cy;
            m_xx += dx * dx;
            m_yy += dy * dy;
            m_xy += dx * dy;
        }
        m_xx /= n;
        m_yy /= n;
        m_xy /= n;

        let trace = m_xx + m_yy;
        let det = m_xx * m_yy - m_xy * m_xy;
        let disc = ((trace * 0.5) * (trace * 0.5) - det).max(0.0).sqrt();
        let lambda1 = trace * 0.5 + disc;
        let lambda2 = (trace * 0.5 - disc).max(0.0);

        let rx = (2.0 * lambda1).sqrt();
        let ry = (2.0 * lambda2).sqrt();
        let rot = 0.5 * (2.0 * m_xy).atan2(m_xx - m_yy);

        if rx >= 6.0 && ry >= 4.0 {
            // Evaluate RMS algebraic/geometric distance to ellipse
            let cos_rot = rot.cos();
            let sin_rot = rot.sin();
            let mut sum_sq_err = 0.0f64;
            for p in raw_points {
                let dx = p[0] - cx;
                let dy = p[1] - cy;
                let px = dx * cos_rot + dy * sin_rot;
                let py = -dx * sin_rot + dy * cos_rot;
                let dist = (px / rx).hypot(py / ry) - 1.0;
                sum_sq_err += dist * dist;
            }
            let rms_ellipse = (sum_sq_err / n).sqrt();
            if rms_ellipse < 0.16 {
                if constrain {
                    let avg_r = (rx + ry) * 0.5;
                    return Some(QuickShapeKind::Circle {
                        center: [cx, cy],
                        radius: avg_r,
                    });
                } else {
                    return Some(QuickShapeKind::Ellipse {
                        center: [cx, cy],
                        radius_x: rx,
                        radius_y: ry,
                        rotation: rot,
                    });
                }
            }
        }
    }

    // 3. Circular Arc Check (open curved stroke)
    if !is_closed && total_len >= 20.0 {
        // Kåsa circle fit
        let n = raw_points.len() as f64;
        let mut sum_x = 0.0f64;
        let mut sum_y = 0.0f64;
        let mut sum_x2 = 0.0f64;
        let mut sum_y2 = 0.0f64;
        let mut sum_xy = 0.0f64;
        let mut sum_z = 0.0f64;
        let mut sum_xz = 0.0f64;
        let mut sum_yz = 0.0f64;

        for p in raw_points {
            let x = p[0];
            let y = p[1];
            let z = x * x + y * y;
            sum_x += x;
            sum_y += y;
            sum_x2 += x * x;
            sum_y2 += y * y;
            sum_xy += x * y;
            sum_z += z;
            sum_xz += x * z;
            sum_yz += y * z;
        }

        // Linear system for center [cx, cy]
        let a11 = 2.0 * (n * sum_x2 - sum_x * sum_x);
        let a12 = 2.0 * (n * sum_xy - sum_x * sum_y);
        let b1 = n * sum_xz - sum_x * sum_z;

        let a21 = 2.0 * (n * sum_xy - sum_x * sum_y);
        let a22 = 2.0 * (n * sum_y2 - sum_y * sum_y);
        let b2 = n * sum_yz - sum_y * sum_z;

        let det = a11 * a22 - a12 * a21;
        if det.abs() > 1e-6 {
            let cx = (b1 * a22 - a12 * b2) / det;
            let cy = (a11 * b2 - b1 * a21) / det;
            let mut sum_r = 0.0f64;
            let mut sum_sq_r = 0.0f64;
            for p in raw_points {
                let r = (p[0] - cx).hypot(p[1] - cy);
                sum_r += r;
                sum_sq_r += r * r;
            }
            let mean_r = sum_r / n;
            let var_r = (sum_sq_r / n - mean_r * mean_r).max(0.0);
            let std_r = var_r.sqrt();
            let rel_err = std_r / mean_r.max(1e-4);

            // Radius must be reasonable compared to path length
            if rel_err < 0.10 && mean_r >= 8.0 && mean_r <= 6.0 * total_len {
                let start_angle = (p_start[1] - cy).atan2(p_start[0] - cx);
                let end_angle = (p_end[1] - cy).atan2(p_end[0] - cx);

                // Ensure angular progression doesn't loop multiple times
                let mut d_theta = end_angle - start_angle;
                while d_theta > PI {
                    d_theta -= 2.0 * PI;
                }
                while d_theta < -PI {
                    d_theta += 2.0 * PI;
                }

                if d_theta.abs() > 0.20 {
                    return Some(QuickShapeKind::Arc {
                        center: [cx, cy],
                        radius: mean_r,
                        start_angle,
                        end_angle: start_angle + d_theta,
                    });
                }
            }
        }
    }

    // High error / erratic squiggle: leave stroke freehand
    None
}

/// Generates smooth, clean stroke samples along the classified primitive,
/// preserving the raw stroke's interpolated pressure profile.
pub fn generate_quickshape_stroke(
    kind: &QuickShapeKind,
    raw_points: &[[f64; 3]],
    constrain: bool,
) -> Vec<[f64; 3]> {
    let target_count = raw_points.len().clamp(24, 180);
    let mut out = Vec::with_capacity(target_count);

    let interp_pressure = |t: f64| -> f64 {
        if raw_points.is_empty() {
            return 1.0;
        }
        let idx = t.clamp(0.0, 1.0) * (raw_points.len() - 1) as f64;
        let i0 = idx.floor() as usize;
        let i1 = idx.ceil() as usize;
        let frac = idx.fract();
        let p0 = raw_points[i0][2];
        let p1 = raw_points[i1][2];
        p0 + (p1 - p0) * frac
    };

    match kind {
        QuickShapeKind::Line { start, end } => {
            let actual_end = if constrain {
                snap_line_angle_15_deg(*start, *end)
            } else {
                *end
            };
            for i in 0..target_count {
                let t = i as f64 / (target_count - 1) as f64;
                let x = start[0] + t * (actual_end[0] - start[0]);
                let y = start[1] + t * (actual_end[1] - start[1]);
                let p = interp_pressure(t);
                out.push([x, y, p]);
            }
        }
        QuickShapeKind::Circle { center, radius } => {
            let count = target_count.max(48);
            for i in 0..=count {
                let t = i as f64 / count as f64;
                let angle = 2.0 * PI * t;
                let x = center[0] + radius * angle.cos();
                let y = center[1] + radius * angle.sin();
                let p = interp_pressure(t);
                out.push([x, y, p]);
            }
        }
        QuickShapeKind::Ellipse {
            center,
            radius_x,
            radius_y,
            rotation,
        } => {
            if constrain {
                let avg_r = (radius_x + radius_y) * 0.5;
                return generate_quickshape_stroke(
                    &QuickShapeKind::Circle {
                        center: *center,
                        radius: avg_r,
                    },
                    raw_points,
                    false,
                );
            }
            let count = target_count.max(48);
            let cos_rot = rotation.cos();
            let sin_rot = rotation.sin();
            for i in 0..=count {
                let t = i as f64 / count as f64;
                let angle = 2.0 * PI * t;
                let lx = radius_x * angle.cos();
                let ly = radius_y * angle.sin();
                let x = center[0] + lx * cos_rot - ly * sin_rot;
                let y = center[1] + lx * sin_rot + ly * cos_rot;
                let p = interp_pressure(t);
                out.push([x, y, p]);
            }
        }
        QuickShapeKind::Arc {
            center,
            radius,
            start_angle,
            end_angle,
        } => {
            for i in 0..target_count {
                let t = i as f64 / (target_count - 1) as f64;
                let angle = start_angle + t * (end_angle - start_angle);
                let x = center[0] + radius * angle.cos();
                let y = center[1] + radius * angle.sin();
                let p = interp_pressure(t);
                out.push([x, y, p]);
            }
        }
        QuickShapeKind::Polygon { vertices, closed } => {
            if vertices.is_empty() {
                return raw_points.to_vec();
            }
            let segs = if *closed {
                vertices.len()
            } else {
                vertices.len().saturating_sub(1)
            };
            let pts_per_seg = (target_count / segs.max(1)).max(4);
            for s in 0..segs {
                let v0 = vertices[s];
                let v1 = vertices[(s + 1) % vertices.len()];
                for i in 0..pts_per_seg {
                    let seg_t = i as f64 / pts_per_seg as f64;
                    let glob_t = (s as f64 + seg_t) / segs as f64;
                    let x = v0[0] + seg_t * (v1[0] - v0[0]);
                    let y = v0[1] + seg_t * (v1[1] - v0[1]);
                    let p = interp_pressure(glob_t);
                    out.push([x, y, p]);
                }
            }
            if let Some(&last) = vertices.last() {
                out.push([last[0], last[1], interp_pressure(1.0)]);
            }
        }
    }
    out
}

/// Dynamically updates the endpoint or scale of an active QuickShape as the nib drags before liftoff.
pub fn update_quickshape_drag(
    kind: &QuickShapeKind,
    raw_points: &[[f64; 3]],
    new_nib: [f64; 2],
    constrain: bool,
) -> (QuickShapeKind, Vec<[f64; 3]>) {
    let updated_kind = match kind {
        QuickShapeKind::Line { start, .. } => {
            let end = if constrain {
                snap_line_angle_15_deg(*start, new_nib)
            } else {
                new_nib
            };
            QuickShapeKind::Line { start: *start, end }
        }
        QuickShapeKind::Circle { center, .. } => {
            let r = (new_nib[0] - center[0]).hypot(new_nib[1] - center[1]);
            QuickShapeKind::Circle {
                center: *center,
                radius: r.max(3.0),
            }
        }
        QuickShapeKind::Ellipse {
            center,
            radius_x,
            radius_y,
            rotation,
        } => {
            let dist = (new_nib[0] - center[0]).hypot(new_nib[1] - center[1]);
            if constrain {
                QuickShapeKind::Circle {
                    center: *center,
                    radius: dist.max(3.0),
                }
            } else {
                let base_r = (radius_x + radius_y) * 0.5;
                let scale = (dist / base_r.max(1.0)).clamp(0.1, 10.0);
                QuickShapeKind::Ellipse {
                    center: *center,
                    radius_x: radius_x * scale,
                    radius_y: radius_y * scale,
                    rotation: *rotation,
                }
            }
        }
        QuickShapeKind::Arc {
            center,
            radius,
            start_angle,
            ..
        } => {
            let new_angle = (new_nib[1] - center[1]).atan2(new_nib[0] - center[0]);
            QuickShapeKind::Arc {
                center: *center,
                radius: *radius,
                start_angle: *start_angle,
                end_angle: new_angle,
            }
        }
        QuickShapeKind::Polygon { vertices, closed } => {
            QuickShapeKind::Polygon {
                vertices: vertices.clone(),
                closed: *closed,
            }
        }
    };
    let stroke = generate_quickshape_stroke(&updated_kind, raw_points, constrain);
    (updated_kind, stroke)
}
