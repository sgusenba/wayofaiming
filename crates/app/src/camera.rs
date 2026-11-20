//! Camera capture thread: grabs frames, finds the aiming mark and sends the
//! aim point to the UI.

use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{Context, Result};
use nokhwa::pixel_format::LumaFormat;
use nokhwa::utils::{ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution};
use nokhwa::Camera;
use wayofaiming_core::detect::{aim_point_mm, find_mark, DetectorConfig, GrayImage, MarkDetection};
use wayofaiming_core::target::TargetSpec;

use crate::clock;
use crate::Event;

#[derive(Debug, Clone)]
pub struct CameraSettings {
    pub index: u32,
    /// Requested width, height and frame rate. `None` picks the highest frame rate.
    pub format: Option<(u32, u32, u32)>,
    pub spec: TargetSpec,
}

/// Seconds between preview images sent to the UI.
const PREVIEW_INTERVAL: f64 = 0.1;

pub fn list() -> Result<()> {
    let cams = nokhwa::query(ApiBackend::Auto).context("querying cameras")?;
    if cams.is_empty() {
        println!("No cameras found.");
    }
    for c in cams {
        println!("{}: {}", c.index(), c.human_name());
    }
    Ok(())
}

pub fn spawn(settings: CameraSettings, tx: Sender<Event>, repaint: impl Fn() + Send + 'static) {
    thread::Builder::new()
        .name("camera".into())
        .spawn(move || {
            if let Err(e) = run(&settings, &tx, &repaint) {
                let _ = tx.send(Event::Status(format!("Camera error: {e:#}")));
                repaint();
            }
        })
        .expect("spawning camera thread");
}

fn run(settings: &CameraSettings, tx: &Sender<Event>, repaint: &dyn Fn()) -> Result<()> {
    let requested = match settings.format {
        Some((w, h, fps)) => RequestedFormatType::Closest(CameraFormat::new(Resolution::new(w, h), FrameFormat::MJPEG, fps)),
        None => RequestedFormatType::AbsoluteHighestFrameRate,
    };
    let mut cam = Camera::new(CameraIndex::Index(settings.index), RequestedFormat::new::<LumaFormat>(requested))
        .with_context(|| format!("opening camera {}", settings.index))?;
    cam.open_stream().context("starting camera stream")?;
    let fmt = cam.camera_format();
    let _ = tx.send(Event::Status(format!(
        "Camera {}: {}x{} @ {} fps ({:?})",
        settings.index,
        fmt.resolution().width(),
        fmt.resolution().height(),
        fmt.frame_rate(),
        fmt.format()
    )));

    let detector = DetectorConfig::default();
    let mut last_preview = f64::NEG_INFINITY;
    loop {
        let buf = cam.frame().context("reading frame")?;
        // Timestamp on arrival; the session subtracts the configured latency.
        let t = clock::now();
        let img = buf.decode_image::<LumaFormat>().context("decoding frame")?;
        let (w, h) = (img.width() as usize, img.height() as usize);
        let data = img.into_raw();
        let gray = GrayImage { width: w, height: h, data: &data };
        let mark: Option<MarkDetection> = find_mark(&gray, &detector);
        let aim = mark.map(|m| aim_point_mm(&m, w, h, &settings.spec));
        if tx.send(Event::Frame { t, aim, mark }).is_err() {
            return Ok(()); // UI closed
        }
        if t - last_preview >= PREVIEW_INTERVAL {
            last_preview = t;
            let _ = tx.send(Event::Preview { width: w, height: h, gray: data });
        }
        repaint();
    }
}

/// Demo source: renders a synthetic target as seen by a barrel camera with a
/// wobbling hold, runs it through the real detection, and fires a shot every
/// few seconds. Lets you try the UI without camera or microphone.
pub fn spawn_demo(spec: TargetSpec, tx: Sender<Event>, repaint: impl Fn() + Send + 'static) {
    thread::Builder::new()
        .name("demo".into())
        .spawn(move || {
            let _ = tx.send(Event::Status("Demo mode: synthetic camera and shots".into()));
            let (w, h, fps) = (320usize, 240usize, 60.0);
            let radius_px = 70.0;
            let px_per_mm = radius_px / spec.black_radius;
            let detector = DetectorConfig::default();
            let mut frame = 0u64;
            let mut last_preview = f64::NEG_INFINITY;
            let mut next_shot = 3.0;
            loop {
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
                let gray = GrayImage { width: w, height: h, data: &data };
                let mark = find_mark(&gray, &detector);
                let aim = mark.map(|m| aim_point_mm(&m, w, h, &spec));
                if tx.send(Event::Frame { t: t + 0.05, aim, mark }).is_err() {
                    return;
                }
                if t >= next_shot {
                    let _ = tx.send(Event::Shot { t });
                    next_shot = t + 4.0;
                }
                if t - last_preview >= PREVIEW_INTERVAL {
                    last_preview = t;
                    let _ = tx.send(Event::Preview { width: w, height: h, gray: data });
                }
                repaint();
                frame += 1;
                let due = frame as f64 / fps;
                let sleep = due - clock::now();
                if sleep > 0.0 {
                    thread::sleep(std::time::Duration::from_secs_f64(sleep));
                }
            }
        })
        .expect("spawning demo thread");
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
