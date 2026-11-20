//! wayofaiming: webcam-based shooting trainer.
//!
//! Threads:
//! - camera thread: frames -> aiming-mark detection -> aim point
//! - audio callback: microphone -> shot detection -> shot time
//! - UI thread: merges both into a session, draws it, saves it
//!
//! All timestamps use [`clock::now`] so audio and video can be aligned.

// No console window for the Windows release build.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod camera;
mod clock;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use wayofaiming_core::detect::MarkDetection;
use wayofaiming_core::storage;
use wayofaiming_core::target::TargetSpec;

/// Messages from the capture threads to the UI.
pub enum Event {
    /// Aim point in units of the black's radius (y up); the UI converts to mm.
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

/// All options except the capture format are remembered between runs and
/// can also be changed in the window.
#[derive(Parser)]
#[command(version, about = "Webcam-based shooting trainer")]
struct Args {
    /// Camera index (see --list-cameras).
    #[arg(long)]
    camera: Option<u32>,
    /// Requested capture width (use with --height and --fps).
    #[arg(long, requires_all = ["height", "fps"])]
    width: Option<u32>,
    #[arg(long, requires_all = ["width", "fps"])]
    height: Option<u32>,
    #[arg(long, requires_all = ["width", "height"])]
    fps: Option<u32>,
    /// Target type.
    #[arg(long, value_enum)]
    target: Option<Target>,
    /// Microphone name (or part of it) instead of the system default.
    #[arg(long)]
    mic: Option<String>,
    /// Shot detection level, 0..1 of full scale.
    #[arg(long)]
    threshold: Option<f32>,
    /// Delay from frame exposure to arrival in ms (aligns video with audio).
    #[arg(long)]
    video_latency: Option<f64>,
    /// Run with a synthetic camera and shots (no hardware needed).
    #[arg(long)]
    demo: bool,
    /// Where sessions and settings are stored.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// List cameras and exit.
    #[arg(long)]
    list_cameras: bool,
    /// List microphones and exit.
    #[arg(long)]
    list_mics: bool,
}

/// The release build has no console; reattach to the one we were started
/// from (if any) so --help and --list-* still print.
fn attach_console() {
    #[cfg(windows)]
    // SAFETY: plain Win32 call without pointers; failure just means no console.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

fn main() -> Result<()> {
    attach_console();
    clock::init();
    let args = Args::parse();
    if args.list_cameras || args.list_mics {
        if args.list_cameras {
            println!("Cameras:");
            for (i, name) in camera::available() {
                println!("  {i}: {name}");
            }
        }
        if args.list_mics {
            println!("Microphones:");
            for name in audio::available() {
                println!("  {name}");
            }
        }
        return Ok(());
    }

    let data_dir = args
        .data_dir
        .or_else(|| dirs::data_dir().map(|d| d.join("wayofaiming")))
        .unwrap_or_else(|| PathBuf::from("wayofaiming-data"));
    let mut settings = storage::load_settings(&data_dir.join("settings.json"));
    if let Some(c) = args.camera {
        settings.camera = c;
    }
    if let Some(t) = args.target {
        settings.target = match t {
            Target::Rifle => TargetSpec::AIR_RIFLE_10M.name,
            Target::Pistol => TargetSpec::AIR_PISTOL_10M.name,
        }
        .to_string();
    }
    if args.mic.is_some() {
        settings.mic = args.mic;
    }
    if let Some(t) = args.threshold {
        settings.shot_threshold = t;
    }
    if let Some(ms) = args.video_latency {
        settings.video_latency = ms / 1000.0;
    }
    let options = ui::AppOptions {
        data_dir,
        demo: args.demo,
        camera_format: args.width.zip(args.height).zip(args.fps).map(|((w, h), f)| (w, h, f)),
    };

    eframe::run_native(
        "wayofaiming",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1250.0, 820.0]).with_title("wayofaiming"),
            ..Default::default()
        },
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc.egui_ctx.clone(), options, settings)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
