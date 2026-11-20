//! Camera capture thread: grabs frames, finds the aiming mark and sends the
//! aim point to the UI. The thread can be stopped so the user can switch
//! cameras at runtime.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use nokhwa::pixel_format::LumaFormat;
use nokhwa::utils::{ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution};
use nokhwa::Camera;
use wayofaiming_core::detect::{aim_point_radii, find_mark, DetectorConfig, GrayImage};
use wayofaiming_core::target::TargetSpec;

use crate::clock;
use crate::Event;

/// Seconds between preview images sent to the UI.
const PREVIEW_INTERVAL: f64 = 0.1;

#[derive(Debug, Clone)]
pub enum Source {
    Camera {
        index: u32,
        /// Requested width, height and frame rate. `None` picks the highest frame rate.
        format: Option<(u32, u32, u32)>,
    },
    Demo,
}

/// A running capture thread. Dropping it stops the thread.
pub struct CameraHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for CameraHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // Waits at most for one frame, then the device is free again.
            let _ = t.join();
        }
    }
}

/// Cameras as (index, name).
pub fn available() -> Vec<(u32, String)> {
    nokhwa::query(ApiBackend::Auto)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| match c.index() {
            CameraIndex::Index(i) => Some((*i, c.human_name())),
            CameraIndex::String(_) => None,
        })
        .collect()
}

pub fn spawn(source: Source, tx: Sender<Event>, repaint: impl Fn() + Send + 'static) -> CameraHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::Builder::new()
        .name("camera".into())
        .spawn(move || {
            let result = match source {
                Source::Camera { index, format } => run(index, format, &tx, &repaint, &stop_thread),
                Source::Demo => run_demo(&tx, &repaint, &stop_thread),
            };
            if let Err(e) = result {
                let _ = tx.send(Event::Status(format!("Camera error: {e:#}")));
                repaint();
            }
        })
        .expect("spawning camera thread");
    CameraHandle { stop, thread: Some(thread) }
}

fn run(index: u32, format: Option<(u32, u32, u32)>, tx: &Sender<Event>, repaint: &dyn Fn(), stop: &AtomicBool) -> Result<()> {
    let requested = match format {
        Some((w, h, fps)) => RequestedFormatType::Closest(CameraFormat::new(Resolution::new(w, h), FrameFormat::MJPEG, fps)),
        None => RequestedFormatType::AbsoluteHighestFrameRate,
    };
    let mut cam = Camera::new(CameraIndex::Index(index), RequestedFormat::new::<LumaFormat>(requested))
        .with_context(|| format!("opening camera {index}"))?;
    cam.open_stream().context("starting camera stream")?;
    let fmt = cam.camera_format();
    let _ = tx.send(Event::Status(format!(
        "Camera {index}: {}x{} @ {} fps ({:?})",
        fmt.resolution().width(),
        fmt.resolution().height(),
        fmt.frame_rate(),
        fmt.format()
    )));

    let detector = DetectorConfig::default();
    let mut last_preview = f64::NEG_INFINITY;
    while !stop.load(Ordering::Relaxed) {
        let buf = cam.frame().context("reading frame")?;
        // Timestamp on arrival; the session subtracts the configured latency.
        let t = clock::now();
        let img = buf.decode_image::<LumaFormat>().context("decoding frame")?;
        let (w, h) = (img.width() as usize, img.height() as usize);
        let data = img.into_raw();
        if !send_frame(tx, &detector, t, w, h, data, &mut last_preview) {
            break;
        }
        repaint();
    }
    let _ = cam.stop_stream();
    Ok(())
}

/// Detects the mark and sends the result (and sometimes a preview).
/// Returns `false` once the UI has gone away.
fn send_frame(tx: &Sender<Event>, detector: &DetectorConfig, t: f64, w: usize, h: usize, data: Vec<u8>, last_preview: &mut f64) -> bool {
    let mark = find_mark(&GrayImage { width: w, height: h, data: &data }, detector);
    let aim = mark.map(|m| aim_point_radii(&m, w, h));
    if tx.send(Event::Frame { t, aim, mark }).is_err() {
        return false;
    }
    if t - *last_preview >= PREVIEW_INTERVAL {
        *last_preview = t;
        let _ = tx.send(Event::Preview { width: w, height: h, gray: data });
    }
    true
}

/// Demo source: renders a synthetic target as seen by a barrel camera with a
/// wobbling hold, runs it through the real detection, and fires a shot every
/// few seconds. Lets you try the program without camera or microphone.
fn run_demo(tx: &Sender<Event>, repaint: &dyn Fn(), stop: &AtomicBool) -> Result<()> {
    let _ = tx.send(Event::Status("Demo mode: synthetic camera and shots".into()));
    let spec = TargetSpec::AIR_RIFLE_10M;
    let (w, h, fps) = (320usize, 240usize, 60.0);
    let radius_px = 70.0;
    let px_per_mm = radius_px / spec.black_radius;
    let detector = DetectorConfig::default();
    let mut last_preview = f64::NEG_INFINITY;
    let start = clock::now();
    let mut next_shot = start + 3.0;
    let mut frame = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let t = clock::now();
        // Hold: slow drift plus tremor, settling before each shot.
        let phase = (next_shot - t).clamp(0.0, 3.0) / 3.0;
        let amp = 1.0 + 6.0 * phase;
        let aim = [
            amp * (t * 1.3).sin() + 0.6 * (t * 7.1).sin() + 0.8,
            amp * (t * 0.9 + 1.0).cos() + 0.5 * (t * 9.3).cos() - 0.5,
        ];
        // The mark appears opposite to where the barrel points.
        let cx = w as f64 / 2.0 - aim[0] * px_per_mm;
        let cy = h as f64 / 2.0 + aim[1] * px_per_mm;
        let data = render_target(w, h, cx, cy, radius_px, &spec);
        // Pretend the frame took 50 ms to arrive, like a real camera.
        if !send_frame(tx, &detector, t + 0.05, w, h, data, &mut last_preview) {
            break;
        }
        if t >= next_shot {
            let _ = tx.send(Event::Shot { t });
            next_shot = t + 4.0;
        }
        repaint();
        frame += 1;
        let sleep = start + frame as f64 / fps - clock::now();
        if sleep > 0.0 {
            thread::sleep(std::time::Duration::from_secs_f64(sleep));
        }
    }
    Ok(())
}

fn render_target(w: usize, h: usize, cx: f64, cy: f64, r: f64, spec: &TargetSpec) -> Vec<u8> {
    let px_per_mm = r / spec.black_radius;
    let rings: Vec<f64> = (spec.first_ring_in_black + 1..=9).map(|n| spec.ring_radius(n) * px_per_mm).collect();
    let mut img = vec![225u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let d = (x as f64 + 0.5 - cx).hypot(y as f64 + 0.5 - cy);
            if d <= r && !rings.iter().any(|rr| (d - rr).abs() < 0.6) {
                img[y * w + x] = 25;
            }
        }
    }
    img
}
