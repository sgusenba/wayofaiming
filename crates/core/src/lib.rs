//! Hardware-independent core of wayofaiming.
//!
//! - [`detect`]: find the black aiming mark in a grayscale frame and turn it
//!   into an aim point in millimetres.
//! - [`shot`]: detect shots in microphone samples.
//! - [`session`]: merge aim trace and shots into scored shots with metrics.
//! - [`target`]: target geometry and decimal scoring.

pub mod detect;
pub mod session;
pub mod shot;
pub mod target;
