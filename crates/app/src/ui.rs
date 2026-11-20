//! egui front end: target with live and per-shot aim trace, shot list,
//! metrics and settings.

use std::sync::mpsc::Receiver;

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2};
use wayofaiming_core::detect::MarkDetection;
use wayofaiming_core::session::{AimSample, Session, Shot};
use wayofaiming_core::target::TargetSpec;

use crate::audio::SharedThreshold;
use crate::Event;

/// Shots used by "Centre group" calibration.
const CALIBRATION_SHOTS: usize = 5;
/// Seconds of live trace drawn while aiming.
const LIVE_TRACE: f64 = 1.0;

pub struct App {
    rx: Receiver<Event>,
    session: Session,
    threshold: SharedThreshold,
    selected: Option<usize>,
    zoom: f32,
    show_preview: bool,
    preview: Option<TextureHandle>,
    preview_size: [usize; 2],
    last_mark: Option<MarkDetection>,
    audio_level: f32,
    status: Vec<String>,
    fps: FpsCounter,
}

impl App {
    pub fn new(rx: Receiver<Event>, session: Session, threshold: SharedThreshold) -> Self {
        Self {
            rx,
            session,
            threshold,
            selected: None,
            zoom: 2.0,
            show_preview: true,
            preview: None,
            preview_size: [0, 0],
            last_mark: None,
            audio_level: 0.0,
            status: Vec::new(),
            fps: FpsCounter::default(),
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        let shots_before = self.session.shots().len();
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Frame { t, aim, mark } => {
                    self.session.push_frame(t, aim);
                    self.last_mark = mark;
                    self.fps.tick(t);
                }
                Event::Shot { t } => self.session.push_shot(t),
                Event::AudioLevel(l) => self.audio_level = self.audio_level.max(l),
                Event::Preview { width, height, gray } => {
                    let img = egui::ColorImage::from_gray([width, height], &gray);
                    self.preview_size = [width, height];
                    match &mut self.preview {
                        Some(tex) => tex.set(img, TextureOptions::LINEAR),
                        None => self.preview = Some(ctx.load_texture("camera", img, TextureOptions::LINEAR)),
                    }
                }
                Event::Status(s) => {
                    self.status.push(s);
                    if self.status.len() > 6 {
                        self.status.remove(0);
                    }
                }
            }
        }
        if self.session.shots().len() > shots_before {
            self.selected = Some(self.session.shots().len() - 1);
        }
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("wayofaiming");
        ui.separator();

        ui.label(format!("Camera: {:.0} fps", self.fps.fps));
        ui.label(match &self.last_mark {
            Some(m) => format!("Target found (r = {:.0} px)", m.radius),
            None => "Target NOT found".into(),
        });
        ui.horizontal(|ui| {
            ui.label("Mic");
            let thr = self.threshold.get();
            ui.add(egui::ProgressBar::new(self.audio_level.min(1.0)).desired_width(120.0).fill(
                if self.audio_level > thr { Color32::from_rgb(200, 60, 60) } else { Color32::from_rgb(60, 140, 60) },
            ));
        });
        // Peak-hold meter that decays each frame.
        self.audio_level *= 0.9;

        ui.separator();
        ui.label("Target");
        let mut spec = self.session.spec.clone();
        egui::ComboBox::from_id_salt("target").selected_text(spec.name).show_ui(ui, |ui| {
            for s in [TargetSpec::AIR_RIFLE_10M, TargetSpec::AIR_PISTOL_10M] {
                let name = s.name;
                ui.selectable_value(&mut spec, s, name);
            }
        });
        if spec != self.session.spec {
            // The camera thread scales by the black radius it was started with,
            // so switching only changes scoring/display until restart.
            self.session.spec = spec;
        }

        let mut thr = self.threshold.get();
        if ui.add(egui::Slider::new(&mut thr, 0.02..=1.0).text("shot threshold")).changed() {
            self.threshold.set(thr);
        }
        let mut latency_ms = self.session.config.video_latency * 1000.0;
        if ui.add(egui::Slider::new(&mut latency_ms, 0.0..=300.0).text("video latency ms")).changed() {
            self.session.config.video_latency = latency_ms / 1000.0;
        }
        ui.add(egui::Slider::new(&mut self.zoom, 1.0..=8.0).text("zoom"));

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui.button("Centre group").on_hover_text("Calibrate: move the mean of the last 5 shots to the centre").clicked()
                && !self.session.calibrate_to_group_centre(CALIBRATION_SHOTS)
            {
                self.status.push("Calibration needs shots with a position".into());
            }
            if ui.button("Reset calibration").clicked() {
                self.session.offset = [0.0, 0.0];
            }
            if ui.button("Clear shots").clicked() {
                self.session.clear();
                self.selected = None;
            }
            if ui.button("Export CSV").clicked() {
                self.export_csv();
            }
        });
        ui.label(format!("Calibration offset: {:+.1}, {:+.1} mm", self.session.offset[0], self.session.offset[1]));

        ui.separator();
        self.shot_table(ui);

        ui.separator();
        ui.checkbox(&mut self.show_preview, "Camera preview");
        if self.show_preview {
            self.preview(ui);
        }

        ui.separator();
        for s in &self.status {
            ui.small(s);
        }
    }

    fn shot_table(&mut self, ui: &mut egui::Ui) {
        let shots = self.session.shots();
        if shots.is_empty() {
            ui.label("No shots yet.");
            return;
        }
        let total: f64 = shots.iter().filter_map(|s| self.session.metrics(s)).map(|m| m.score).sum();
        ui.label(format!("{} shots, total {:.1}", shots.len(), total));
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            egui::Grid::new("shots").striped(true).show(ui, |ui| {
                for h in ["#", "score", "in 10 (1 s)", "speed 1 s", "speed 250 ms"] {
                    ui.strong(h);
                }
                ui.end_row();
                for (i, shot) in shots.iter().enumerate().rev() {
                    let label = format!("{}", shot.number);
                    if ui.selectable_label(self.selected == Some(i), label).clicked() {
                        self.selected = Some(i);
                    }
                    match self.session.metrics(shot) {
                        Some(m) => {
                            ui.label(format!("{:.1}", m.score));
                            ui.label(format!("{:.0} %", m.in10_1s * 100.0));
                            ui.label(format!("{:.1} mm/s", m.speed_1s));
                            ui.label(format!("{:.1} mm/s", m.speed_250ms));
                        }
                        None => {
                            ui.label("no trace");
                            ui.label("");
                            ui.label("");
                            ui.label("");
                        }
                    }
                    ui.end_row();
                }
            });
        });
    }

    fn export_csv(&mut self) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = format!("wayofaiming-{secs}.csv");
        let msg = match std::fs::write(&path, self.session.shots_csv()) {
            Ok(()) => format!("Saved {path}"),
            Err(e) => format!("Saving {path} failed: {e}"),
        };
        self.status.push(msg);
    }

    fn target_view(&self, ui: &mut egui::Ui) {
        let side = ui.available_width().min(ui.available_height());
        let (resp, painter) = ui.allocate_painter(Vec2::splat(side), Sense::hover());
        let rect = resp.rect;
        let spec = &self.session.spec;
        let centre = rect.center();
        let px_per_mm = (side / 2.0) / (spec.ring_radius(1) as f32 * 1.05) * self.zoom;
        let to_screen = |p: [f64; 2]| Pos2::new(centre.x + p[0] as f32 * px_per_mm, centre.y - p[1] as f32 * px_per_mm);
        let painter = painter.with_clip_rect(rect);

        // Target face.
        painter.rect_filled(rect, 0.0, Color32::from_rgb(242, 238, 228));
        painter.circle_filled(centre, spec.black_radius as f32 * px_per_mm, Color32::from_gray(25));
        for n in 1..=10u8 {
            let r = spec.ring_radius(n) as f32 * px_per_mm;
            let col = if n >= spec.first_ring_in_black { Color32::from_gray(230) } else { Color32::from_gray(40) };
            if n == 10 && spec.ring10_radius < 0.5 {
                painter.circle_filled(centre, r.max(1.5), col);
            } else {
                painter.circle_stroke(centre, r, Stroke::new(1.0, col));
            }
        }

        // Shot holes, then the selected shot's trace on top of them.
        for (i, shot) in self.session.shots().iter().enumerate() {
            let Some(m) = self.session.metrics(shot) else { continue };
            let p = to_screen(m.pos);
            let r = (spec.calibre as f32 / 2.0) * px_per_mm;
            let selected = self.selected == Some(i);
            let fill = if selected { Color32::from_rgba_unmultiplied(220, 40, 40, 200) } else { Color32::from_rgba_unmultiplied(80, 120, 220, 150) };
            painter.circle_filled(p, r, fill);
            painter.circle_stroke(p, r, Stroke::new(1.0, Color32::WHITE));
            painter.text(p, egui::Align2::CENTER_CENTER, shot.number.to_string(), egui::FontId::proportional(r.clamp(8.0, 16.0)), Color32::WHITE);
        }

        if let Some(shot) = self.selected.and_then(|i| self.session.shots().get(i)) {
            self.draw_shot_trace(&painter, shot, &to_screen);
        }

        // Live aim.
        let live: Vec<AimSample> = self.session.live_trace(LIVE_TRACE).collect();
        draw_polyline(&painter, &live, &to_screen, |_| Color32::from_rgb(40, 200, 90));
        if let Some(p) = live.last().and_then(|s| s.pos) {
            let p = to_screen(p);
            painter.circle_stroke(p, 6.0, Stroke::new(2.0, Color32::from_rgb(40, 200, 90)));
        }

        if let Some(m) = self.selected.and_then(|i| self.session.shots().get(i)).and_then(|s| self.session.metrics(s)) {
            let text = format!(
                "{:.1}   in 10: {:.0} %   speed 1 s: {:.1} mm/s   250 ms: {:.1} mm/s   spread: {:.2} mm",
                m.score, m.in10_1s * 100.0, m.speed_1s, m.speed_250ms, m.spread_1s
            );
            let galley = painter.layout_no_wrap(text, egui::FontId::monospace(14.0), Color32::WHITE);
            let pos = rect.left_top() + Vec2::new(8.0, 8.0);
            painter.rect_filled(Rect::from_min_size(pos, galley.size()).expand(6.0), 4.0, Color32::from_black_alpha(200));
            painter.galley(pos, galley, Color32::WHITE);
        }
    }

    fn draw_shot_trace(&self, painter: &egui::Painter, shot: &Shot, to_screen: &dyn Fn([f64; 2]) -> Pos2) {
        let trace: Vec<AimSample> = shot.trace.iter().map(|s| self.session.calibrated(*s)).collect();
        // Colour by time relative to the shot.
        draw_polyline(painter, &trace, to_screen, |t| {
            let dt = t - shot.t;
            if dt > 0.0 {
                Color32::from_rgb(220, 50, 50) // follow-through
            } else if dt > -0.2 {
                Color32::from_rgb(60, 120, 240) // last 0.2 s
            } else if dt > -1.0 {
                Color32::from_rgb(240, 200, 40) // last second
            } else {
                Color32::from_rgb(60, 170, 80) // earlier
            }
        });
    }

    fn preview(&self, ui: &mut egui::Ui) {
        let Some(tex) = &self.preview else {
            ui.label("No camera image yet.");
            return;
        };
        let [w, h] = self.preview_size;
        let mark = self.last_mark;
        {
            let width = ui.available_width().min(640.0);
            let size = Vec2::new(width, width * h as f32 / w.max(1) as f32);
            let (resp, painter) = ui.allocate_painter(size, Sense::hover());
            let rect = resp.rect;
            painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            let scale = rect.width() / w.max(1) as f32;
            // Crosshair at the image centre = aim point.
            let c = rect.center();
            let red = Stroke::new(1.0, Color32::RED);
            painter.line_segment([c - Vec2::X * 10.0, c + Vec2::X * 10.0], red);
            painter.line_segment([c - Vec2::Y * 10.0, c + Vec2::Y * 10.0], red);
            if let Some(m) = mark {
                let mc = rect.min + Vec2::new(m.center[0] as f32, m.center[1] as f32) * scale;
                painter.circle_stroke(mc, m.radius as f32 * scale, Stroke::new(2.0, Color32::GREEN));
            }
        }
    }
}

fn draw_polyline(painter: &egui::Painter, trace: &[AimSample], to_screen: &dyn Fn([f64; 2]) -> Pos2, colour: impl Fn(f64) -> Color32) {
    for w in trace.windows(2) {
        if let (Some(a), Some(b)) = (w[0].pos, w[1].pos) {
            painter.line_segment([to_screen(a), to_screen(b)], Stroke::new(2.0, colour(w[1].t)));
        }
    }
}

#[derive(Default)]
struct FpsCounter {
    window_start: f64,
    frames: u32,
    fps: f64,
}

impl FpsCounter {
    fn tick(&mut self, t: f64) {
        self.frames += 1;
        if t - self.window_start >= 1.0 {
            self.fps = f64::from(self.frames) / (t - self.window_start);
            self.window_start = t;
            self.frames = 0;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        egui::Panel::left("side").default_size(340.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui));
        });
        egui::CentralPanel::default().show(ui, |ui| self.target_view(ui));
    }
}
