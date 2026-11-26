//! Offline Apple Pencil / stylus touch-stream recording, force-refinement resolution, and replay.
//!
//! Records raw `UITouch` samples `(x, y, force, max_force, is_force_estimated,
//! estimation_update_index, altitude, azimuth, timestamp, phase)` when explicitly enabled
//! (zero allocation / single boolean check when disabled) and replays them deterministically
//! through [`TouchForceResolver`], [`crate::dynamics::PressureFilter`], and
//! [`crate::dynamics::DabGenerator`].

use serde::{Deserialize, Serialize};

use crate::brush::BrushSettings;
use crate::dynamics::{DabGenerator, PressureFilter};
use crate::{Dab, StrokePoint};

/// Raw hardware stylus sample phase matching iOS `MetalView`.
/// `0` = began, `1` = moved, `2` = ended, `3` = cancelled, `4` = estimated properties updated.
pub const PHASE_BEGAN: u8 = 0;
pub const PHASE_MOVED: u8 = 1;
pub const PHASE_ENDED: u8 = 2;
pub const PHASE_CANCELLED: u8 = 3;
pub const PHASE_ESTIMATE_UPDATE: u8 = 4;

/// One raw Pencil touch record captured directly at the platform touch handler.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawTouchRecord {
    pub x: f64,
    pub y: f64,
    pub force: f32,
    pub max_force: f32,
    pub is_force_estimated: bool,
    pub estimation_update_index: Option<u64>,
    pub altitude: f32,
    pub azimuth: f32,
    pub timestamp: f64,
    pub phase: u8,
}

/// Toggleable offline touch-stream recorder (off by default, zero overhead when disabled).
#[derive(Clone, Debug, Default)]
pub struct TouchRecorder {
    enabled: bool,
    records: Vec<RawTouchRecord>,
}

impl TouchRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn clear(&mut self) {
        self.records.clear();
    }

    pub fn records(&self) -> &[RawTouchRecord] {
        &self.records
    }

    #[inline]
    pub fn record(&mut self, sample: RawTouchRecord) {
        if !self.enabled {
            return;
        }
        self.records.push(sample);
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.records)
    }

    pub fn from_json(json: &str) -> Result<Vec<RawTouchRecord>, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn to_csv(&self) -> String {
        let mut out = String::from("x,y,force,max_force,is_force_estimated,estimation_update_index,altitude,azimuth,timestamp,phase\n");
        for r in &self.records {
            let idx = r.estimation_update_index.map_or(String::new(), |v| v.to_string());
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{}\n",
                r.x, r.y, r.force, r.max_force, r.is_force_estimated, idx, r.altitude, r.azimuth, r.timestamp, r.phase
            ));
        }
        out
    }

    pub fn from_csv(csv: &str) -> Result<Vec<RawTouchRecord>, String> {
        let mut records = Vec::new();
        for (line_no, line) in csv.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || (line_no == 0 && trimmed.starts_with("x,")) {
                continue;
            }
            let cols: Vec<&str> = trimmed.split(',').collect();
            if cols.len() != 10 {
                return Err(format!("line {}: expected 10 columns, found {}", line_no + 1, cols.len()));
            }
            let parse_f64 = |i: usize| cols[i].parse::<f64>().map_err(|e| format!("line {} col {}: {e}", line_no + 1, i));
            let parse_f32 = |i: usize| cols[i].parse::<f32>().map_err(|e| format!("line {} col {}: {e}", line_no + 1, i));
            let is_force_estimated = cols[4].parse::<bool>().map_err(|e| format!("line {} col 4: {e}", line_no + 1))?;
            let estimation_update_index = if cols[5].is_empty() {
                None
            } else {
                Some(cols[5].parse::<u64>().map_err(|e| format!("line {} col 5: {e}", line_no + 1))?)
            };
            let phase = cols[9].parse::<u8>().map_err(|e| format!("line {} col 9: {e}", line_no + 1))?;
            records.push(RawTouchRecord {
                x: parse_f64(0)?,
                y: parse_f64(1)?,
                force: parse_f32(2)?,
                max_force: parse_f32(3)?,
                is_force_estimated,
                estimation_update_index,
                altitude: parse_f32(6)?,
                azimuth: parse_f32(7)?,
                timestamp: parse_f64(8)?,
                phase,
            });
        }
        Ok(records)
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingTouch {
    rec: RawTouchRecord,
    raw_norm: f32,
    unknown_force: bool,
    ready: bool,
}

/// Resolves unrefined/placeholder `force` samples in raw Apple Pencil touch streams before any
/// calibration, pressure filtering, or dab generation occurs.
///
/// Mirrors `MetalView.swift`'s exact force-holding policy:
/// - Touchdown (`phase == 0`) samples with estimated or `0.00` force are held up to `INITIAL_HOLD_SECS`
///   (~1–2 frames at 120 Hz) until a refined estimate or confirmed non-zero sample arrives.
/// - Mid-stroke (`phase == 1`) and lift-off (`phase >= 2`) samples with unrefined or `0.00` force
///   immediately substitute `last_confirmed_force` without waiting or dropping to `0.00`.
#[derive(Clone, Debug)]
pub struct TouchForceResolver {
    pending: Vec<PendingTouch>,
    last_confirmed_force: Option<f32>,
    last_valid_dir: Option<[f64; 2]>,
    prev_pos: Option<(f64, f64)>,
    start_ts_s: Option<f64>,
    last_time_ms: f64,
    pressure_filter: PressureFilter,
    pressure_divisor: f32,
}

impl Default for TouchForceResolver {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl TouchForceResolver {
    /// Maximum time (seconds) a `touchesBegan` sample waits for initial force refinement (~2 frames at 120 Hz).
    pub const INITIAL_HOLD_SECS: f64 = 0.016;
    /// Safe non-zero initial force fallback if hardware fails to deliver any confirmed force within the hold window.
    pub const INITIAL_FALLBACK_FORCE: f32 = 0.25;
    const BOGUS_ZERO_THRESHOLD: f32 = 1e-4;
    const MIN_POINT_DIST: f64 = 0.5;
    const MICRO_HOOK_MIN_DIST: f64 = 1.0;
    const MICRO_HOOK_REVERSAL_MAX_DIST: f64 = 8.0;
    const MICRO_HOOK_MIN_DOT: f64 = 0.2;

    pub fn new(pressure_divisor: f32) -> Self {
        Self {
            pending: Vec::new(),
            last_confirmed_force: None,
            last_valid_dir: None,
            prev_pos: None,
            start_ts_s: None,
            last_time_ms: 0.0,
            pressure_filter: PressureFilter::new(),
            pressure_divisor,
        }
    }

    pub fn last_confirmed_force(&self) -> Option<f32> {
        self.last_confirmed_force
    }

    #[inline]
    fn raw_norm_and_unknown(rec: &RawTouchRecord) -> (f32, bool) {
        let raw_norm = if rec.max_force.is_finite() && rec.max_force > 0.0 && rec.force.is_finite() {
            (rec.force / rec.max_force).clamp(0.0, 1.0)
        } else {
            0.5
        };
        let bogus_zero = raw_norm <= Self::BOGUS_ZERO_THRESHOLD;
        (raw_norm, rec.is_force_estimated || bogus_zero)
    }

    fn tilt_from_record(rec: &RawTouchRecord) -> (f32, f32) {
        let alt = f64::from(rec.altitude);
        let az = f64::from(rec.azimuth);
        let upright = alt.sin().max(0.0001);
        let flat = alt.cos();
        let tx = ((az.cos() * flat).atan2(upright) * 180.0 / std::f64::consts::PI) as f32;
        let ty = ((az.sin() * flat).atan2(upright) * 180.0 / std::f64::consts::PI) as f32;
        (tx, ty)
    }

    fn time_ms(&mut self, timestamp_s: f64) -> f64 {
        let t0 = *self.start_ts_s.get_or_insert(timestamp_s);
        let elapsed_ms = if timestamp_s.is_finite() && t0.is_finite() {
            ((timestamp_s - t0) * 1000.0).max(self.last_time_ms)
        } else {
            self.last_time_ms
        };
        self.last_time_ms = elapsed_ms;
        elapsed_ms
    }

    fn emit_confirmed(&mut self, item: PendingTouch, generator: &mut DabGenerator, dabs: &mut Vec<Dab>, dual: &mut Vec<Dab>, out_points: &mut Vec<StrokePoint>) {
        let raw_force = item.raw_norm.max(PressureFilter::MIN_PRESSURE);
        let (tx, ty) = Self::tilt_from_record(&item.rec);
        let mut x = item.rec.x;
        let mut y = item.rec.y;
        match item.rec.phase {
            PHASE_BEGAN => {
                self.prev_pos = Some((x, y));
                self.last_valid_dir = None;
                let p = self.pressure_filter.begin(raw_force, self.pressure_divisor);
                let sp = StrokePoint { x, y, pressure: p, tilt_x: tx, tilt_y: ty, rotation: 0.0, wheel: 1.0, time: self.time_ms(item.rec.timestamp) };
                out_points.push(sp);
                generator.push(&[sp], dabs, dual);
            }
            PHASE_MOVED => {
                if let Some((px, py)) = self.prev_pos {
                    let (dx, dy) = (x - px, y - py);
                    let dist = dx.hypot(dy);
                    if dist < Self::MIN_POINT_DIST {
                        return;
                    }
                    if dist >= 1.0 {
                        let u = [dx / dist, dy / dist];
                        self.last_valid_dir = Some(match self.last_valid_dir {
                            Some(prev_u) => {
                                let b = [prev_u[0] * 0.5 + u[0] * 0.5, prev_u[1] * 0.5 + u[1] * 0.5];
                                let blen = b[0].hypot(b[1]);
                                if blen > 1e-6 { [b[0] / blen, b[1] / blen] } else { u }
                            }
                            None => u,
                        });
                    }
                }
                self.prev_pos = Some((x, y));
                let p = self.pressure_filter.push_move(raw_force, self.pressure_divisor);
                let sp = StrokePoint { x, y, pressure: p, tilt_x: tx, tilt_y: ty, rotation: 0.0, wheel: 1.0, time: self.time_ms(item.rec.timestamp) };
                out_points.push(sp);
                generator.push(&[sp], dabs, dual);
            }
            _ => {
                if let Some((px, py)) = self.prev_pos {
                    let (dx, dy) = (x - px, y - py);
                    let dist = dx.hypot(dy);
                    let sharp_rev = dist > 1e-6
                        && dist < Self::MICRO_HOOK_REVERSAL_MAX_DIST
                        && self.last_valid_dir.is_some_and(|dir| (dx / dist) * dir[0] + (dy / dist) * dir[1] < Self::MICRO_HOOK_MIN_DOT);
                    if dist < Self::MICRO_HOOK_MIN_DIST || sharp_rev {
                        x = px;
                        y = py;
                    }
                }
                let p = self.pressure_filter.end_liftoff(raw_force, self.pressure_divisor);
                let sp = StrokePoint { x, y, pressure: p, tilt_x: tx, tilt_y: ty, rotation: 0.0, wheel: 1.0, time: self.time_ms(item.rec.timestamp) };
                out_points.push(sp);
                generator.push(&[sp], dabs, dual);
                generator.finish(dabs, dual);
                self.last_confirmed_force = None;
                self.prev_pos = None;
                self.last_valid_dir = None;
            }
        }
    }

    fn flush_ready(&mut self, force_all: bool, now_s: f64, generator: &mut DabGenerator, dabs: &mut Vec<Dab>, dual: &mut Vec<Dab>, out_points: &mut Vec<StrokePoint>) {
        while let Some(front) = self.pending.first().copied() {
            if front.ready {
                self.pending.remove(0);
                self.emit_confirmed(front, generator, dabs, dual, out_points);
            } else if force_all || (now_s - front.rec.timestamp) >= Self::INITIAL_HOLD_SECS {
                let fallback = self
                    .last_confirmed_force
                    .or_else(|| self.pending.iter().find(|i| !i.unknown_force).map(|i| i.raw_norm))
                    .unwrap_or(Self::INITIAL_FALLBACK_FORCE);
                self.last_confirmed_force = Some(fallback);
                for item in &mut self.pending {
                    if item.unknown_force {
                        item.raw_norm = fallback;
                        item.unknown_force = false;
                        item.ready = true;
                    }
                }
            } else {
                break;
            }
        }
    }

    /// Feed one raw touch record and return resolved points pushed to `generator`.
    pub fn push(&mut self, rec: RawTouchRecord, generator: &mut DabGenerator, dabs: &mut Vec<Dab>, dual: &mut Vec<Dab>) -> Vec<StrokePoint> {
        let mut out_points = Vec::new();
        let (raw_norm, is_unknown) = Self::raw_norm_and_unknown(&rec);
        if rec.phase == PHASE_ESTIMATE_UPDATE {
            if !is_unknown {
                let confirmed = raw_norm.max(PressureFilter::MIN_PRESSURE);
                self.last_confirmed_force = Some(confirmed);
                if let Some(target_idx) = rec.estimation_update_index {
                    for item in &mut self.pending {
                        if item.rec.estimation_update_index == Some(target_idx) || item.unknown_force {
                            item.raw_norm = confirmed;
                            item.unknown_force = false;
                            item.ready = true;
                        }
                    }
                } else {
                    for item in &mut self.pending {
                        if item.unknown_force {
                            item.raw_norm = confirmed;
                            item.unknown_force = false;
                            item.ready = true;
                        }
                    }
                }
                self.flush_ready(false, rec.timestamp, generator, dabs, dual, &mut out_points);
            }
            return out_points;
        }

        if rec.phase == PHASE_BEGAN {
            self.pending.clear();
            self.last_confirmed_force = None;
            self.start_ts_s = None;
            self.last_time_ms = 0.0;
            self.pressure_filter.reset();
        }

        if !is_unknown {
            let confirmed = raw_norm.max(PressureFilter::MIN_PRESSURE);
            self.last_confirmed_force = Some(confirmed);
            for item in &mut self.pending {
                if item.unknown_force {
                    item.raw_norm = confirmed;
                    item.unknown_force = false;
                    item.ready = true;
                }
            }
            self.pending.push(PendingTouch { rec, raw_norm: confirmed, unknown_force: false, ready: true });
        } else if let Some(confirmed) = self.last_confirmed_force {
            // Mid-stroke or lift-off: forward last confirmed force immediately with zero delay.
            self.pending.push(PendingTouch { rec, raw_norm: confirmed, unknown_force: false, ready: true });
        } else {
            // Stroke start before first confirmed force: hold briefly for estimate update.
            self.pending.push(PendingTouch { rec, raw_norm: 0.0, unknown_force: true, ready: false });
        }

        let flush_all = rec.phase >= PHASE_ENDED;
        self.flush_ready(flush_all, rec.timestamp, generator, dabs, dual, &mut out_points);
        out_points
    }

    /// Replay a complete recorded touch stream into `DabGenerator`.
    pub fn replay(brush: &BrushSettings, records: &[RawTouchRecord], pressure_divisor: f32) -> (Vec<StrokePoint>, Vec<Dab>) {
        let mut resolver = Self::new(pressure_divisor);
        let mut dgen = DabGenerator::new(brush, 1.0);
        let (mut points, mut dabs, mut dual) = (Vec::new(), Vec::new(), Vec::new());
        for &rec in records {
            points.extend(resolver.push(rec, &mut dgen, &mut dabs, &mut dual));
        }
        (points, dabs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brush::Control;

    fn pressure_sized_zero_smooth_brush() -> BrushSettings {
        let mut brush = BrushSettings {
            size: 30.0,
            spacing: 0.25,
            pressure_size: true,
            pressure_opacity: true,
            ..BrushSettings::default()
        };
        brush.shape_dynamics.enabled = true;
        brush.shape_dynamics.size.control = Control::PenPressure;
        brush.smoothing.amount = 0.0;
        brush.smoothing.stabilization = 0.0;
        brush.smoothing.streamline = 0.0;
        brush
    }

    fn raw_rec(x: f64, y: f64, force: f32, is_force_estimated: bool, estimation_update_index: Option<u64>, timestamp: f64, phase: u8) -> RawTouchRecord {
        RawTouchRecord {
            x,
            y,
            force,
            max_force: 1.0,
            is_force_estimated,
            estimation_update_index,
            altitude: 1.0,
            azimuth: 0.0,
            timestamp,
            phase,
        }
    }

    #[test]
    fn touch_recorder_is_disabled_by_default_and_roundtrips_json_and_csv() {
        let mut rec = TouchRecorder::new();
        assert!(!rec.is_enabled());
        rec.record(raw_rec(10.0, 20.0, 0.4, false, Some(7), 100.0, PHASE_BEGAN));
        assert!(rec.records().is_empty(), "disabled recorder must store nothing");

        rec.set_enabled(true);
        rec.record(raw_rec(10.0, 20.0, 0.0, true, Some(7), 100.000, PHASE_BEGAN));
        rec.record(raw_rec(10.0, 20.0, 0.35, false, Some(7), 100.004, PHASE_ESTIMATE_UPDATE));
        rec.record(raw_rec(24.0, 20.0, 0.45, false, None, 100.012, PHASE_MOVED));
        rec.record(raw_rec(38.0, 20.0, 0.20, false, None, 100.020, PHASE_ENDED));

        let json = rec.to_json().expect("json serialize");
        let parsed_json = TouchRecorder::from_json(&json).expect("json deserialize");
        assert_eq!(parsed_json, rec.records());

        let csv = rec.to_csv();
        let parsed_csv = TouchRecorder::from_csv(&csv).expect("csv deserialize");
        assert_eq!(parsed_csv, rec.records());
    }

    #[test]
    fn replay_touchdown_zero_estimated_force_waits_for_refined_035_and_never_stamps_zero_dab() {
        let brush = pressure_sized_zero_smooth_brush();
        let stream = [
            // touchesBegan with bogus 0.00 estimated force at t = 10.000s
            raw_rec(10.0, 20.0, 0.0, true, Some(101), 10.000, PHASE_BEGAN),
            // 4 ms later: refined update arrives with 0.35 force
            raw_rec(10.0, 20.0, 0.35, false, Some(101), 10.004, PHASE_ESTIMATE_UPDATE),
            raw_rec(24.0, 20.0, 0.35, false, None, 10.012, PHASE_MOVED),
            raw_rec(38.0, 20.0, 0.35, false, None, 10.020, PHASE_ENDED),
        ];

        let (points, dabs) = TouchForceResolver::replay(&brush, &stream, 1.0);
        assert!(points.len() >= 3);
        assert!(dabs.len() >= 4);

        let expected_p0 = PressureFilter::new().begin(0.35, 1.0);
        assert!((points[0].pressure - expected_p0).abs() < 1e-5, "touchdown point must backfill refined 0.35 force, got {}", points[0].pressure);
        let min_r = dabs.iter().map(|d| d.radius).fold(f32::INFINITY, f32::min);
        assert!(min_r > 5.0, "must never stamp a bogus 0.00 minimum-radius dab on touchdown, min_r={min_r}");
    }

    #[test]
    fn replay_midstroke_zero_unrefined_sandwiched_between_045_has_zero_bead_dropouts() {
        let brush = pressure_sized_zero_smooth_brush();
        let stream = [
            raw_rec(10.0, 50.0, 0.45, false, None, 20.000, PHASE_BEGAN),
            raw_rec(25.0, 50.0, 0.45, false, None, 20.004, PHASE_MOVED),
            // Mid-stroke unrefined 0.00 glitch!
            raw_rec(40.0, 50.0, 0.0, true, Some(205), 20.008, PHASE_MOVED),
            raw_rec(55.0, 50.0, 0.45, false, None, 20.012, PHASE_MOVED),
            raw_rec(70.0, 50.0, 0.45, false, None, 20.016, PHASE_ENDED),
        ];

        let (_points, dabs) = TouchForceResolver::replay(&brush, &stream, 1.0);
        assert!(dabs.len() >= 6);

        let first_r = dabs[0].radius;
        for (i, d) in dabs.iter().enumerate() {
            assert!(
                (d.radius - first_r).abs() < 0.2,
                "dab {i} dropped or spiked radius ({} vs steady {first_r}) due to unrefined 0.00 estimate",
                d.radius
            );
        }
        // Check uniform dab spacing with zero gaps/beads
        for (i, w) in dabs.windows(2).enumerate() {
            let gap = (w[1].center.x - w[0].center.x).hypot(w[1].center.y - w[0].center.y);
            assert!(gap > 1.0 && gap <= (first_r * 2.0 * 0.25) as f64 + 0.5, "uneven dab gap {gap} at {i}");
        }
    }

    #[test]
    fn replay_fast_flick_uneven_hardware_timestamps_yields_consistent_velocity_and_monotonic_liftoff() {
        let mut brush = pressure_sized_zero_smooth_brush();
        brush.dynamics_velocity_size = 1.0;

        // Uneven real hardware timestamps: [0.0, 3.8, 11.2, 15.1, 22.7] ms
        // Constant physical speed = 4.0 px/ms -> displacements [15.2, 29.6, 15.6, 30.4] px
        let stream = [
            raw_rec(10.0, 40.0, 0.60, false, None, 100.0000, PHASE_BEGAN),
            raw_rec(25.2, 40.0, 0.55, false, None, 100.0038, PHASE_MOVED),
            raw_rec(54.8, 40.0, 0.48, false, None, 100.0112, PHASE_MOVED),
            raw_rec(70.4, 40.0, 0.35, false, None, 100.0151, PHASE_MOVED),
            // Noisy micro-hook backward + unrefined 0.0 spike on lift-off, OR low refined tail
            raw_rec(100.8, 40.0, 0.18, false, None, 100.0227, PHASE_ENDED),
        ];

        let (points, dabs) = TouchForceResolver::replay(&brush, &stream, 1.0);
        let times_ms: Vec<f64> = points.iter().map(|p| p.time).collect();
        let expected_times = [0.0, 3.8, 11.2, 15.1, 22.7];
        for (got, want) in times_ms.iter().zip(expected_times) {
            assert!((got - want).abs() < 1e-6, "timestamp mismatch: got {got} ms, expected {want} ms");
        }

        // Verify PathWalker speed calculation uses real dt (steady 4.0 px/ms despite uneven dt!)
        let mut observed_vel = Vec::new();
        let mut walker = crate::dynamics::PathWalker::new(None);
        for pt in &points {
            walker.push(*pt, &|_| 5.0, &mut |s| observed_vel.push(s.velocity));
        }
        assert!(observed_vel.len() >= 4);
        for v in &observed_vel[1..] {
            assert!((*v - 4.0).abs() < 0.05, "velocity must stay ~4.0 px/ms from real dt, got {v}");
        }

        // Monotonic lift-off tail: final dabs must not spike wider or darker than the last body dab
        let tail_start = dabs.len().saturating_sub(5);
        for w in dabs[tail_start..].windows(2) {
            assert!(w[1].radius <= w[0].radius + 1e-4, "lift-off radius spiked: {} -> {}", w[0].radius, w[1].radius);
            assert!(w[1].opacity <= w[0].opacity + 1e-4, "lift-off opacity spiked: {} -> {}", w[0].opacity, w[1].opacity);
        }
    }
}
