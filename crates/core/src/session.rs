//! Combines the aim trace (from the camera) with shot times (from the
//! microphone) into scored shots with hold metrics.
//!
//! Audio and video reach the program with different delays, so shot times
//! and frame times are both converted to capture time first. A shot is only
//! finalised once video up to a short follow-through window after it has
//! arrived, so the aim at the moment of the shot is always interpolated
//! from frames on both sides.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::target::TargetSpec;

/// Aim point at one camera frame. `pos` is `None` when the mark was not found.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AimSample {
    pub t: f64,
    pub pos: Option<[f64; 2]>,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Seconds of trace kept before each shot.
    pub pre_shot: f64,
    /// Seconds of follow-through trace kept after each shot.
    pub follow_through: f64,
    /// Delay from frame exposure until the program receives the frame (s).
    pub video_latency: f64,
    /// Largest gap between frames that may be interpolated across (s).
    pub max_gap: f64,
    /// Seconds of live trace kept for display.
    pub history: f64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { pre_shot: 1.5, follow_through: 0.4, video_latency: 0.05, max_gap: 0.1, history: 10.0 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shot {
    pub number: usize,
    pub t: f64,
    /// Uncalibrated aim point at the moment of the shot.
    pub raw_pos: Option<[f64; 2]>,
    /// Uncalibrated trace from `t - pre_shot` to `t + follow_through`.
    pub trace: Vec<AimSample>,
}

/// Hold quality figures, computed with the current calibration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShotMetrics {
    pub score: f64,
    pub pos: [f64; 2],
    /// Share of the last second before the shot the aim was in the 10 (0..1).
    pub in10_1s: f64,
    /// Mean aim speed over the last second / 250 ms before the shot (mm/s).
    pub speed_1s: f64,
    pub speed_250ms: f64,
    /// Mean distance of the aim from its own centre over the last second (mm).
    pub spread_1s: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub spec: TargetSpec,
    pub config: SessionConfig,
    /// Added to every raw position (mm). Set by calibration.
    pub offset: [f64; 2],
    samples: VecDeque<AimSample>,
    pending: Vec<f64>,
    shots: Vec<Shot>,
}

impl Session {
    pub fn new(spec: TargetSpec, config: SessionConfig) -> Self {
        Self { spec, config, ..Default::default() }
    }

    /// Adds a frame result. `t_received` is when the frame arrived.
    pub fn push_frame(&mut self, t_received: f64, raw_pos: Option<[f64; 2]>) {
        let t = t_received - self.config.video_latency;
        if self.samples.back().is_some_and(|s| s.t >= t) {
            return; // out of order; ignore
        }
        self.samples.push_back(AimSample { t, pos: raw_pos });
        let keep_from = t - self.config.history.max(self.config.pre_shot + self.config.follow_through + 1.0);
        while self.samples.front().is_some_and(|s| s.t < keep_from) {
            self.samples.pop_front();
        }
        self.finalize_ready();
    }

    /// Registers a shot at audio capture time `t`.
    pub fn push_shot(&mut self, t: f64) {
        self.pending.push(t);
        self.finalize_ready();
    }

    fn finalize_ready(&mut self) {
        let Some(latest) = self.samples.back().map(|s| s.t) else { return };
        let follow = self.config.follow_through;
        let (ready, waiting): (Vec<f64>, Vec<f64>) = self.pending.iter().partition(|&&t| latest >= t + follow);
        self.pending = waiting;
        for t in ready {
            let trace: Vec<AimSample> = self
                .samples
                .iter()
                .filter(|s| s.t >= t - self.config.pre_shot && s.t <= t + follow)
                .copied()
                .collect();
            let raw_pos = interpolate(&trace, t, self.config.max_gap);
            let number = self.shots.len() + 1;
            self.shots.push(Shot { number, t, raw_pos, trace });
        }
    }

    pub fn shots(&self) -> &[Shot] {
        &self.shots
    }

    pub fn has_pending_shots(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Live trace for the last `seconds`, calibrated.
    pub fn live_trace(&self, seconds: f64) -> impl Iterator<Item = AimSample> + '_ {
        let from = self.samples.back().map_or(0.0, |s| s.t - seconds);
        self.samples.iter().filter(move |s| s.t >= from).map(|s| self.calibrated(*s))
    }

    pub fn calibrated(&self, s: AimSample) -> AimSample {
        AimSample { t: s.t, pos: s.pos.map(|p| self.apply_offset(p)) }
    }

    pub fn apply_offset(&self, p: [f64; 2]) -> [f64; 2] {
        [p[0] + self.offset[0], p[1] + self.offset[1]]
    }

    pub fn metrics(&self, shot: &Shot) -> Option<ShotMetrics> {
        let pos = self.apply_offset(shot.raw_pos?);
        let window = |secs: f64| -> Vec<(f64, [f64; 2])> {
            shot.trace
                .iter()
                .filter(|s| s.t >= shot.t - secs && s.t <= shot.t)
                .filter_map(|s| s.pos.map(|p| (s.t, self.apply_offset(p))))
                .collect()
        };
        let last_1s = window(1.0);
        let in10 = if last_1s.is_empty() {
            0.0
        } else {
            last_1s.iter().filter(|(_, p)| self.spec.score(*p) >= 10.0).count() as f64 / last_1s.len() as f64
        };
        let spread = if last_1s.is_empty() {
            0.0
        } else {
            let n = last_1s.len() as f64;
            let c = last_1s.iter().fold([0.0, 0.0], |a, (_, p)| [a[0] + p[0] / n, a[1] + p[1] / n]);
            last_1s.iter().map(|(_, p)| (p[0] - c[0]).hypot(p[1] - c[1])).sum::<f64>() / n
        };
        Some(ShotMetrics {
            score: self.spec.score(pos),
            pos,
            in10_1s: in10,
            speed_1s: mean_speed(&last_1s),
            speed_250ms: mean_speed(&window(0.25)),
            spread_1s: spread,
        })
    }

    /// Shifts the calibration so the mean of the last `n` shots lands on the
    /// target centre. Use after shooting a group with a correctly zeroed gun.
    pub fn calibrate_to_group_centre(&mut self, n: usize) -> bool {
        let pts: Vec<[f64; 2]> = self.shots.iter().rev().filter_map(|s| s.raw_pos).take(n).collect();
        if pts.is_empty() {
            return false;
        }
        let k = pts.len() as f64;
        let mean = pts.iter().fold([0.0, 0.0], |a, p| [a[0] + p[0] / k, a[1] + p[1] / k]);
        self.offset = [-mean[0], -mean[1]];
        true
    }

    /// Session holding already finished shots, e.g. loaded from disk.
    pub fn from_shots(spec: TargetSpec, config: SessionConfig, offset: [f64; 2], shots: Vec<Shot>) -> Self {
        Self { spec, config, offset, shots, ..Default::default() }
    }

    /// Removes all shots and the trace; keeps calibration.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.pending.clear();
        self.shots.clear();
    }

    /// CSV with one row per shot.
    pub fn shots_csv(&self) -> String {
        let mut out = String::from("shot,time_s,x_mm,y_mm,score,in10_1s_pct,speed_1s_mm_s,speed_250ms_mm_s,spread_1s_mm\n");
        for shot in &self.shots {
            match self.metrics(shot) {
                Some(m) => out.push_str(&format!(
                    "{},{:.4},{:.2},{:.2},{:.1},{:.0},{:.1},{:.1},{:.2}\n",
                    shot.number, shot.t, m.pos[0], m.pos[1], m.score, m.in10_1s * 100.0, m.speed_1s, m.speed_250ms, m.spread_1s
                )),
                None => out.push_str(&format!("{},{:.4},,,,,,,\n", shot.number, shot.t)),
            }
        }
        out
    }
}

/// Linear interpolation of the aim point at time `t`.
pub fn interpolate(trace: &[AimSample], t: f64, max_gap: f64) -> Option<[f64; 2]> {
    let after = trace.iter().position(|s| s.t >= t)?;
    let b = trace[after];
    if b.t == t {
        return b.pos;
    }
    let a = trace[after.checked_sub(1)?];
    let (pa, pb) = (a.pos?, b.pos?);
    if b.t - a.t > max_gap {
        return None;
    }
    let f = (t - a.t) / (b.t - a.t);
    Some([pa[0] + (pb[0] - pa[0]) * f, pa[1] + (pb[1] - pa[1]) * f])
}

fn mean_speed(pts: &[(f64, [f64; 2])]) -> f64 {
    if pts.len() < 2 {
        return 0.0;
    }
    let dist: f64 = pts.windows(2).map(|w| (w[1].1[0] - w[0].1[0]).hypot(w[1].1[1] - w[0].1[1])).sum();
    let dt = pts[pts.len() - 1].0 - pts[0].0;
    if dt > 0.0 {
        dist / dt
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session::new(TargetSpec::AIR_RIFLE_10M, SessionConfig { video_latency: 0.0, ..Default::default() })
    }

    #[test]
    fn shot_waits_for_follow_through_then_interpolates() {
        let mut s = session();
        // Aim moves right at 10 mm/s, 100 fps.
        for i in 0..100 {
            s.push_frame(i as f64 * 0.01, Some([i as f64 * 0.1, 1.0]));
        }
        s.push_shot(0.8);
        assert!(s.shots().is_empty() && s.has_pending_shots());
        for i in 100..140 {
            s.push_frame(i as f64 * 0.01, Some([i as f64 * 0.1, 1.0]));
        }
        assert_eq!(s.shots().len(), 1);
        let p = s.shots()[0].raw_pos.unwrap();
        assert!((p[0] - 8.0).abs() < 1e-9 && (p[1] - 1.0).abs() < 1e-9, "{p:?}");
        let m = s.metrics(&s.shots()[0]).unwrap();
        assert!((m.speed_1s - 10.0).abs() < 0.01, "{m:?}");
    }

    #[test]
    fn video_latency_shifts_frames_onto_audio_clock() {
        let mut s = Session::new(TargetSpec::AIR_RIFLE_10M, SessionConfig { video_latency: 0.1, ..Default::default() });
        for i in 0..200 {
            // Frame captured at i*0.01 arrives 0.1 s later.
            s.push_frame(i as f64 * 0.01 + 0.1, Some([i as f64 * 0.01, 0.0]));
        }
        s.push_shot(1.0);
        for i in 200..260 {
            s.push_frame(i as f64 * 0.01 + 0.1, Some([i as f64 * 0.01, 0.0]));
        }
        let p = s.shots()[0].raw_pos.unwrap();
        assert!((p[0] - 1.0).abs() < 1e-9, "{p:?}");
    }

    #[test]
    fn steady_centre_hold_is_all_in_10() {
        let mut s = session();
        for i in 0..300 {
            s.push_frame(i as f64 * 0.01, Some([0.1, -0.1]));
        }
        s.push_shot(2.0);
        for i in 300..400 {
            s.push_frame(i as f64 * 0.01, Some([0.1, -0.1]));
        }
        let m = s.metrics(&s.shots()[0]).unwrap();
        assert_eq!(m.score, 10.9);
        assert_eq!(m.in10_1s, 1.0);
        assert_eq!(m.speed_1s, 0.0);
    }

    #[test]
    fn lost_mark_at_shot_gives_no_position() {
        let mut s = session();
        for i in 0..200 {
            let pos = if (90..110).contains(&i) { None } else { Some([0.0, 0.0]) };
            s.push_frame(i as f64 * 0.01, pos);
        }
        s.push_shot(1.0);
        for i in 200..250 {
            s.push_frame(i as f64 * 0.01, Some([0.0, 0.0]));
        }
        assert!(s.shots()[0].raw_pos.is_none());
        assert!(s.metrics(&s.shots()[0]).is_none());
    }

    #[test]
    fn calibration_moves_group_to_centre() {
        let mut s = session();
        let mut t = 0.0;
        for (i, x) in [4.0, 6.0].iter().enumerate() {
            for _ in 0..100 {
                t += 0.01;
                s.push_frame(t, Some([*x, -3.0]));
            }
            s.push_shot(t - 0.5);
            for _ in 0..60 {
                t += 0.01;
                s.push_frame(t, Some([*x, -3.0]));
            }
            assert_eq!(s.shots().len(), i + 1);
        }
        assert!(s.calibrate_to_group_centre(10));
        let a = s.metrics(&s.shots()[0]).unwrap().pos;
        let b = s.metrics(&s.shots()[1]).unwrap().pos;
        assert!((a[0] + 1.0).abs() < 1e-9 && (b[0] - 1.0).abs() < 1e-9 && a[1].abs() < 1e-9);
        assert!(s.shots_csv().lines().count() == 3);
    }
}
