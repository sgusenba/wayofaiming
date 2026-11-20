//! Microphone input: runs the shot detector inside the audio callback and
//! sends shot times (on the shared clock) to the UI.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use wayofaiming_core::shot::{ShotDetector, ShotDetectorConfig};

use crate::clock;
use crate::Event;

/// Detector threshold shared with the UI (f32 bits), so it can be tuned live.
#[derive(Clone)]
pub struct SharedThreshold(Arc<AtomicU32>);

impl SharedThreshold {
    pub fn new(v: f32) -> Self {
        Self(Arc::new(AtomicU32::new(v.to_bits())))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

/// Names of the available microphones.
pub fn available() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devs| devs.filter_map(|d| d.description().ok().map(|n| n.name().to_string())).collect())
        .unwrap_or_default()
}

/// Starts capturing. Keep the returned stream alive for as long as audio is needed.
pub fn start(
    device_name: Option<&str>,
    threshold: SharedThreshold,
    tx: Sender<Event>,
    repaint: impl Fn() + Send + 'static,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = match device_name {
        None => host.default_input_device().ok_or_else(|| anyhow!("no default microphone"))?,
        Some(want) => host
            .input_devices()?
            .find(|d| d.description().is_ok_and(|n| n.name().to_lowercase().contains(&want.to_lowercase())))
            .ok_or_else(|| anyhow!("no microphone matching {want:?}"))?,
    };
    let name = device.description().map(|d| d.name().to_string()).unwrap_or_default();
    let supported = device.default_input_config().context("querying microphone format")?;
    let config = supported.config();
    let _ = tx.send(Event::Status(format!("Microphone: {name} ({} Hz, {} ch)", config.sample_rate, config.channels)));

    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, threshold, tx, repaint),
        SampleFormat::I16 => build::<i16>(&device, &config, threshold, tx, repaint),
        SampleFormat::I32 => build::<i32>(&device, &config, threshold, tx, repaint),
        SampleFormat::U16 => build::<u16>(&device, &config, threshold, tx, repaint),
        other => Err(anyhow!("unsupported sample format {other:?}")),
    }?;
    stream.play().context("starting microphone")?;
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    threshold: SharedThreshold,
    tx: Sender<Event>,
    repaint: impl Fn() + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let rate = f64::from(config.sample_rate);
    let mut detector = ShotDetector::new(
        ShotDetectorConfig { threshold: threshold.get(), ..Default::default() },
        rate,
    );
    let mut mono: Vec<f32> = Vec::new();
    let err_tx = tx.clone();
    let stream = device.build_input_stream(
        *config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            // How long ago the first sample of this block was captured.
            let ts = info.timestamp();
            let t0 = clock::before_now(ts.callback.duration_since(ts.capture));
            mono.clear();
            // First channel only: averaging channels can cancel a transient.
            mono.extend(data.chunks(channels).map(|f| <f32 as FromSample<T>>::from_sample_(f[0])));
            detector.config.threshold = threshold.get();
            let shots = detector.process(&mono, t0);
            let _ = tx.send(Event::AudioLevel(detector.last_peak));
            for t in shots {
                let _ = tx.send(Event::Shot { t });
            }
            repaint();
        },
        move |e| {
            let _ = err_tx.send(Event::Status(format!("Microphone error: {e}")));
        },
        None,
    )?;
    Ok(stream)
}
