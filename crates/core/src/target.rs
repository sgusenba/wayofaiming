//! Target geometry and ISSF-style decimal scoring.
//!
//! All coordinates are in millimetres on the target face, origin at the
//! centre, x to the right, y upwards.

/// Geometry of a ring target.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetSpec {
    pub name: &'static str,
    /// Outer radius of the 10 ring (mm).
    pub ring10_radius: f64,
    /// Radial distance between neighbouring rings (mm).
    pub ring_step: f64,
    /// Radius of the black aiming mark (mm). Used to scale camera pixels to mm.
    pub black_radius: f64,
    /// Lowest ring whose outline lies inside the black (drawn white on black).
    pub first_ring_in_black: u8,
    /// Projectile calibre (mm). Scoring uses the projectile edge, not its centre.
    pub calibre: f64,
}

impl TargetSpec {
    /// ISSF 10 m air rifle target.
    pub const AIR_RIFLE_10M: TargetSpec = TargetSpec {
        name: "10 m air rifle",
        ring10_radius: 0.25,
        ring_step: 2.5,
        black_radius: 15.25,
        first_ring_in_black: 4,
        calibre: 4.5,
    };

    /// ISSF 10 m air pistol target.
    pub const AIR_PISTOL_10M: TargetSpec = TargetSpec {
        name: "10 m air pistol",
        ring10_radius: 5.75,
        ring_step: 8.0,
        black_radius: 29.75,
        first_ring_in_black: 7,
        calibre: 4.5,
    };

    /// Outer radius of ring `n` (1..=10) in mm.
    pub fn ring_radius(&self, n: u8) -> f64 {
        self.ring10_radius + f64::from(10 - n.clamp(1, 10)) * self.ring_step
    }

    /// Decimal score (0.0, or 1.0..=10.9) for a projectile centred at `pos`.
    pub fn score(&self, pos: [f64; 2]) -> f64 {
        let dist = pos[0].hypot(pos[1]);
        // Distance from target centre to the nearest edge of the projectile.
        let edge = dist - self.calibre / 2.0;
        if edge > self.ring_radius(1) {
            return 0.0;
        }
        let raw = 10.0 + (self.ring10_radius - edge) / self.ring_step;
        // Truncate to one decimal (scores are never rounded up).
        let truncated = (raw * 10.0 + 1e-9).floor() / 10.0;
        truncated.clamp(1.0, 10.9)
    }
}

impl Default for TargetSpec {
    fn default() -> Self {
        Self::AIR_RIFLE_10M
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: TargetSpec = TargetSpec::AIR_RIFLE_10M;
    const P: TargetSpec = TargetSpec::AIR_PISTOL_10M;

    #[test]
    fn ring_radii_match_issf_dimensions() {
        // Ring 1 diameter: rifle 45.5 mm, pistol 155.5 mm.
        assert!((R.ring_radius(1) * 2.0 - 45.5).abs() < 1e-9);
        assert!((P.ring_radius(1) * 2.0 - 155.5).abs() < 1e-9);
        // The black covers ring 4 (rifle) and ring 7 (pistol).
        assert!((R.ring_radius(R.first_ring_in_black) - R.black_radius).abs() < 1e-9);
        assert!((P.ring_radius(P.first_ring_in_black) - P.black_radius).abs() < 1e-9);
    }

    #[test]
    fn centre_shot_is_10_9() {
        assert_eq!(R.score([0.0, 0.0]), 10.9);
        assert_eq!(P.score([0.0, 0.0]), 10.9);
    }

    #[test]
    fn ring_boundaries() {
        // A pellet touching the 10 ring line from outside still scores 10.0.
        assert_eq!(R.score([2.5, 0.0]), 10.0);
        assert_eq!(R.score([2.51, 0.0]), 9.9);
        assert_eq!(R.score([0.0, -5.0]), 9.0);
        assert_eq!(P.score([8.0, 0.0]), 10.0);
    }

    #[test]
    fn miss_scores_zero() {
        assert_eq!(R.score([30.0, 0.0]), 0.0);
        // Pellet edge just touching ring 1 counts.
        assert_eq!(R.score([R.ring_radius(1) + 2.25, 0.0]), 1.0);
    }
}
