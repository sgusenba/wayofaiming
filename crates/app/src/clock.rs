//! One monotonic clock shared by camera, audio and UI, in seconds since start.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

static START: OnceLock<Instant> = OnceLock::new();

fn start() -> Instant {
    *START.get_or_init(Instant::now)
}

/// Seconds since program start.
pub fn now() -> f64 {
    start().elapsed().as_secs_f64()
}

/// Seconds since program start of a moment `ago` before now.
pub fn before_now(ago: Duration) -> f64 {
    now() - ago.as_secs_f64()
}

/// Call early in `main` so `0.0` means program start.
pub fn init() {
    start();
}
