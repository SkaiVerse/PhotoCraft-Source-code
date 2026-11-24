//! From input points to dabs: smoothing, brush pose, path walking (spacing, airbrush build-up) and
//! the per-dab dynamics (shape, scattering, transfer, colour, texture depth).
//!
//! Everything is incremental (feed points in any chunking, get the same dabs) and deterministic:
//! jitter comes from [`crate::rng`] keyed by the stroke seed and the step/dab index.

use photocraft_geom::Point;

use crate::brush::{BrushSettings, Control, Dynamic, Smoothing};
use crate::rng::{rand_signed, rand01, stream};
use crate::{Dab, StrokePoint};

/// Maximum pulled-string length at 100 % smoothing, in screen pixels.
pub const MAX_STRING_PX: f64 = 100.0;
/// Stroke time over which exponential smoothing moves its fraction of the way to the pointer.
pub const SMOOTHING_STEP_MS: f64 = 16.0;

/// With Spacing unchecked, one dab per this many milliseconds of stroke time, so faster strokes
/// space their dabs further apart (Photoshop: "the speed of the cursor determines the spacing").
pub const SPEED_SPACING_MS: f64 = 8.0;

/// Longest pixel-grid segment (in pixels) before falling back to distance spacing.
const MAX_GRID_STEPS: f64 = 1e7;
/// Widest gap (in pixels) [`PathWalker::bridge`] fills between two pixel-grid segments.
const MAX_BRIDGE: f64 = 4.0;

/// Densest speed spacing, in pixels between dabs (a slow, long stroke never floods the buffer).
const MIN_SPEED_STEP: f64 = 0.5;
/// Maximum airbrush catch-up dabs for one input segment.
const MAX_AIRBRUSH_DABS_PER_SEGMENT: usize = 512;
/// Most steps one distance-spaced segment emits: two far-apart points at a quarter-pixel spacing
/// would otherwise be billions of steps, all buffered before the first is drawn.
const MAX_DISTANCE_STEPS_PER_SEGMENT: usize = 1 << 20;
/// Most steps one stroke emits in total, over all its segments and spacing modes; later ones are
/// dropped (a real stroke is orders of magnitude shorter).
const MAX_STEPS_PER_STROKE: u64 = 1 << 21;

/// The dab centre of the pixel-grid cell holding `v` (see [`PathWalker::pixel_grid`]): the
/// pixel's centre for `offset` 0.5, the nearest pixel corner for 0. A value within rounding
/// noise of a cell boundary counts as on it, so snapping and line stepping agree.
pub(crate) fn grid_cell(v: f64, offset: f64) -> f64 {
    let w = v - offset + 0.5;
    let w = if (w - w.round()).abs() < 1e-7 { w.round() } else { w };
    w.floor() + offset
}

#[inline]
fn lerp_pt(a: &StrokePoint, b: &StrokePoint, f: f64) -> StrokePoint {
    let ff = f as f32;
    StrokePoint {
        x: a.x + (b.x - a.x) * f,
        y: a.y + (b.y - a.y) * f,
        pressure: a.pressure + (b.pressure - a.pressure) * ff,
        tilt_x: a.tilt_x + (b.tilt_x - a.tilt_x) * ff,
        tilt_y: a.tilt_y + (b.tilt_y - a.tilt_y) * ff,
        rotation: a.rotation + (b.rotation - a.rotation) * ff,
        wheel: a.wheel + (b.wheel - a.wheel) * ff,
        time: a.time + (b.time - a.time) * f,
    }
}

// ---------------------------------------------------------------------------
// Smoothing
// ---------------------------------------------------------------------------

/// The smoothed brush moved `f` of the way from `brush` towards the pointer `p`. Its pose
/// (pressure, tilt, rotation, wheel) moves with it, so the lagging brush paints with the pose the
/// pen had along the way, not the pointer's newest one. Taking the pointer's pressure made a fast
/// stroke's lift thin the brush while it still trailed far behind, and the end catch-up a hairline
/// (#5713). Time is the pointer's.
fn pull(brush: &StrokePoint, p: &StrokePoint, f: f64) -> StrokePoint {
    let ff = f as f32;
    // Barrel rotation takes the shorter way round.
    let dr = (p.rotation - brush.rotation + 540.0).rem_euclid(360.0) - 180.0;
    StrokePoint {
        x: brush.x + (p.x - brush.x) * f,
        y: brush.y + (p.y - brush.y) * f,
        pressure: brush.pressure + (p.pressure - brush.pressure) * ff,
        tilt_x: brush.tilt_x + (p.tilt_x - brush.tilt_x) * ff,
        tilt_y: brush.tilt_y + (p.tilt_y - brush.tilt_y) * ff,
        rotation: (brush.rotation + dr * ff).rem_euclid(360.0),
        wheel: brush.wheel + (p.wheel - brush.wheel) * ff,
        time: p.time,
    }
}

/// Does the brush's pose (pressure, tilt, rotation, wheel) differ from `p`'s by more than `eps`?
/// [`pull`] moves the pose with the position, so a pen that only changes pressure or tilt in place
/// leaves the brush behind too (#7747).
fn pose_differs(brush: &StrokePoint, p: &StrokePoint, eps: f32) -> bool {
    let dr = (p.rotation - brush.rotation + 540.0).rem_euclid(360.0) - 180.0;
    (p.pressure - brush.pressure).abs() > eps
        || (p.tilt_x - brush.tilt_x).abs() > eps
        || (p.tilt_y - brush.tilt_y).abs() > eps
        || dr.abs() > eps
        || (p.wheel - brush.wheel).abs() > eps
}

/// Global Apple Pencil concave pressure calibration curve:
/// Saturates full output (1.0) at 0.70 of hardware max force (clamp(p / 0.70, 0.0, 1.0))
/// and applies a concave gamma lift (pow(p_scaled, 0.60)) so light sketching registers
/// effortlessly while retaining full dynamic range.
#[inline]
pub fn calibrate_stylus_pressure(raw_force: f32) -> f32 {
    let p_scaled = (raw_force.max(0.0) / 0.70).min(1.0);
    p_scaled.powf(0.60)
}

/// Damped circular angle filter with shortest-arc angular interpolation across [-pi, pi].
/// `current`: current filtered angle in radians [-pi, pi].
/// `target`: new target angle in radians [-pi, pi].
/// `alpha`: damping factor in [0.0, 1.0] (0.0 = keep current, 1.0 = instant target).
#[inline]
pub fn filter_circular_angle(current: f32, target: f32, alpha: f32) -> f32 {
    let pi = std::f32::consts::PI;
    let two_pi = 2.0 * pi;
    let diff = (target - current + pi).rem_euclid(two_pi) - pi;
    let filtered = current + diff * alpha.clamp(0.0, 1.0);
    (filtered + pi).rem_euclid(two_pi) - pi
}

/// Perpendicular motion filtering: dampens hand wobble perpendicular to the travel vector
/// without dragging the stroke behind the nib or shrinking tight loops / collapsing corners.
pub fn filter_perpendicular_motion(
    raw: StrokePoint,
    prev: StrokePoint,
    dir: &mut Option<[f64; 2]>,
    strength: f32,
) -> StrokePoint {
    let s = (strength as f64).clamp(0.0, 1.0);
    if s <= 0.0 {
        return raw;
    }
    let dx = raw.x - prev.x;
    let dy = raw.y - prev.y;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 1e-6 {
        return StrokePoint { x: prev.x, y: prev.y, ..raw };
    }
    let raw_u = [dx / dist, dy / dist];
    let (damped_dx, damped_dy) = if let Some(u) = *dir {
        let dot = raw_u[0] * u[0] + raw_u[1] * u[1]; // cosine of heading change
        // Corner-collapse protection: when heading changes sharply (dot < 0.3),
        // ease off damping so corner apexes and tight loops do not shrink or collapse.
        let corner_factor = if dot < 0.3 {
            (dot.max(0.0) / 0.3).powi(2)
        } else {
            1.0
        };
        let eff_s = s * corner_factor;
        let alpha_par = 1.0 - 0.5 * eff_s;
        let alpha_perp = 1.0 - 0.875 * eff_s;

        let par_len = dx * u[0] + dy * u[1];
        let par_x = par_len * u[0];
        let par_y = par_len * u[1];
        let perp_x = dx - par_x;
        let perp_y = dy - par_y;
        (par_x * alpha_par + perp_x * alpha_perp, par_y * alpha_par + perp_y * alpha_perp)
    } else {
        *dir = Some(raw_u);
        (dx, dy)
    };

    let new_len = (damped_dx * damped_dx + damped_dy * damped_dy).sqrt();
    if new_len > 1e-6 {
        let new_u = [damped_dx / new_len, damped_dy / new_len];
        *dir = Some(if let Some(u) = *dir {
            let blend_x = u[0] * 0.75 + new_u[0] * 0.25;
            let blend_y = u[1] * 0.75 + new_u[1] * 0.25;
            let blen = (blend_x * blend_x + blend_y * blend_y).sqrt().max(1e-6);
            [blend_x / blen, blend_y / blen]
        } else {
            new_u
        });
    }
    let ff = (1.0 - 0.5 * s) as f32;
    let dr = (raw.rotation - prev.rotation + 540.0).rem_euclid(360.0) - 180.0;
    let raw_p = if raw.pressure.is_finite() { raw.pressure.clamp(0.0009765625, 1.0) } else { prev.pressure };
    let new_p = (prev.pressure + (raw_p - prev.pressure) * ff).clamp(0.0009765625, 1.0);
    StrokePoint {
        x: prev.x + damped_dx,
        y: prev.y + damped_dy,
        pressure: new_p,
        tilt_x: prev.tilt_x + (raw.tilt_x - prev.tilt_x) * ff,
        tilt_y: prev.tilt_y + (raw.tilt_y - prev.tilt_y) * ff,
        rotation: (prev.rotation + dr * ff).rem_euclid(360.0),
        wheel: prev.wheel + (raw.wheel - prev.wheel) * ff,
        time: raw.time,
    }
}

/// Directional damping with 0.125 base gain + EMA for stabilization and streamline.
pub fn apply_directional_damping(
    raw: StrokePoint,
    prev: StrokePoint,
    dir: &mut Option<[f64; 2]>,
    strength: f32,
) -> StrokePoint {
    let s = (strength as f64).clamp(0.0, 1.0);
    if s <= 0.0 {
        return raw;
    }
    // Base gain 0.125: perpendicular shakiness damped with alpha in [1.0 -> 0.125];
    // parallel travel damped with alpha in [1.0 -> 0.5] (strictly < 2 frames latency at max strength).
    let alpha_par = 1.0 - 0.5 * s;
    let alpha_perp = 1.0 - 0.875 * s;
    let dx = raw.x - prev.x;
    let dy = raw.y - prev.y;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 1e-6 {
        return StrokePoint { x: prev.x, y: prev.y, ..raw };
    }
    let (damped_dx, damped_dy) = if let Some(u) = *dir {
        let dot = dx * u[0] + dy * u[1];
        let par_x = dot * u[0];
        let par_y = dot * u[1];
        let perp_x = dx - par_x;
        let perp_y = dy - par_y;
        (par_x * alpha_par + perp_x * alpha_perp, par_y * alpha_par + perp_y * alpha_perp)
    } else {
        (dx * alpha_par, dy * alpha_par)
    };
    let new_len = (damped_dx * damped_dx + damped_dy * damped_dy).sqrt();
    if new_len > 1e-6 {
        let new_u = [damped_dx / new_len, damped_dy / new_len];
        *dir = Some(if let Some(u) = *dir {
            let blend_x = u[0] * 0.7 + new_u[0] * 0.3;
            let blend_y = u[1] * 0.7 + new_u[1] * 0.3;
            let blen = (blend_x * blend_x + blend_y * blend_y).sqrt().max(1e-6);
            [blend_x / blen, blend_y / blen]
        } else {
            new_u
        });
    }
    let ff = alpha_par as f32;
    let dr = (raw.rotation - prev.rotation + 540.0).rem_euclid(360.0) - 180.0;
    let raw_p = if raw.pressure.is_finite() { raw.pressure.clamp(0.0009765625, 1.0) } else { prev.pressure };
    let new_p = (prev.pressure + (raw_p - prev.pressure) * ff).clamp(0.0009765625, 1.0);
    StrokePoint {
        x: prev.x + damped_dx,
        y: prev.y + damped_dy,
        pressure: new_p,
        tilt_x: prev.tilt_x + (raw.tilt_x - prev.tilt_x) * ff,
        tilt_y: prev.tilt_y + (raw.tilt_y - prev.tilt_y) * ff,
        rotation: (prev.rotation + dr * ff).rem_euclid(360.0),
        wheel: prev.wheel + (raw.wheel - prev.wheel) * ff,
        time: raw.time,
    }
}

/// Incremental stroke smoothing: exponential (with optional time-based catch-up) or pulled string.
#[derive(Clone, Debug)]
pub struct Smoother {
    cfg: Smoothing,
    zoom: f64,
    pos: Option<StrokePoint>,
    last_input: Option<StrokePoint>,
    direction: Option<[f64; 2]>,
    damped_pos: Option<StrokePoint>,
}

impl Smoother {
    /// `zoom` is the view zoom (only used with `adjust_for_zoom`).
    pub fn new(cfg: &Smoothing, zoom: f32) -> Self {
        Self {
            cfg: cfg.clone(),
            zoom: f64::from(zoom.max(0.01)),
            pos: None,
            last_input: None,
            direction: None,
            damped_pos: None,
        }
    }

    fn active(&self) -> bool {
        self.cfg.enabled && self.cfg.amount > 0.0
    }

    /// Feed one input point; returns the brush positions to paint through.
    pub fn push(&mut self, p: StrokePoint, out: &mut Vec<StrokePoint>) {
        // A point without a position is dropped: kept, it would make every later smoothed position NaN.
        if !(p.x.is_finite() && p.y.is_finite()) {
            return;
        }
        let p = if self.cfg.enabled && (self.cfg.stabilization > 0.0 || self.cfg.streamline > 0.0) {
            let strength = self.cfg.stabilization.max(self.cfg.streamline);
            let prev = self.damped_pos.unwrap_or(p);
            let damped = apply_directional_damping(p, prev, &mut self.direction, strength);
            self.damped_pos = Some(damped);
            damped
        } else {
            p
        };
        let prev_input = self.last_input.replace(p);
        if !self.active() {
            out.push(p);
            return;
        }
        let Some(pos) = self.pos else {
            self.pos = Some(p);
            out.push(p);
            return;
        };
        let amount = f64::from(self.cfg.amount.clamp(0.0, 1.0));
        if self.cfg.pulled_string {
            let mut len = amount * MAX_STRING_PX;
            if self.cfg.adjust_for_zoom {
                len /= self.zoom;
            }
            let (dx, dy) = (p.x - pos.x, p.y - pos.y);
            let d = (dx * dx + dy * dy).sqrt();
            if d > len {
                let k = (d - len) / d;
                let np = pull(&pos, &p, k);
                self.pos = Some(np);
                out.push(np);
            }
            return;
        }
        // Exponential: every SMOOTHING_STEP_MS of stroke time moves `a` of the way towards the
        // pointer, so the feel doesn't depend on how often the device reports. Input without
        // timestamps takes one step per point.
        let a = 1.0 - amount.min(0.95);
        let dt = prev_input.map_or(0.0, |q| p.time - q.time);
        let still = prev_input.is_some_and(|q| q.x == p.x && q.y == p.y);
        if still && !self.cfg.catch_up {
            // Without Stroke Catch-Up the brush stops while the pointer pauses (Photoshop).
            return;
        }
        let (iters, step) = if dt > 0.0 && dt.is_finite() {
            let steps = dt / SMOOTHING_STEP_MS;
            let iters = steps.ceil().clamp(1.0, 64.0);
            // Each of `iters` moves covers `steps / iters` steps' worth of pull.
            (iters as usize, 1.0 - (1.0 - a).powf(steps.min(64.0) / iters))
        } else {
            (1, a)
        };
        let mut cur = pos;
        for _ in 0..iters {
            cur = pull(&cur, &p, step);
            out.push(cur);
        }
        self.pos = Some(cur);
    }

    /// Is Stroke Catch-Up still pulling the brush towards a pointer that has stopped? Then the
    /// stroke needs time to keep passing (repeats of the last point) while the pointer is held.
    pub fn lagging(&self) -> bool {
        match (self.active() && self.cfg.catch_up && !self.cfg.pulled_string, self.pos, self.last_input) {
            (true, Some(pos), Some(last)) => (pos.x - last.x).abs() + (pos.y - last.y).abs() > 0.01 || pose_differs(&pos, &last, 1e-3),
            _ => false,
        }
    }

    /// The string between the brush and the pointer: `(brush, pointer)` document positions while
    /// smoothing holds the brush back from the last input point, `None` when they coincide (or
    /// smoothing is off). The canvas draws it, as Photoshop does for Pulled String Mode.
    pub fn string(&self) -> Option<([f64; 2], [f64; 2])> {
        let (pos, last) = (self.pos?, self.last_input?);
        (self.active() && (pos.x != last.x || pos.y != last.y)).then_some(([pos.x, pos.y], [last.x, last.y]))
    }

    /// End of stroke: with Catch-Up On Stroke End the brush finishes at the last pointer position
    /// and pose.
    pub fn finish(&mut self, out: &mut Vec<StrokePoint>) {
        if !self.active() || !self.cfg.catch_up_on_end {
            return;
        }
        if let (Some(pos), Some(last)) = (self.pos, self.last_input)
            && (pos.x != last.x || pos.y != last.y || pose_differs(&pos, &last, 0.0))
        {
            self.pos = Some(last);
            out.push(last);
        }
    }
}

/// Smooth a whole point list with the brush's smoothing settings.
pub fn smooth_points(cfg: &Smoothing, zoom: f32, pts: &[StrokePoint]) -> Vec<StrokePoint> {
    let mut s = Smoother::new(cfg, zoom);
    let mut out = Vec::with_capacity(pts.len());
    for p in pts {
        s.push(*p, &mut out);
    }
    s.finish(&mut out);
    out
}

// ---------------------------------------------------------------------------
// Path walking
// ---------------------------------------------------------------------------

/// One spacing (or airbrush time) step along the path.
#[derive(Clone, Copy, Debug)]
pub struct StepInput {
    pub point: StrokePoint,
    /// Visual (counter-clockwise, y-up) direction of travel in radians.
    pub direction: f32,
    pub initial_direction: f32,
    /// Sequential step number (0 = first).
    pub step: u64,
    /// Estimated stroke velocity in px/ms.
    pub velocity: f32,
}

/// Walks a polyline and emits steps every `step_len(point)` pixels, plus airbrush steps in time.
#[derive(Clone, Debug)]
pub struct PathWalker {
    last: Option<StrokePoint>,
    /// Path distance from `last` to the next step.
    next_at: f64,
    /// First point, emitted lazily once the initial direction is known.
    pending_first: Option<StrokePoint>,
    initial_dir: Option<f32>,
    dir: f32,
    step: u64,
    /// Airbrush interval in ms (None = off).
    interval: Option<f64>,
    time_acc: f64,
    /// Speed spacing (Spacing unchecked): one step per this many ms of stroke time.
    speed: Option<f64>,
    speed_acc: f64,
    /// Pixel-grid stepping for small aliased tips: the dab-centre offset inside a pixel (see
    /// [`PathWalker::pixel_grid`]).
    grid: Option<f64>,
    /// The grid cell of the last pixel-grid step, so the next segment joins it (#8040).
    cell: Option<(f64, f64)>,
    velocity: f32,
}

impl PathWalker {
    pub fn new(build_up_interval_ms: Option<f64>) -> Self {
        Self {
            last: None,
            next_at: 0.0,
            pending_first: None,
            initial_dir: None,
            dir: 0.0,
            step: 0,
            interval: build_up_interval_ms,
            time_acc: 0.0,
            speed: None,
            speed_acc: 0.0,
            grid: None,
            cell: None,
            velocity: 0.0,
        }
    }

    /// Step pixel by pixel instead of by distance: one step per pixel along the segment's major
    /// axis, each at the dab centre (`offset` 0.5 for odd tip diameters, 0 for even ones, as
    /// `render::grid_center` snaps) of the pixel the segment's line crosses at that column (or
    /// row, when steep). A small aliased tip (the Pencil) then draws a clean 8-connected
    /// staircase: stepping by distance put two dabs in a row wherever the line crossed a pixel
    /// corner (#2139). The line is sampled where it really runs, not between the pixels holding
    /// the segment's ends, so a straight line split into pieces (by Smoothing, or the pointer
    /// samples of a ⇧-drag) paints the pixels of the whole line, whichever end it starts from
    /// (#6336).
    pub fn pixel_grid(mut self, offset: f64) -> Self {
        self.grid = Some(offset);
        self
    }

    /// Emits the pixel steps from `a` to `p` (see [`PathWalker::pixel_grid`]). False, emitting
    /// nothing, when the span is not finite or implausibly long: distance spacing takes over.
    fn grid_steps(&mut self, a: &StrokePoint, p: &StrokePoint, offset: f64, out: &mut impl FnMut(StepInput)) -> bool {
        // The pieces of a 45° line differ in rounding noise: keep them all on one major axis.
        let steep = (p.y - a.y).abs() > (p.x - a.x).abs() * (1.0 + 1e-9);
        let (am, an, pm, pn) = if steep { (a.y, a.x, p.y, p.x) } else { (a.x, a.y, p.x, p.y) };
        let (m0, m1) = (grid_cell(am, offset), grid_cell(pm, offset));
        let (dm, dn) = (pm - am, pn - an);
        let n = (m1 - m0).abs();
        if !(n.is_finite() && dn.is_finite()) || n > MAX_GRID_STEPS || dm == 0.0 {
            return false;
        }
        let dir = (m1 - m0).signum();
        for i in 1..=n as u64 {
            let c = m0 + dir * i as f64;
            // The last column's centre can sit up to half a pixel past `p`: the line goes on to
            // it, so the next piece of a straight line picks up where this one left off.
            let t = (c - am) / dm;
            let mut q = lerp_pt(a, p, t.clamp(0.0, 1.0));
            let cn = grid_cell(an + dn * t, offset);
            (q.x, q.y) = if steep { (cn, c) } else { (c, cn) };
            self.bridge(&q, out);
            self.emit(q, out);
        }
        true
    }

    /// Fills the pixels between the last grid step and `q` when they don't touch. Each segment
    /// samples its own line, so where a freehand stroke turns, the last pixel of one segment and
    /// the first of the next can sit two pixels apart, and the 1 px Pencil line breaks (#8040).
    fn bridge(&mut self, q: &StrokePoint, out: &mut impl FnMut(StepInput)) {
        let Some((mut x, mut y)) = self.cell else { return };
        // Only a turn's small gap: a step cut off by the walker's budget is not filled in.
        if (q.x - x).abs().max((q.y - y).abs()) > MAX_BRIDGE {
            return;
        }
        while (q.x - x).abs() > 1.5 || (q.y - y).abs() > 1.5 {
            if (q.x - x).abs() > 0.5 {
                x += (q.x - x).signum();
            }
            if (q.y - y).abs() > 0.5 {
                y += (q.y - y).signum();
            }
            self.emit(StrokePoint { x, y, ..*q }, out);
        }
    }

    /// Space steps by stroke time instead of distance ([`SPEED_SPACING_MS`]): faster movement
    /// spreads them out. Segments without timestamps are spaced by distance.
    pub fn speed_spacing(mut self, interval_ms: f64) -> Self {
        self.speed = Some(interval_ms.max(0.1));
        self
    }

    /// The stroke's first point. With the airbrush on it lays two dabs, so a plain click builds
    /// up as Photoshop's does (#11886).
    fn emit_first(&mut self, point: StrokePoint, out: &mut impl FnMut(StepInput)) {
        self.emit(point, out);
        if self.interval.is_some() {
            self.emit(point, out);
        }
    }

    fn emit(&mut self, point: StrokePoint, out: &mut impl FnMut(StepInput)) {
        if self.step >= MAX_STEPS_PER_STROKE {
            return;
        }
        if let Some(g) = self.grid {
            self.cell = Some((grid_cell(point.x, g), grid_cell(point.y, g)));
        }
        let s = StepInput {
            point,
            direction: self.dir,
            initial_direction: self.initial_dir.unwrap_or(self.dir),
            step: self.step,
            velocity: self.velocity,
        };
        self.step += 1;
        out(s);
    }

    pub fn push(&mut self, p: StrokePoint, step_len: &impl Fn(&StrokePoint) -> f64, out: &mut impl FnMut(StepInput)) {
        let Some(a) = self.last else {
            self.last = Some(p);
            self.pending_first = Some(p);
            self.next_at = step_len(&p);
            return;
        };
        let (dx, dy) = (p.x - a.x, p.y - a.y);
        let len = (dx * dx + dy * dy).sqrt();
        if len > 1e-9 {
            let target_dir = (-dy).atan2(dx) as f32;
            if self.initial_dir.is_none() {
                self.dir = target_dir;
                self.initial_dir = Some(target_dir);
            } else {
                let alpha = (len as f32 / 6.0).clamp(0.20, 0.75);
                self.dir = filter_circular_angle(self.dir, target_dir, alpha);
            }
            let dt = (p.time - a.time).max(0.0);
            let instant_speed = if dt > 0.0 { (len / dt) as f32 } else { (len / 16.0) as f32 };
            self.velocity = self.velocity * 0.7 + instant_speed * 0.3;
        }
        if let Some(f) = self.pending_first.take() {
            self.emit_first(f, out);
        }
        if len > 1e-9 {
            // Speed spacing needs the segment's duration; without timestamps (a straight ⇧-click
            // line, or input that carries no times) the segment is spaced by distance instead.
            let dt = p.time - a.time;
            match self.speed.filter(|_| dt > 0.0 && dt.is_finite()) {
                None => {
                    if !self.grid.is_some_and(|g| self.grid_steps(&a, &p, g, out)) {
                        let mut emitted = 0usize;
                        while self.next_at <= len + 1e-9 && emitted < MAX_DISTANCE_STEPS_PER_SEGMENT && self.step < MAX_STEPS_PER_STROKE {
                            let q = lerp_pt(&a, &p, (self.next_at / len).min(1.0));
                            self.emit(q, out);
                            self.next_at += step_len(&q).max(0.25);
                            emitted += 1;
                        }
                        // Cut short: the rest of the segment is skipped, spacing restarts at its end.
                        // Only when the cap held back a step that was still due; a segment that ends right at the cap keeps its carry-over.
                        let cut_short = emitted == MAX_DISTANCE_STEPS_PER_SEGMENT && self.next_at <= len + 1e-9;
                        self.next_at = if cut_short { step_len(&p).max(0.25) } else { self.next_at - len };
                    }
                }
                Some(iv) => {
                    self.speed_acc += dt;
                    let max_n = (len / MIN_SPEED_STEP).ceil().max(1.0) as usize;
                    let mut n = 0;
                    while self.speed_acc >= iv && n < max_n && self.step < MAX_STEPS_PER_STROKE {
                        self.speed_acc -= iv;
                        let f = ((dt - self.speed_acc) / dt).clamp(0.0, 1.0);
                        self.emit(lerp_pt(&a, &p, f), out);
                        n += 1;
                    }
                    if n == max_n {
                        self.speed_acc = self.speed_acc.rem_euclid(iv);
                    }
                }
            }
        }
        if let Some(iv) = self.interval {
            let dt = p.time - a.time;
            if dt > 0.0 && dt.is_finite() {
                if dt > iv * MAX_AIRBRUSH_DABS_PER_SEGMENT as f64 {
                    // A stalled or hostile timestamp must not replay an arbitrarily long backlog.
                    self.time_acc = 0.0;
                    self.emit(p, out);
                    self.last = Some(p);
                    return;
                }
                self.time_acc += dt;
                let mut n = 0;
                while self.time_acc >= iv && n < MAX_AIRBRUSH_DABS_PER_SEGMENT {
                    self.time_acc -= iv;
                    let f = ((dt - self.time_acc) / dt).clamp(0.0, 1.0);
                    self.emit(lerp_pt(&a, &p, f), out);
                    n += 1;
                }
            }
        }
        self.last = Some(p);
    }

    pub fn finish(&mut self, out: &mut impl FnMut(StepInput)) {
        if let Some(f) = self.pending_first.take() {
            self.emit_first(f, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Dynamics
// ---------------------------------------------------------------------------

/// Scalar control value in 0..1.
pub fn control_value(c: Control, fade_steps: u32, s: &StepInput) -> f32 {
    let p = &s.point;
    match c {
        Control::Off | Control::InitialDirection | Control::Direction => 1.0,
        Control::Fade => 1.0 - (s.step as f32 / fade_steps.max(1) as f32).min(1.0),
        Control::PenPressure => p.pressure.clamp(0.0, 1.0),
        // Upright pen = 1, flat pen = 0 (a mouse reports no tilt, so paints at full value).
        Control::PenTilt => 1.0 - (p.tilt_x.hypot(p.tilt_y) / 90.0).clamp(0.0, 1.0),
        Control::StylusWheel => p.wheel.clamp(0.0, 1.0),
        Control::Rotation => (p.rotation.rem_euclid(360.0)) / 360.0,
    }
}

/// A dynamic's control value passed through its own response curve ([`Dynamic::curve`]). Controls
/// with no 0..1 input (Off, the direction controls) read 1 and skip the curve.
pub fn dynamic_value(d: &Dynamic, s: &StepInput) -> f32 {
    let v = control_value(d.control, d.fade_steps, s);
    match d.control {
        Control::Off | Control::InitialDirection | Control::Direction => v,
        Control::Fade | Control::PenPressure | Control::PenTilt | Control::StylusWheel | Control::Rotation => d.curve.eval(v),
    }
}

/// Angle contributed by a control, in degrees (counter-clockwise).
pub fn control_angle(c: Control, fade_steps: u32, s: &StepInput) -> f32 {
    let p = &s.point;
    match c {
        Control::Off => 0.0,
        Control::Fade => 360.0 * (s.step as f32 / fade_steps.max(1) as f32).min(1.0),
        Control::PenPressure => 360.0 * p.pressure.clamp(0.0, 1.0),
        Control::PenTilt => {
            if p.tilt_x == 0.0 && p.tilt_y == 0.0 {
                0.0
            } else {
                (-p.tilt_y).atan2(p.tilt_x).to_degrees()
            }
        }
        Control::StylusWheel => 360.0 * p.wheel.clamp(0.0, 1.0),
        Control::Rotation => p.rotation,
        Control::InitialDirection => s.initial_direction.to_degrees(),
        Control::Direction => s.direction.to_degrees(),
    }
}

/// A 0..1 multiplier from a dynamic: control lerps from `minimum` to 1, jitter reduces randomly,
/// never below `minimum`.
fn scalar(d: &Dynamic, s: &StepInput, r: f32) -> f32 {
    if !d.is_active() {
        return 1.0;
    }
    let min = d.minimum.clamp(0.0, 1.0);
    let cv = dynamic_value(d, s);
    let v = (min + (1.0 - min) * cv) * (1.0 - d.jitter.clamp(0.0, 1.0) * r);
    v.max(min)
}

fn rgb_to_hsv(c: [f32; 3]) -> [f32; 3] {
    let mx = c[0].max(c[1]).max(c[2]);
    let mn = c[0].min(c[1]).min(c[2]);
    let d = mx - mn;
    let h = if d <= 0.0 {
        0.0
    } else if mx == c[0] {
        ((c[1] - c[2]) / d).rem_euclid(6.0)
    } else if mx == c[1] {
        (c[2] - c[0]) / d + 2.0
    } else {
        (c[0] - c[1]) / d + 4.0
    } / 6.0;
    [h, if mx > 0.0 { d / mx } else { 0.0 }, mx]
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h6 = h.rem_euclid(1.0) * 6.0;
    let i = h6.floor();
    let f = h6 - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match i as i32 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// Colour Dynamics for one dab (or once per stroke, with `index = 0`).
pub fn dynamic_color(b: &BrushSettings, s: &StepInput, seed: u64, index: u64) -> [f32; 4] {
    let cd = &b.color_dynamics;
    let (fg, bg) = (b.color, b.background);
    let r = |st: u64| rand01(seed, index, st);
    let base = if cd.fg_bg.control != Control::Off { 1.0 - dynamic_value(&cd.fg_bg, s) } else { 0.0 };
    let t = (base + (1.0 - base) * cd.fg_bg.jitter.clamp(0.0, 1.0) * r(stream::FG_BG)).clamp(0.0, 1.0);
    let rgb = [fg[0] + (bg[0] - fg[0]) * t, fg[1] + (bg[1] - fg[1]) * t, fg[2] + (bg[2] - fg[2]) * t];
    let [mut h, mut sat, mut v] = rgb_to_hsv(rgb);
    h += rand_signed(seed, index, stream::HUE) * cd.hue_jitter.clamp(0.0, 1.0) * 0.5;
    sat = (sat + rand_signed(seed, index, stream::SAT) * cd.saturation_jitter.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let pu = cd.purity.clamp(-1.0, 1.0);
    sat = if pu >= 0.0 { sat + (1.0 - sat) * pu } else { sat * (1.0 + pu) };
    v = (v + rand_signed(seed, index, stream::BRIGHT) * cd.brightness_jitter.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let o = hsv_to_rgb(h, sat, v);
    [o[0], o[1], o[2], fg[3] + (bg[3] - fg[3]) * t]
}

#[inline]
fn sample_lut_256(table: &[f32; 256], x: f32) -> f32 {
    let x_safe = if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
    let t = x_safe * 255.0;
    let idx = t.floor() as usize;
    if idx >= 255 {
        return table[255];
    }
    let frac = t - idx as f32;
    table[idx] * (1.0 - frac) + table[idx + 1] * frac
}

/// Precomputed 256-entry 1-D lookup tables evaluated from brush curves for high-performance dab dynamics.
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicsLut {
    /// Maps pressure (0..=255 -> 0.0..=1.0) to size multiplier [0.0..=1.0].
    pub pressure_to_size: [f32; 256],
    /// Maps pressure (0..=255 -> 0.0..=1.0) to opacity multiplier [0.0..=1.0].
    pub pressure_to_opacity: [f32; 256],
    /// Maps tilt magnitude (0..=255 -> 0..=90 deg) to size multiplier [0.0..=1.0].
    pub tilt_to_size: [f32; 256],
    /// Maps tilt magnitude (0..=255 -> 0..=90 deg) to opacity multiplier [0.0..=1.0].
    pub tilt_to_opacity: [f32; 256],
    /// Maps tilt magnitude (0..=255 -> 0..=90 deg) to scatter multiplier [0.0..=1.0].
    pub tilt_to_scatter: [f32; 256],
    /// Maps azimuth / rotation (0..=255 -> 0..=360 deg) to rotation angle in degrees.
    pub azimuth_to_rotation: [f32; 256],
}

impl DynamicsLut {
    pub fn new(b: &BrushSettings) -> Self {
        let mut pressure_to_size = [1.0f32; 256];
        let mut pressure_to_opacity = [1.0f32; 256];
        let mut tilt_to_size = [1.0f32; 256];
        let mut tilt_to_opacity = [1.0f32; 256];
        let mut tilt_to_scatter = [1.0f32; 256];
        let mut azimuth_to_rotation = [0.0f32; 256];

        let sd = &b.shape_dynamics;
        let tr = &b.transfer;
        let sc = &b.scattering;

        // 1. pressure -> size
        for i in 0..256 {
            let t = i as f32 / 255.0;
            if b.dynamics_pressure_size > 0.0 {
                let curve_val = b.dynamics_pressure_size_curve.eval(t);
                let gain = b.dynamics_pressure_size.clamp(0.0, 1.0);
                pressure_to_size[i] = (1.0 - gain) + gain * curve_val;
            } else if sd.enabled && sd.size.control == Control::PenPressure {
                let min = sd.size.minimum.clamp(0.0, 1.0);
                pressure_to_size[i] = min + (1.0 - min) * sd.size.curve.eval(t);
            } else if b.pressure_size {
                pressure_to_size[i] = sd.size.curve.eval(t);
            } else {
                pressure_to_size[i] = 1.0;
            }
        }

        // 2. pressure -> opacity
        for i in 0..256 {
            let t = i as f32 / 255.0;
            if b.dynamics_pressure_opacity > 0.0 {
                let curve_val = b.dynamics_pressure_opacity_curve.eval(t);
                let gain = b.dynamics_pressure_opacity.clamp(0.0, 1.0);
                pressure_to_opacity[i] = (1.0 - gain) + gain * curve_val;
            } else if tr.enabled && tr.opacity.control == Control::PenPressure {
                let min = tr.opacity.minimum.clamp(0.0, 1.0);
                pressure_to_opacity[i] = min + (1.0 - min) * tr.opacity.curve.eval(t);
            } else if b.pressure_opacity {
                pressure_to_opacity[i] = tr.opacity.curve.eval(t);
            } else {
                pressure_to_opacity[i] = 1.0;
            }
        }

        // 3. tilt -> size
        for i in 0..256 {
            let norm_tilt = i as f32 / 255.0; // 0..90 deg normalized
            let pen_tilt_val = 1.0 - norm_tilt; // upright pen = 1, flat pen = 0
            if b.dynamics_tilt_size > 0.0 {
                let gain = b.dynamics_tilt_size.clamp(0.0, 1.0);
                tilt_to_size[i] = 1.0 + gain * norm_tilt;
            } else if sd.enabled && sd.size.control == Control::PenTilt {
                let min = sd.size.minimum.clamp(0.0, 1.0);
                tilt_to_size[i] = min + (1.0 - min) * sd.size.curve.eval(pen_tilt_val);
            } else {
                tilt_to_size[i] = 1.0;
            }
        }

        // 4. tilt -> opacity
        for i in 0..256 {
            let norm_tilt = i as f32 / 255.0;
            let pen_tilt_val = 1.0 - norm_tilt;
            if b.dynamics_tilt_opacity > 0.0 {
                let gain = b.dynamics_tilt_opacity.clamp(0.0, 1.0);
                tilt_to_opacity[i] = (1.0 - gain) + gain * (1.0 - norm_tilt * 0.5);
            } else if tr.enabled && tr.opacity.control == Control::PenTilt {
                let min = tr.opacity.minimum.clamp(0.0, 1.0);
                tilt_to_opacity[i] = min + (1.0 - min) * tr.opacity.curve.eval(pen_tilt_val);
            } else {
                tilt_to_opacity[i] = 1.0;
            }
        }

        // 5. tilt -> scatter
        for i in 0..256 {
            let norm_tilt = i as f32 / 255.0;
            let pen_tilt_val = 1.0 - norm_tilt;
            if sc.enabled && sc.scatter.control == Control::PenTilt {
                let min = sc.scatter.minimum.clamp(0.0, 1.0);
                tilt_to_scatter[i] = min + (1.0 - min) * sc.scatter.curve.eval(pen_tilt_val);
            } else {
                tilt_to_scatter[i] = 1.0;
            }
        }

        // 6. azimuth -> rotation
        for i in 0..256 {
            let deg = (i as f32 / 255.0) * 360.0;
            azimuth_to_rotation[i] = deg;
        }

        Self {
            pressure_to_size,
            pressure_to_opacity,
            tilt_to_size,
            tilt_to_opacity,
            tilt_to_scatter,
            azimuth_to_rotation,
        }
    }

    #[inline]
    pub fn size_from_pressure(&self, pressure: f32) -> f32 {
        sample_lut_256(&self.pressure_to_size, pressure)
    }

    #[inline]
    pub fn opacity_from_pressure(&self, pressure: f32) -> f32 {
        sample_lut_256(&self.pressure_to_opacity, pressure)
    }

    #[inline]
    pub fn size_from_tilt(&self, tilt_mag: f32) -> f32 {
        let norm = (tilt_mag.clamp(0.0, 90.0) / 90.0).clamp(0.0, 1.0);
        sample_lut_256(&self.tilt_to_size, norm)
    }

    #[inline]
    pub fn opacity_from_tilt(&self, tilt_mag: f32) -> f32 {
        let norm = (tilt_mag.clamp(0.0, 90.0) / 90.0).clamp(0.0, 1.0);
        sample_lut_256(&self.tilt_to_opacity, norm)
    }

    #[inline]
    pub fn scatter_from_tilt(&self, tilt_mag: f32) -> f32 {
        let norm = (tilt_mag.clamp(0.0, 90.0) / 90.0).clamp(0.0, 1.0);
        sample_lut_256(&self.tilt_to_scatter, norm)
    }

    #[inline]
    pub fn rotation_from_azimuth(&self, azimuth_deg: f32) -> f32 {
        let norm = (azimuth_deg.rem_euclid(360.0) / 360.0).clamp(0.0, 1.0);
        sample_lut_256(&self.azimuth_to_rotation, norm)
    }

    /// Direct $O(1)$ zero-allocation evaluation of size factor from pressure.
    #[inline]
    pub fn eval_pressure_size(b: &BrushSettings, pressure: f32) -> f32 {
        let t = pressure.clamp(0.0, 1.0);
        let sd = &b.shape_dynamics;
        if b.dynamics_pressure_size > 0.0 {
            let curve_val = b.dynamics_pressure_size_curve.eval(t);
            let gain = b.dynamics_pressure_size.clamp(0.0, 1.0);
            (1.0 - gain) + gain * curve_val
        } else if sd.enabled && sd.size.control == Control::PenPressure {
            let min = sd.size.minimum.clamp(0.0, 1.0);
            min + (1.0 - min) * sd.size.curve.eval(t)
        } else if b.pressure_size {
            sd.size.curve.eval(t)
        } else {
            1.0
        }
    }

    /// Direct $O(1)$ zero-allocation evaluation of opacity factor from pressure.
    #[inline]
    pub fn eval_pressure_opacity(b: &BrushSettings, pressure: f32) -> f32 {
        let t = pressure.clamp(0.0, 1.0);
        let tr = &b.transfer;
        if b.dynamics_pressure_opacity > 0.0 {
            let curve_val = b.dynamics_pressure_opacity_curve.eval(t);
            let gain = b.dynamics_pressure_opacity.clamp(0.0, 1.0);
            (1.0 - gain) + gain * curve_val
        } else if tr.enabled && tr.opacity.control == Control::PenPressure {
            let min = tr.opacity.minimum.clamp(0.0, 1.0);
            min + (1.0 - min) * tr.opacity.curve.eval(t)
        } else if b.pressure_opacity {
            tr.opacity.curve.eval(t)
        } else {
            1.0
        }
    }
}

/// Base (pre-jitter) diameter at a step: size × size control (pressure etc.). Drives spacing.
pub fn base_diameter(b: &BrushSettings, s: &StepInput) -> f32 {
    let sd = &b.shape_dynamics;
    let mut f = 1.0;
    if b.dynamics_pressure_size > 0.0 || b.pressure_size {
        f = DynamicsLut::eval_pressure_size(b, s.point.pressure);
    } else if sd.enabled && sd.size.control != Control::Off {
        let min = sd.size.minimum.clamp(0.0, 1.0);
        f = min + (1.0 - min) * dynamic_value(&sd.size, s);
    }
    b.size * f
}

/// Distance to the next step at `p`: the diameter there times the spacing.
fn step_len_for(brush: &BrushSettings, p: &StrokePoint) -> f64 {
    let s = StepInput { point: *p, direction: 0.0, initial_direction: 0.0, step: 0, velocity: 0.0 };
    let d = base_diameter(brush, &s).max(1.0);
    f64::from(d * brush.spacing.max(0.01)).max(0.5)
}

/// Builds the primary dabs of a step (Shape Dynamics, Scattering, Transfer, Color, Texture depth).
#[derive(Clone, Debug)]
pub struct DabBuilder {
    pub brush: BrushSettings,
    pub seed: u64,
    pub lut: DynamicsLut,
    /// Per-stroke colour (Color Dynamics with per-tip off), computed at the first step.
    stroke_color: Option<[f32; 4]>,
    next_index: u64,
}

impl DabBuilder {
    pub fn new(brush: &BrushSettings) -> Self {
        Self { brush: brush.clone(), seed: brush.seed, lut: DynamicsLut::new(brush), stroke_color: None, next_index: 0 }
    }

    pub fn step_len(&self, p: &StrokePoint) -> f64 {
        step_len_for(&self.brush, p)
    }

    pub fn build(&mut self, s: &StepInput, out: &mut Vec<Dab>) {
        let b = &self.brush;
        let seed = self.seed;
        let sd = &b.shape_dynamics;
        let sc = &b.scattering;
        let count = if sc.enabled {
            let n = sc.count.clamp(1, 16) as f32;
            let k = scalar(&sc.count_jitter, s, rand01(seed, s.step, stream::COUNT));
            ((n * k).round() as u32).clamp(1, 16)
        } else {
            1
        };
        let base_d = if b.dynamics_pressure_size > 0.0 {
            b.size * self.lut.size_from_pressure(s.point.pressure)
        } else if sd.enabled && sd.size.control != Control::Off {
            base_diameter(b, s)
        } else if b.pressure_size {
            b.size * self.lut.size_from_pressure(s.point.pressure)
        } else {
            base_diameter(b, s)
        };
        for _ in 0..count {
            let i = self.next_index;
            self.next_index += 1;
            let r = |st: u64| rand01(seed, i, st);
            // Shape.
            let mut diameter = base_d;
            if b.dynamics_tilt_size > 0.0 {
                let tilt_mag = s.point.tilt_x.hypot(s.point.tilt_y);
                diameter *= self.lut.size_from_tilt(tilt_mag);
            }
            let mut taper_factor = 1.0f32;
            if b.taper_start_length > 0.0 {
                let start_steps = (b.taper_start_length * 100.0).max(1.0);
                if (s.step as f32) < start_steps {
                    let ramp = (s.step as f32) / start_steps;
                    let p_inf = if b.taper_pressure > 0.0 { b.taper_pressure } else { 1.0 };
                    taper_factor = (1.0 - p_inf * (1.0 - ramp)).clamp(0.01, 1.0);
                }
            }
            if b.taper_start_length > 0.0 {
                if b.taper_size != 0.0 || (b.taper_size == 0.0 && b.taper_opacity == 0.0) {
                    diameter *= taper_factor;
                }
            }
            if b.dynamics_velocity_size > 0.0 {
                let norm_v = (s.velocity / 2.0).clamp(0.0, 1.0);
                let v_size_mult = 1.0 - b.dynamics_velocity_size.clamp(0.0, 1.0) * 0.40 * norm_v;
                diameter = (diameter * v_size_mult).max(0.5);
            }
            let mut angle = b.angle;
            if b.shape_azimuth {
                angle += self.lut.rotation_from_azimuth(s.point.rotation);
            }
            let mut roundness = b.roundness.clamp(0.01, 1.0);
            let (mut fx, mut fy) = (b.flip_x, b.flip_y);
            if sd.enabled {
                if sd.size.jitter > 0.0 {
                    let min = sd.size.minimum.clamp(0.0, 1.0);
                    let f = (1.0 - sd.size.jitter.clamp(0.0, 1.0) * r(stream::SIZE)).max(min);
                    diameter *= f;
                }
                angle +=
                    control_angle(sd.angle.control, sd.angle.fade_steps, s) + rand_signed(seed, i, stream::ANGLE) * sd.angle.jitter.clamp(0.0, 1.0) * 180.0;
                roundness = (roundness * scalar(&sd.roundness, s, r(stream::ROUNDNESS))).clamp(0.01, 1.0);
                if sd.flip_x_jitter && r(stream::FLIP_X) < 0.5 {
                    fx = !fx;
                }
                if sd.flip_y_jitter && r(stream::FLIP_Y) < 0.5 {
                    fy = !fy;
                }
            }
            // Tilt Scale and Brush Projection (Shape Dynamics).
            let (mut proj_angle, mut proj_scale) = (0.0, 1.0);
            if sd.enabled {
                let tilt = s.point.tilt_x.hypot(s.point.tilt_y).clamp(0.0, 90.0);
                if sd.size.control == Control::PenTilt && sd.tilt_scale > 0.0 {
                    let k = 1.0 - sd.tilt_scale.clamp(0.0, 2.0) * 0.5 * tilt / 90.0;
                    roundness = (roundness * k).clamp(0.01, 1.0);
                }
                if sd.brush_projection {
                    angle += self.lut.rotation_from_azimuth(s.point.rotation);
                    if tilt > 0.0 {
                        proj_angle = (-s.point.tilt_y).atan2(s.point.tilt_x);
                        proj_scale = tilt.to_radians().cos().max(0.05);
                    }
                }
            }
            let radius = (diameter / 2.0).max(0.5);
            // Scatter.
            let mut c = Point::new(s.point.x, s.point.y);
            let scatter_factor = if b.shape_scatter > 0.0 {
                b.shape_scatter
            } else if sc.enabled && sc.scatter.jitter > 0.0 {
                let cv = if sc.scatter.control == Control::PenTilt {
                    let tilt = s.point.tilt_x.hypot(s.point.tilt_y);
                    self.lut.scatter_from_tilt(tilt)
                } else {
                    dynamic_value(&sc.scatter, s)
                };
                sc.scatter.jitter * cv
            } else {
                0.0
            };
            if scatter_factor > 0.0 {
                let amt = f64::from(scatter_factor * radius);
                let (ox, oy) = (rand_signed(seed, i, stream::SCATTER_X) as f64 * amt, rand_signed(seed, i, stream::SCATTER_Y) as f64 * amt);
                if sc.both_axes || b.shape_scatter > 0.0 {
                    c.x += ox;
                    c.y += oy;
                } else {
                    // Perpendicular to the direction of travel (y-down image space).
                    let (dx, dy) = (f64::from(s.direction.cos()), -f64::from(s.direction.sin()));
                    c.x += -dy * ox;
                    c.y += dx * ox;
                }
            }
            // Transfer.
            let tr = &b.transfer;
            let (mut opacity, mut flow) = (1.0, b.flow);
            if tr.enabled {
                if tr.opacity.control == Control::PenTilt {
                    let tilt = s.point.tilt_x.hypot(s.point.tilt_y);
                    let cv = self.lut.opacity_from_tilt(tilt);
                    let min = tr.opacity.minimum.clamp(0.0, 1.0);
                    let v = (min + (1.0 - min) * cv) * (1.0 - tr.opacity.jitter.clamp(0.0, 1.0) * r(stream::OPACITY));
                    opacity = v.max(min);
                } else {
                    opacity = scalar(&tr.opacity, s, r(stream::OPACITY));
                }
                flow *= scalar(&tr.flow, s, r(stream::FLOW));
            }
            let (wet, mix) = if tr.enabled { (scalar(&tr.wetness, s, r(stream::WET)), scalar(&tr.mix, s, r(stream::MIX))) } else { (1.0, 1.0) };
            if b.dynamics_pressure_opacity > 0.0 {
                opacity *= self.lut.opacity_from_pressure(s.point.pressure);
            } else if b.pressure_opacity && !(tr.enabled && tr.opacity.control != Control::Off) {
                opacity *= self.lut.opacity_from_pressure(s.point.pressure);
            }
            if b.dynamics_tilt_opacity > 0.0 {
                let tilt_mag = s.point.tilt_x.hypot(s.point.tilt_y);
                opacity *= self.lut.opacity_from_tilt(tilt_mag);
            }
            if b.taper_start_length > 0.0 {
                if b.taper_opacity != 0.0 || (b.taper_size == 0.0 && b.taper_opacity == 0.0) {
                    opacity *= taper_factor;
                }
            }
            if b.dynamics_velocity_opacity > 0.0 {
                let norm_v = (s.velocity / 2.0).clamp(0.0, 1.0);
                let v_op_mult = 1.0 - b.dynamics_velocity_opacity.clamp(0.0, 1.0) * 0.35 * norm_v;
                opacity = (opacity * v_op_mult).clamp(0.0, 1.0);
            }
            // Colour.
            let color = if b.color_dynamics.enabled {
                if b.color_dynamics.per_tip { dynamic_color(b, s, seed, i) } else { *self.stroke_color.get_or_insert_with(|| dynamic_color(b, s, seed, 0)) }
            } else {
                b.color
            };
            // Texture depth (per tip).
            let tx = &b.texture;
            let depth = if b.grain_depth > 0.0 {
                b.grain_depth.clamp(0.0, 1.0)
            } else if tx.enabled && tx.each_tip {
                tx.depth.clamp(0.0, 1.0) * scalar(&tx.depth_jitter, s, r(stream::DEPTH))
            } else {
                tx.depth.clamp(0.0, 1.0)
            };
            out.push(Dab {
                center: c,
                radius,
                alpha: flow.clamp(0.0, 1.0),
                angle: angle.to_radians(),
                roundness,
                flip_x: fx,
                flip_y: fy,
                opacity: opacity.clamp(0.0, 1.0),
                color,
                depth,
                index: i,
                proj_angle,
                proj_scale,
                wet,
                mix,
            });
        }
    }
}

/// Builds Dual Brush dabs (the second tip: size, spacing, scatter, count, flip).
#[derive(Clone, Debug)]
pub struct DualBuilder {
    pub brush: BrushSettings,
    next_index: u64,
}

impl DualBuilder {
    pub fn new(brush: &BrushSettings) -> Self {
        Self { brush: brush.clone(), next_index: 0 }
    }
    pub fn step_len(&self, _p: &StrokePoint) -> f64 {
        let d = &self.brush.dual_brush;
        f64::from(d.size.max(1.0) * d.spacing.max(0.01)).max(0.5)
    }
    pub fn build(&mut self, s: &StepInput, out: &mut Vec<Dab>) {
        let d = &self.brush.dual_brush;
        let seed = self.brush.seed ^ 0xD0A1;
        for _ in 0..d.count.clamp(1, 16) {
            let i = self.next_index;
            self.next_index += 1;
            let radius = (d.size / 2.0).max(0.5);
            let amt = f64::from(d.scatter * radius);
            let (ox, oy) = (rand_signed(seed, i, stream::DUAL_X) as f64 * amt, rand_signed(seed, i, stream::DUAL_Y) as f64 * amt);
            let mut c = Point::new(s.point.x, s.point.y);
            if d.both_axes {
                c.x += ox;
                c.y += oy;
            } else {
                let (dx, dy) = (f64::from(s.direction.cos()), -f64::from(s.direction.sin()));
                c.x += -dy * ox;
                c.y += dx * ox;
            }
            let flip = d.flip && rand01(seed, i, stream::DUAL_FLIP) < 0.5;
            out.push(Dab {
                center: c,
                radius,
                alpha: 1.0,
                angle: d.angle.to_radians(),
                roundness: d.roundness.clamp(0.01, 1.0),
                flip_x: flip,
                flip_y: false,
                opacity: 1.0,
                color: [0.0, 0.0, 0.0, 1.0],
                depth: 1.0,
                index: i,
                proj_angle: 0.0,
                proj_scale: 1.0,
                wet: 1.0,
                mix: 1.0,
            });
        }
    }
}

/// Apply Brush Pose to an input point.
pub fn apply_pose(b: &BrushSettings, mut p: StrokePoint) -> StrokePoint {
    let pose = &b.pose;
    if pose.enabled {
        if pose.override_tilt_x {
            p.tilt_x = pose.tilt_x;
        }
        if pose.override_tilt_y {
            p.tilt_y = pose.tilt_y;
        }
        if pose.override_rotation {
            p.rotation = pose.rotation;
        }
        if pose.override_pressure {
            p.pressure = pose.pressure;
        }
    }
    p
}

/// Retroactively taper the final segment of dabs based on exit velocity and tangent across final samples.
/// Scales dab diameter (radius) and opacity smoothly down to a point along the exit tangent,
/// eliminating blunt pill-caps on fast flicks.
pub fn apply_retroactive_liftoff_taper(
    dabs: &mut [Dab],
    recent_points: &[StrokePoint],
) {
    if dabs.is_empty() || recent_points.len() < 2 {
        return;
    }
    let n_pts = recent_points.len();
    let sample_window = n_pts.min(5);
    let pts = &recent_points[n_pts - sample_window..];

    let dx = pts[pts.len() - 1].x - pts[0].x;
    let dy = pts[pts.len() - 1].y - pts[0].y;
    let chord_len = (dx * dx + dy * dy).sqrt();
    if chord_len < 1e-4 {
        return;
    }

    let taper_count = (dabs.len().min(8) as f32).round().max(2.0) as usize;
    let taper_count = taper_count.min(dabs.len());
    if taper_count == 0 {
        return;
    }

    let start_idx = dabs.len() - taper_count;
    for (i, dab) in dabs[start_idx..].iter_mut().enumerate() {
        let progress = (i + 1) as f32 / taper_count as f32;
        let taper_scale = (1.0 - progress * 0.90).clamp(0.05, 1.0);
        let smooth_taper = (taper_scale * std::f32::consts::PI * 0.5).sin();

        dab.radius = (dab.radius * smooth_taper).max(0.2);
        dab.opacity = (dab.opacity * smooth_taper).clamp(0.0, 1.0);
    }
}

/// Incremental dab generation for a whole brush (primary and optional dual dabs).
#[derive(Clone, Debug)]
pub struct DabGenerator {
    smoother: Smoother,
    walker: PathWalker,
    dual_walker: Option<PathWalker>,
    builder: DabBuilder,
    dual: Option<DualBuilder>,
    scratch: Vec<StrokePoint>,
    /// The brush the step length is read from, apart from the builder so the walker can read it
    /// while its callback has the builder.
    step_brush: std::sync::Arc<BrushSettings>,
    recent_points: [StrokePoint; 5],
    recent_count: usize,
}

impl DabGenerator {
    /// Does the stroke change with time while the pointer is held still (airbrush Build-up, or
    /// smoothing still catching up)? The canvas then keeps feeding it the held point.
    pub fn wants_time(&self) -> bool {
        self.walker.interval.is_some() || self.smoother.lagging()
    }

    /// See [`Smoother::string`].
    pub fn smoothing_string(&self) -> Option<([f64; 2], [f64; 2])> {
        self.smoother.string()
    }

    pub fn new(brush: &BrushSettings, zoom: f32) -> Self {
        let brush = brush.bounded_for_render();
        let interval = brush.build_up.then(|| 1000.0 / f64::from(brush.build_up_rate.clamp(0.1, 1000.0)));
        let dual_on = brush.dual_brush.enabled;
        let mut walker = if brush.spacing_enabled { PathWalker::new(interval) } else { PathWalker::new(interval).speed_spacing(SPEED_SPACING_MS) };
        // A small aliased tip whose dabs are at most a pixel apart steps pixel by pixel (#2139).
        if brush.aliased && brush.size * brush.spacing.max(0.01) <= 1.0 {
            walker = walker.pixel_grid(f64::from(crate::render::grid_center(0.0, 0.0, brush.size).0));
        }
        Self {
            smoother: Smoother::new(&brush.smoothing, zoom),
            walker,
            dual_walker: dual_on.then(|| PathWalker::new(None)),
            builder: DabBuilder::new(&brush),
            dual: dual_on.then(|| DualBuilder::new(&brush)),
            scratch: Vec::new(),
            step_brush: std::sync::Arc::new(brush),
            recent_points: [StrokePoint::default(); 5],
            recent_count: 0,
        }
    }

    fn walk(&mut self, pts: &[StrokePoint], dabs: &mut Vec<Dab>, dual: &mut Vec<Dab>) {
        for p in pts {
            for i in 0..4 {
                self.recent_points[i] = self.recent_points[i + 1];
            }
            self.recent_points[4] = *p;
            self.recent_count = (self.recent_count + 1).min(5);
            let p = apply_pose(&self.builder.brush, *p);
            // Each step becomes its dabs as the walker emits it: a long segment never holds its
            // step list (the pixel-grid mode can emit 1e7 steps for one).
            let brush = &*self.step_brush;
            let builder = &mut self.builder;
            self.walker.push(p, &|q| step_len_for(brush, q), &mut |s| builder.build(&s, dabs));
            if let (Some(w), Some(db)) = (self.dual_walker.as_mut(), self.dual.as_mut()) {
                let step = db.step_len(&p);
                w.push(p, &|_| step, &mut |s| db.build(&s, dual));
            }
        }
    }

    /// Feed input points; appends the new primary and dual dabs.
    pub fn push(&mut self, pts: &[StrokePoint], dabs: &mut Vec<Dab>, dual: &mut Vec<Dab>) {
        let mut sm = std::mem::take(&mut self.scratch);
        sm.clear();
        for p in pts {
            self.smoother.push(*p, &mut sm);
        }
        self.walk(&sm, dabs, dual);
        self.scratch = sm;
    }

    /// End of stroke.
    pub fn finish(&mut self, dabs: &mut Vec<Dab>, dual: &mut Vec<Dab>) {
        let mut sm = Vec::new();
        self.smoother.finish(&mut sm);
        self.walk(&sm, dabs, dual);
        self.walker.finish(&mut |s| self.builder.build(&s, dabs));
        if let (Some(w), Some(db)) = (self.dual_walker.as_mut(), self.dual.as_mut()) {
            w.finish(&mut |s| db.build(&s, dual));
        }
        let brush = &self.builder.brush;
        if brush.taper_end_length > 0.0 && !dabs.is_empty() {
            let end_steps = (brush.taper_end_length * 100.0).max(1.0) as usize;
            let len = dabs.len();
            let start_idx = len.saturating_sub(end_steps);
            let p_inf = if brush.taper_pressure > 0.0 { brush.taper_pressure } else { 1.0 };
            for (idx, dab) in dabs[start_idx..].iter_mut().enumerate() {
                let remaining = (len - 1 - (start_idx + idx)) as f32;
                let ramp = remaining / end_steps as f32;
                let taper_factor = (1.0 - p_inf * (1.0 - ramp)).clamp(0.01, 1.0);
                if brush.taper_size != 0.0 || (brush.taper_size == 0.0 && brush.taper_opacity == 0.0) {
                    dab.radius = (dab.radius * taper_factor).max(0.5);
                }
                if brush.taper_opacity != 0.0 || (brush.taper_size == 0.0 && brush.taper_opacity == 0.0) {
                    dab.opacity = (dab.opacity * taper_factor).clamp(0.0, 1.0);
                }
            }
        }
    }
}

#[cfg(test)]
mod walker_budget_tests {
    use super::*;

    #[test]
    fn one_far_segment_emits_a_bounded_number_of_steps() {
        let steps = std::cell::Cell::new(0usize);
        let mut count = |_: StepInput| steps.set(steps.get() + 1);
        let mut walker = PathWalker::new(None);
        walker.push(StrokePoint::new(0.0, 0.0, 1.0), &|_: &StrokePoint| 1.0, &mut count);
        // At a quarter pixel, a billion-pixel segment is four billion steps.
        walker.push(StrokePoint::new(1.0e9, 0.0, 1.0), &|_: &StrokePoint| 0.25, &mut count);
        assert!(steps.get() <= MAX_DISTANCE_STEPS_PER_SEGMENT + 1, "{}", steps.get());
        // Spacing carries on from the end of the cut segment.
        let before = steps.get();
        walker.push(StrokePoint::new(1.0e9 + 10.0, 0.0, 1.0), &|_: &StrokePoint| 1.0, &mut count);
        assert!(steps.get() > before && steps.get() - before <= 11, "{} more steps", steps.get() - before);
    }

    #[test]
    fn a_stroke_of_many_far_segments_stops_at_the_total_cap() {
        let steps = std::cell::Cell::new(0u64);
        let mut count = |_: StepInput| steps.set(steps.get() + 1);
        let mut walker = PathWalker::new(None);
        let quarter = |_: &StrokePoint| 0.25;
        walker.push(StrokePoint::new(0.0, 0.0, 1.0), &quarter, &mut count);
        for i in 1..=8 {
            walker.push(StrokePoint::new(1.0e9 * f64::from(i), 0.0, 1.0), &quarter, &mut count);
        }
        assert!(steps.get() <= MAX_STEPS_PER_STROKE, "{}", steps.get());
        // Speed spacing over absurd timestamps stops too, without iterating billions of times.
        let mut speed = PathWalker::new(None).speed_spacing(1.0);
        speed.push(StrokePoint::new(0.0, 0.0, 1.0), &quarter, &mut count);
        let mut far = StrokePoint::new(1.0e9, 0.0, 1.0);
        far.time = 1.0e12;
        let before = steps.get();
        speed.push(far, &quarter, &mut count);
        assert!(steps.get() - before <= MAX_STEPS_PER_STROKE, "{}", steps.get() - before);
    }

    #[test]
    fn a_segment_ending_at_the_cap_keeps_its_carry_over() {
        let steps = std::cell::Cell::new(0usize);
        let mut count = |_: StepInput| steps.set(steps.get() + 1);
        let mut walker = PathWalker::new(None);
        let one = |_: &StrokePoint| 1.0;
        walker.push(StrokePoint::new(0.0, 0.0, 1.0), &one, &mut count);
        // Exactly the cap in steps, then 0.75 left before the next is due.
        let end = MAX_DISTANCE_STEPS_PER_SEGMENT as f64 + 0.25;
        walker.push(StrokePoint::new(end, 0.0, 1.0), &one, &mut count);
        let before = steps.get();
        walker.push(StrokePoint::new(end + 0.8, 0.0, 1.0), &one, &mut count);
        assert_eq!(steps.get(), before + 1);
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;

    /// The step-by-step walk before the steps were streamed: every step of a point is collected
    /// first, then built. The streamed walk must produce the same dabs.
    fn buffered(brush: &BrushSettings, pts: &[StrokePoint]) -> (Vec<Dab>, Vec<Dab>) {
        let brush = brush.bounded_for_render();
        let mut smoother = Smoother::new(&brush.smoothing, 1.0);
        let mut walker = DabGenerator::new(&brush, 1.0).walker;
        let mut dual_walker = brush.dual_brush.enabled.then(|| PathWalker::new(None));
        let (mut builder, mut dual_builder) = (DabBuilder::new(&brush), brush.dual_brush.enabled.then(|| DualBuilder::new(&brush)));
        let (mut dabs, mut dual) = (Vec::new(), Vec::new());
        let mut sm = Vec::new();
        for p in pts {
            smoother.push(*p, &mut sm);
        }
        smoother.finish(&mut sm);
        for p in sm {
            let p = apply_pose(&builder.brush, p);
            let mut steps = Vec::new();
            walker.push(p, &|q| builder.step_len(q), &mut |s| steps.push(s));
            for s in &steps {
                builder.build(s, &mut dabs);
            }
            if let (Some(w), Some(db)) = (dual_walker.as_mut(), dual_builder.as_mut()) {
                let mut steps = Vec::new();
                w.push(p, &|q| db.step_len(q), &mut |s| steps.push(s));
                for s in &steps {
                    db.build(s, &mut dual);
                }
            }
        }
        let mut steps = Vec::new();
        walker.finish(&mut |s| steps.push(s));
        for s in &steps {
            builder.build(s, &mut dabs);
        }
        if let (Some(w), Some(db)) = (dual_walker.as_mut(), dual_builder.as_mut()) {
            let mut steps = Vec::new();
            w.finish(&mut |s| steps.push(s));
            for s in &steps {
                db.build(s, &mut dual);
            }
        }
        (dabs, dual)
    }

    fn streamed(brush: &BrushSettings, pts: &[StrokePoint], chunk: usize) -> (Vec<Dab>, Vec<Dab>) {
        let mut g = DabGenerator::new(brush, 1.0);
        let (mut dabs, mut dual) = (Vec::new(), Vec::new());
        for c in pts.chunks(chunk) {
            g.push(c, &mut dabs, &mut dual);
        }
        g.finish(&mut dabs, &mut dual);
        (dabs, dual)
    }

    fn path() -> Vec<StrokePoint> {
        let mut pts = Vec::new();
        for i in 0..40 {
            let t = f64::from(i);
            let mut p = StrokePoint::new(10.0 + t * 9.3, 50.0 + (t * 0.7).sin() * 30.0, 0.3 + 0.7 * ((t * 0.31).sin().abs() as f32));
            p.time = t * 7.0;
            pts.push(p);
        }
        pts
    }

    #[test]
    fn streamed_dabs_equal_the_buffered_walk() {
        let mut variants: Vec<BrushSettings> = vec![BrushSettings::default()];
        variants.push(BrushSettings { size: 14.0, spacing: 0.05, ..BrushSettings::default() });
        variants.push(BrushSettings { size: 1.0, aliased: true, ..BrushSettings::default() });
        variants.push(BrushSettings { size: 9.0, spacing_enabled: false, ..BrushSettings::default() });
        variants.push(BrushSettings { size: 12.0, build_up: true, build_up_rate: 40.0, ..BrushSettings::default() });
        let mut rich = BrushSettings { size: 18.0, spacing: 0.2, ..BrushSettings::default() };
        rich.scattering.enabled = true;
        rich.scattering.scatter.jitter = 0.5;
        rich.scattering.count = 3;
        rich.dual_brush.enabled = true;
        rich.dual_brush.size = 8.0;
        rich.dual_brush.count = 2;
        rich.shape_dynamics.enabled = true;
        rich.shape_dynamics.size.jitter = 0.6;
        rich.smoothing.amount = 0.5;
        variants.push(rich);
        for (n, b) in variants.iter().enumerate() {
            let (want, want_dual) = buffered(b, &path());
            assert!(want.len() > 10, "variant {n}: {} dabs", want.len());
            for chunk in [1, 7, 1000] {
                let (got, got_dual) = streamed(b, &path(), chunk);
                assert_eq!(got, want, "variant {n}, chunk {chunk}");
                assert_eq!(got_dual, want_dual, "variant {n}, chunk {chunk} (dual)");
            }
        }
    }

    #[test]
    fn a_nan_opacity_flow_hardness_or_spacing_paints_as_the_default() {
        let pts = path();
        let base = BrushSettings { size: 10.0, ..BrushSettings::default() };
        let (want, _) = streamed(&base, &pts, 1000);
        for broken in [
            BrushSettings { opacity: f32::NAN, ..base.clone() },
            BrushSettings { flow: f32::NAN, ..base.clone() },
            BrushSettings { hardness: f32::NAN, ..base.clone() },
            BrushSettings { spacing: f32::NAN, ..base.clone() },
        ] {
            // The default brush has opacity, flow and hardness 1 and spacing 0.1, as `base` does.
            let (got, _) = streamed(&broken, &pts, 1000);
            assert_eq!(got, want);
            let b = broken.bounded_for_render();
            assert!(b.opacity.is_finite() && b.flow.is_finite() && b.hardness.is_finite() && b.spacing.is_finite());
        }
        // Finite values are not touched, even out of range.
        let odd = BrushSettings { opacity: 3.0, flow: f32::NEG_INFINITY, spacing: f32::INFINITY, ..base };
        let b = odd.bounded_for_render();
        assert_eq!((b.opacity, b.flow, b.spacing), (3.0, f32::NEG_INFINITY, f32::INFINITY));
    }

    #[test]
    fn a_nan_dual_hardness_or_spacing_paints_as_the_default() {
        let pts = path();
        let mut explicit = BrushSettings { size: 10.0, ..BrushSettings::default() };
        explicit.dual_brush.enabled = true;
        explicit.dual_brush.size = 6.0;
        explicit.dual_brush.hardness = 1.0;
        explicit.dual_brush.spacing = 0.25;
        let (_, want_dual) = streamed(&explicit, &pts, 1000);
        assert!(!want_dual.is_empty());
        let mut broken = explicit.clone();
        broken.dual_brush.spacing = f32::NAN;
        assert_eq!(streamed(&broken, &pts, 1000).1, want_dual);
        broken.dual_brush.hardness = f32::NAN;
        let b = broken.bounded_for_render();
        assert_eq!((b.dual_brush.hardness, b.dual_brush.spacing), (1.0, 0.25));
    }

    #[test]
    fn a_point_without_a_position_does_not_poison_a_smoothed_stroke() {
        let mut brush = BrushSettings { size: 8.0, ..BrushSettings::default() };
        brush.smoothing.amount = 0.6;
        let clean = path();
        let mut dirty = clean.clone();
        dirty.insert(10, StrokePoint::new(f64::NAN, 40.0, 1.0));
        dirty.insert(20, StrokePoint::new(30.0, f64::INFINITY, 1.0));
        assert_eq!(streamed(&brush, &dirty, 1000).0, streamed(&brush, &clean, 1000).0);
        brush.smoothing.amount = 0.0;
        assert_eq!(streamed(&brush, &dirty, 1000).0, streamed(&brush, &clean, 1000).0);
    }

    #[test]
    fn a_long_pixel_grid_segment_streams() {
        let brush = BrushSettings { size: 1.0, aliased: true, spacing: 1.0, ..BrushSettings::default() };
        let (dabs, _) = streamed(&brush, &[StrokePoint::new(0.0, 0.0, 1.0), StrokePoint::new(2.0e6, 3.0, 1.0)], 1000);
        assert_eq!(dabs.len(), 2_000_001);
    }
}
