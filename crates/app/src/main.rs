//! wayofaiming: webcam-based shooting trainer.
//!
//! Threads:
//! - camera thread: frames -> aiming-mark detection -> aim point (mm)
//! - audio callback: microphone -> shot detection -> shot time
//! - UI thread: merges both into a [`Session`] and draws it
//!
//! All timestamps use [`clock::now`] so audio and video can be aligned.

mod audio;
mod camera;
mod clock;
mod ui;

use std::sync::mpsc;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use wayofaiming_core::detect::MarkDetection;
use wayofaiming_core::session::{Session, SessionConfig};
use wayofaiming_core::target::TargetSpec;

/// Messages from the capture threads to the UI.
pub enum Event {
    Frame { t: f64, aim: Option<[f64; 2]>, mark: Option<MarkDetection> },
    Preview { width: usize, height: usize, gray: Vec<u8> },
    Shot { t: f64 },
    AudioLevel(f32),
    Status(String),
}

#[derive(Clone, Copy, ValueEnum)]
enum Target {
    Rifle,
    Pistol,
}

#[derive(Parser)]
#[command(version, about = "Webcam-based shooting trainer")]
struct Args {
    /// Camera index (see --list-cameras).
    #[arg(long, default_value_t = 0)]
    camera: u32,
    /// Requested capture width (use with --height and --fps).
    #[arg(long, requires_all = ["height", "fps"])]
    width: Option<u32>,
    #[arg(long, requires_all = ["width", "fps"])]
    height: Option<u32>,
    #[arg(long, requires_all = ["width", "height"])]
    fps: Option<u32>,
    /// Target type.
    #[arg(long, value_enum, default_value_t = Target::Rifle)]
    target: Target,
    /// Part of the microphone name to use instead of the default input.
    #[arg(long)]
    mic: Option<String>,
    /// Shot detection level, 0..1 of full scale.
    #[arg(long, default_value_t = 0.25)]
    threshold: f32,
    /// Delay from frame exposure to arrival in ms (aligns video with audio).
    #[arg(long, default_value_t = 50.0)]
    video_latency: f64,
    /// Run with a synthetic camera and shots (no hardware needed).
    #[arg(long)]
    demo: bool,
    /// List cameras and exit.
    #[arg(long)]
    list_cameras: bool,
    /// List microphones and exit.
    #[arg(long)]
    list_mics: bool,
}

fn main() -> Result<()> {
    clock::init();
    let args = Args::parse();
    if args.list_cameras || args.list_mics {
        if args.list_cameras {
            camera::list()?;
        }
        if args.list_mics {
            audio::list()?;
        }
        return Ok(());
    }

    let spec = match args.target {
        Target::Rifle => TargetSpec::AIR_RIFLE_10M,
        Target::Pistol => TargetSpec::AIR_PISTOL_10M,
    };
    let session = Session::new(
        spec.clone(),
        SessionConfig { video_latency: args.video_latency / 1000.0, ..Default::default() },
    );
    let threshold = audio::SharedThreshold::new(args.threshold);
    let format = args.width.zip(args.height).zip(args.fps).map(|((w, h), f)| (w, h, f));

    eframe::run_native(
        "wayofaiming",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1200.0, 800.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            let (tx, rx) = mpsc::channel();
            let ctx = cc.egui_ctx.clone();
            if args.demo {
                camera::spawn_demo(spec, tx, move || ctx.request_repaint());
                return Ok(Box::new(AppWithAudio { app: ui::App::new(rx, session, threshold), _stream: None }));
            }
            camera::spawn(camera::CameraSettings { index: args.camera, format, spec }, tx.clone(), move || ctx.request_repaint());

            let ctx = cc.egui_ctx.clone();
            let stream = match audio::start(args.mic.as_deref(), threshold.clone(), tx.clone(), move || ctx.request_repaint()) {
                Ok(s) => Some(s),
                Err(e) => {
                    let _ = tx.send(Event::Status(format!("Microphone error: {e:#}")));
                    None
                }
            };
            Ok(Box::new(AppWithAudio { app: ui::App::new(rx, session, threshold), _stream: stream }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Keeps the audio stream alive as long as the window.
struct AppWithAudio {
    app: ui::App,
    _stream: Option<cpal::Stream>,
}

impl eframe::App for AppWithAudio {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
    }
}
