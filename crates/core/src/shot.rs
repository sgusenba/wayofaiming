//! Shot detection from microphone samples.
//!
//! A shot (or the click of a dry-fire trigger) is a sharp transient. The
//! detector tracks the background noise level and fires when the signal
//! exceeds both an absolute threshold and a multiple of the background,
//! then ignores further input for a refractory period (echoes, follow-up
//! noise from the action).

#[derive(Debug, Clone)]
pub struct ShotDetectorConfig {
    /// Absolute peak level (0..1 of full scale) a shot must exceed.
    pub threshold: f32,
    /// A shot must also be this many times louder than the background RMS.
    pub min_snr: f32,
    /// Seconds after a shot during which no new shot is reported.
    pub refractory: f64,
    /// Time constant (s) of the background noise estimate.
    pub noise_time_constant: f64,
}

impl Default for ShotDetectorConfig {
    fn default() -> Self {
        Self { threshold: 0.25, min_snr: 8.0, refractory: 1.0, noise_time_constant: 0.5 }
    }
}

#[derive(Debug, Clone)]
pub struct ShotDetector {
    pub config: ShotDetectorConfig,
    sample_rate: f64,
    noise_power: f32,
    last_shot: Option<f64>,
    samples_seen: u64,
    /// Peak level of the most recently processed block (for a level meter).
    pub last_peak: f32,
}

impl ShotDetector {
    pub fn new(config: ShotDetectorConfig, sample_rate: f64) -> Self {
        Self { config, sample_rate, noise_power: 1e-6, last_shot: None, samples_seen: 0, last_peak: 0.0 }
    }

    /// Processes a block of mono samples whose first sample was captured at
    /// time `t0` (seconds). Returns the times of detected shots.
    pub fn process(&mut self, samples: &[f32], t0: f64) -> Vec<f64> {
        let mut shots = Vec::new();
        let alpha = (1.0 / (self.config.noise_time_constant * self.sample_rate)) as f32;
        let mut peak = 0.0f32;
        for (i, &s) in samples.iter().enumerate() {
            let t = t0 + i as f64 / self.sample_rate;
            let a = s.abs();
            peak = peak.max(a);
            // Let the noise estimate settle before reporting anything.
            self.samples_seen += 1;
            let warming_up = (self.samples_seen as f64) < 3.0 * self.config.noise_time_constant * self.sample_rate;
            let in_refractory = self.last_shot.is_some_and(|ls| t - ls < self.config.refractory);
            if in_refractory {
                continue;
            }
            let noise_rms = self.noise_power.sqrt();
            if !warming_up && a > self.config.threshold && a > self.config.min_snr * noise_rms {
                self.last_shot = Some(t);
                shots.push(t);
            } else {
                self.noise_power += alpha * (s * s - self.noise_power);
            }
        }
        self.last_peak = peak;
        shots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 48_000.0;

    /// Deterministic pseudo-noise in [-amp, amp].
    fn noise(n: usize, amp: f32, seed: &mut u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 17;
                *seed ^= *seed << 5;
                (*seed as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    fn add_bang(buf: &mut [f32], at: usize) {
        for (k, s) in buf[at..].iter_mut().take(2000).enumerate() {
            *s += 0.9 * (-(k as f32) / 300.0).exp() * if k % 2 == 0 { 1.0 } else { -1.0 };
        }
    }

    #[test]
    fn detects_shot_time_accurately() {
        let mut seed = 1;
        let mut buf = noise(SR as usize * 3, 0.01, &mut seed);
        let at = (SR * 1.5) as usize;
        add_bang(&mut buf, at);

        let mut det = ShotDetector::new(Default::default(), SR);
        // Feed in realistic block sizes.
        let mut shots = Vec::new();
        for (b, block) in buf.chunks(512).enumerate() {
            shots.extend(det.process(block, 10.0 + (b * 512) as f64 / SR));
        }
        assert_eq!(shots.len(), 1, "{shots:?}");
        assert!((shots[0] - 11.5).abs() < 0.001, "{shots:?}");
    }

    #[test]
    fn refractory_suppresses_echo_but_allows_next_shot() {
        let mut seed = 7;
        let mut buf = noise(SR as usize * 4, 0.01, &mut seed);
        add_bang(&mut buf, (SR * 1.6) as usize);
        add_bang(&mut buf, (SR * 1.8) as usize); // echo
        add_bang(&mut buf, (SR * 3.0) as usize); // next shot
        let mut det = ShotDetector::new(Default::default(), SR);
        let shots = det.process(&buf, 0.0);
        assert_eq!(shots.len(), 2, "{shots:?}");
        assert!((shots[1] - 3.0).abs() < 0.001);
    }

    #[test]
    fn loud_steady_noise_is_not_a_shot() {
        let mut seed = 3;
        let buf = noise(SR as usize * 3, 0.3, &mut seed);
        let mut det = ShotDetector::new(Default::default(), SR);
        // Let the noise estimate settle first, as it does after startup.
        det.process(&buf[..SR as usize], 0.0);
        assert!(det.process(&buf[SR as usize..], 1.0).is_empty());
    }
}
