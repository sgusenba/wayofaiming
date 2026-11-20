//! egui front end: target with live and per-shot aim trace, shot list,
//! replay, saved sessions, device selection and settings.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2};
use wayofaiming_core::detect::MarkDetection;
use wayofaiming_core::session::{AimSample, Session, SessionConfig, Shot};
use wayofaiming_core::storage::{self, SessionRecord, SessionSummary, Settings};
use wayofaiming_core::target::TargetSpec;

use crate::audio::{self, SharedThreshold};
use crate::camera::{self, CameraHandle, Source};
use crate::clock;
use crate::Event;

/// Shots used by "Centre group" calibration.
const CALIBRATION_SHOTS: usize = 5;
/// Seconds of live trace drawn while aiming.
const LIVE_TRACE: f64 = 1.0;
const REPLAY_SPEEDS: [f64; 3] = [0.25, 0.5, 1.0];

/// Startup options that are not user settings.
pub struct AppOptions {
    pub data_dir: PathBuf,
    pub demo: bool,
    pub camera_format: Option<(u32, u32, u32)>,
}

/// A saved session opened for review.
struct Review {
    session: Session,
    started_unix: u64,
}

/// Animated playback of the selected shot's trace.
struct Replay {
    /// Clock time when playback started.
    started_at: f64,
    /// Trace time at which playback started.
    from: f64,
    speed: f64,
}

pub struct App {
    ctx: egui::Context,
    rx: Receiver<Event>,
    tx: Sender<Event>,
    settings: Settings,
    settings_path: PathBuf,
    sessions_dir: PathBuf,
    export_dir: PathBuf,
    demo: bool,
    camera_format: Option<(u32, u32, u32)>,

    camera: Option<CameraHandle>,
    mic: Option<cpal::Stream>,
    cameras: Vec<(u32, String)>,
    mics: Vec<String>,
    threshold: SharedThreshold,

    live: Session,
    live_started: u64,
    review: Option<Review>,
    saved: Vec<SessionSummary>,
    selected: Option<usize>,
    replay: Option<Replay>,
    replay_speed: f64,

    zoom: f32,
    show_preview: bool,
    preview: Option<TextureHandle>,
    preview_size: [usize; 2],
    last_mark: Option<MarkDetection>,
    audio_level: f32,
    status: Vec<String>,
    fps: FpsCounter,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn format_time(unix: u64) -> String {
    chrono::DateTime::from_timestamp(unix as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| unix.to_string())
}

impl App {
    pub fn new(ctx: egui::Context, options: AppOptions, settings: Settings) -> Self {
        let (tx, rx) = mpsc::channel();
        let spec = TargetSpec::by_name(&settings.target).unwrap_or_default();
        let mut live = Session::new(spec, SessionConfig { video_latency: settings.video_latency, ..Default::default() });
        live.offset = settings.offset;
        let sessions_dir = options.data_dir.join("sessions");
        let mut app = Self {
            ctx,
            rx,
            tx,
            threshold: SharedThreshold::new(settings.shot_threshold),
            settings_path: options.data_dir.join("settings.json"),
            export_dir: dirs::document_dir().unwrap_or_else(|| options.data_dir.clone()),
            saved: storage::list_sessions(&sessions_dir),
            sessions_dir,
            settings,
            demo: options.demo,
            camera_format: options.camera_format,
            camera: None,
            mic: None,
            cameras: camera::available(),
            mics: audio::available(),
            live,
            live_started: unix_now(),
            review: None,
            selected: None,
            replay: None,
            replay_speed: 0.5,
            zoom: 2.0,
            show_preview: true,
            preview: None,
            preview_size: [0, 0],
            last_mark: None,
            audio_level: 0.0,
            status: Vec::new(),
            fps: FpsCounter::default(),
        };
        app.restart_camera();
        app.restart_mic();
        app
    }

    // ----- devices -------------------------------------------------------

    fn restart_camera(&mut self) {
        self.camera = None; // stops and joins the old thread, freeing the device
        self.last_mark = None;
        let source = if self.demo {
            Source::Demo
        } else {
            Source::Camera { index: self.settings.camera, format: self.camera_format }
        };
        let ctx = self.ctx.clone();
        self.camera = Some(camera::spawn(source, self.tx.clone(), move || ctx.request_repaint()));
    }

    fn restart_mic(&mut self) {
        self.mic = None;
        let ctx = self.ctx.clone();
        match audio::start(self.settings.mic.as_deref(), self.threshold.clone(), self.tx.clone(), move || ctx.request_repaint()) {
            Ok(s) => self.mic = Some(s),
            Err(e) => self.log(format!("Microphone error: {e:#}")),
        }
    }

    // ----- state ---------------------------------------------------------

    fn log(&mut self, msg: impl Into<String>) {
        self.status.push(msg.into());
        if self.status.len() > 6 {
            self.status.remove(0);
        }
    }

    /// The session currently shown: a reviewed one, or the live one.
    fn view(&self) -> &Session {
        self.review.as_ref().map_or(&self.live, |r| &r.session)
    }

    fn save_settings(&mut self) {
        self.settings.shot_threshold = self.threshold.get();
        self.settings.video_latency = self.live.config.video_latency;
        self.settings.offset = self.live.offset;
        self.settings.target = self.live.spec.name.to_string();
        if let Err(e) = storage::save_settings(&self.settings_path, &self.settings) {
            self.log(format!("Saving settings failed: {e}"));
        }
    }

    /// Writes the live session to disk (after every shot, so nothing is lost).
    fn save_live(&mut self) {
        if self.live.shots().is_empty() {
            return;
        }
        let record = SessionRecord::from_session(&self.live, self.live_started);
        match storage::save_session(&self.sessions_dir, &record) {
            Ok(_) => self.saved = storage::list_sessions(&self.sessions_dir),
            Err(e) => self.log(format!("Saving session failed: {e}")),
        }
    }

    fn new_session(&mut self) {
        self.save_live();
        self.live.clear();
        self.live_started = unix_now();
        self.review = None;
        self.selected = None;
        self.replay = None;
    }

    fn open_saved(&mut self, summary: &SessionSummary) {
        match storage::load_session(&summary.path).and_then(SessionRecord::into_session) {
            Ok(session) => {
                self.selected = session.shots().len().checked_sub(1);
                self.review = Some(Review { session, started_unix: summary.started_unix });
                self.replay = None;
            }
            Err(e) => self.log(format!("Opening {} failed: {e}", summary.path.display())),
        }
    }

    fn back_to_live(&mut self) {
        self.review = None;
        self.replay = None;
        self.selected = self.live.shots().len().checked_sub(1);
    }

    fn drain_events(&mut self) {
        let shots_before = self.live.shots().len();
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Frame { t, aim, mark } => {
                    let r = self.live.spec.black_radius;
                    self.live.push_frame(t, aim.map(|a| [a[0] * r, a[1] * r]));
                    self.last_mark = mark;
                    self.fps.tick(t);
                }
                Event::Shot { t } => self.live.push_shot(t),
                Event::AudioLevel(l) => self.audio_level = self.audio_level.max(l),
                Event::Preview { width, height, gray } => {
                    let img = egui::ColorImage::from_gray([width, height], &gray);
                    self.preview_size = [width, height];
                    match &mut self.preview {
                        Some(tex) => tex.set(img, TextureOptions::LINEAR),
                        None => self.preview = Some(self.ctx.load_texture("camera", img, TextureOptions::LINEAR)),
                    }
                }
                Event::Status(s) => self.log(s),
            }
        }
        if self.live.shots().len() > shots_before {
            self.save_live();
            if self.review.is_none() {
                self.selected = Some(self.live.shots().len() - 1);
                self.replay = None;
            }
        }
    }

    // ----- replay --------------------------------------------------------

    fn selected_shot(&self) -> Option<&Shot> {
        self.selected.and_then(|i| self.view().shots().get(i))
    }

    fn start_replay(&mut self) {
        if let Some(from) = self.selected_shot().and_then(|s| s.trace.first()).map(|s| s.t) {
            self.replay = Some(Replay { started_at: clock::now(), from, speed: self.replay_speed });
        }
    }

    /// Current replay time on the trace clock; ends the replay when done.
    fn replay_time(&mut self) -> Option<f64> {
        let r = self.replay.as_ref()?;
        let t = r.from + (clock::now() - r.started_at) * r.speed;
        let end = self.selected_shot().and_then(|s| s.trace.last()).map_or(t, |s| s.t);
        if t > end {
            self.replay = None;
            return None;
        }
        self.ctx.request_repaint();
        Some(t)
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let n = self.view().shots().len();
        let (space, left, right) = ctx.input(|i| {
            (i.key_pressed(egui::Key::Space), i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight))
        });
        if n > 0 && (left || right) {
            let cur = self.selected.unwrap_or(n - 1);
            self.selected = Some(if left { cur.saturating_sub(1) } else { (cur + 1).min(n - 1) });
            self.replay = None;
        }
        if space {
            if self.replay.is_some() {
                self.replay = None;
            } else {
                self.start_replay();
            }
        }
    }

    // ----- side panel ----------------------------------------------------

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("wayofaiming");
        if let Some(r) = &self.review {
            let label = format!("Reviewing session from {}", format_time(r.started_unix));
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(240, 200, 40), label);
            });
            if ui.button("Back to live").clicked() {
                self.back_to_live();
            }
        }
        ui.separator();

        egui::CollapsingHeader::new("Camera & microphone").default_open(true).show(ui, |ui| self.devices_section(ui));
        egui::CollapsingHeader::new("Target & calibration").default_open(true).show(ui, |ui| self.target_section(ui));
        egui::CollapsingHeader::new("Shots").default_open(true).show(ui, |ui| self.shots_section(ui));
        egui::CollapsingHeader::new("Saved sessions").default_open(false).show(ui, |ui| self.saved_section(ui));

        ui.separator();
        for s in &self.status {
            ui.small(s);
        }
    }

    fn devices_section(&mut self, ui: &mut egui::Ui) {
        // Camera choice: a real camera index or the demo source.
        let cam_label = if self.demo {
            "Demo (no hardware)".to_string()
        } else {
            self.cameras
                .iter()
                .find(|(i, _)| *i == self.settings.camera)
                .map_or_else(|| format!("Camera {}", self.settings.camera), |(i, n)| format!("{i}: {n}"))
        };
        let (mut cam, mut demo) = (self.settings.camera, self.demo);
        ui.horizontal(|ui| {
            ui.label("Camera");
            egui::ComboBox::from_id_salt("camera").selected_text(cam_label).width(200.0).show_ui(ui, |ui| {
                for (i, name) in &self.cameras {
                    if ui.selectable_label(!demo && cam == *i, format!("{i}: {name}")).clicked() {
                        cam = *i;
                        demo = false;
                    }
                }
                if ui.selectable_label(demo, "Demo (no hardware)").clicked() {
                    demo = true;
                }
            });
        });
        if cam != self.settings.camera || demo != self.demo {
            self.settings.camera = cam;
            self.demo = demo;
            self.save_settings();
            self.restart_camera();
        }

        let mut mic = self.settings.mic.clone();
        ui.horizontal(|ui| {
            ui.label("Mic");
            let text = mic.clone().unwrap_or_else(|| "System default".into());
            egui::ComboBox::from_id_salt("mic").selected_text(text).width(200.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut mic, None, "System default");
                for name in &self.mics {
                    ui.selectable_value(&mut mic, Some(name.clone()), name);
                }
            });
        });
        if mic != self.settings.mic {
            self.settings.mic = mic;
            self.save_settings();
            self.restart_mic();
        }
        if ui.small_button("Rescan devices").clicked() {
            self.cameras = camera::available();
            self.mics = audio::available();
        }

        ui.label(format!(
            "{:.0} fps, {}",
            self.fps.fps,
            match &self.last_mark {
                Some(m) => format!("target found (r = {:.0} px)", m.radius),
                None => "target NOT found".into(),
            }
        ));
        ui.horizontal(|ui| {
            ui.label("Level");
            let thr = self.threshold.get();
            let col = if self.audio_level > thr { Color32::from_rgb(200, 60, 60) } else { Color32::from_rgb(60, 140, 60) };
            ui.add(egui::ProgressBar::new(self.audio_level.min(1.0)).desired_width(150.0).fill(col));
        });
        // Peak-hold meter that decays each frame.
        self.audio_level *= 0.9;

        let mut thr = self.threshold.get();
        let thr_changed = ui.add(egui::Slider::new(&mut thr, 0.02..=1.0).text("shot threshold")).changed();
        let mut latency_ms = self.live.config.video_latency * 1000.0;
        let lat_changed = ui.add(egui::Slider::new(&mut latency_ms, 0.0..=300.0).text("video latency ms")).changed();
        if thr_changed || lat_changed {
            self.threshold.set(thr);
            self.live.config.video_latency = latency_ms / 1000.0;
            self.save_settings();
        }

        ui.checkbox(&mut self.show_preview, "Camera preview");
        if self.show_preview {
            self.preview(ui);
        }
    }

    fn target_section(&mut self, ui: &mut egui::Ui) {
        let mut spec = self.live.spec.clone();
        ui.add_enabled_ui(self.review.is_none(), |ui| {
            egui::ComboBox::from_id_salt("target").selected_text(spec.name).show_ui(ui, |ui| {
                for s in TargetSpec::ALL {
                    let name = s.name;
                    ui.selectable_value(&mut spec, s, name);
                }
            });
        });
        if spec != self.live.spec {
            // Shots are stored in mm of the old target; keep them apart.
            self.new_session();
            self.live.spec = spec;
            self.save_settings();
        }
        ui.add(egui::Slider::new(&mut self.zoom, 1.0..=8.0).text("zoom"));

        let live = self.review.is_none();
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(live, |ui| {
                if ui.button("Centre group").on_hover_text("Calibrate: move the mean of the last 5 shots to the centre").clicked() {
                    if self.live.calibrate_to_group_centre(CALIBRATION_SHOTS) {
                        self.save_settings();
                        self.save_live();
                    } else {
                        self.log("Calibration needs shots with a position");
                    }
                }
                if ui.button("Reset calibration").clicked() {
                    self.live.offset = [0.0, 0.0];
                    self.save_settings();
                    self.save_live();
                }
            });
        });
        let o = self.view().offset;
        ui.label(format!("Calibration offset: {:+.1}, {:+.1} mm", o[0], o[1]));
    }

    fn shots_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let can_replay = self.selected_shot().is_some_and(|s| !s.trace.is_empty());
            let label = if self.replay.is_some() { "⏹ Stop" } else { "▶ Replay" };
            if ui.add_enabled(can_replay, egui::Button::new(label)).on_hover_text("Space").clicked() {
                if self.replay.is_some() {
                    self.replay = None;
                } else {
                    self.start_replay();
                }
            }
            egui::ComboBox::from_id_salt("speed")
                .selected_text(format!("{}×", self.replay_speed))
                .width(60.0)
                .show_ui(ui, |ui| {
                    for s in REPLAY_SPEEDS {
                        ui.selectable_value(&mut self.replay_speed, s, format!("{s}×"));
                    }
                });
            if ui.button("Export CSV").clicked() {
                self.export_csv();
            }
            if self.review.is_none() && ui.button("New session").on_hover_text("Saves this session and starts an empty one").clicked() {
                self.new_session();
            }
        });
        ui.small("← → select shot, Space replay");
        self.shot_table(ui);
    }

    fn shot_table(&mut self, ui: &mut egui::Ui) {
        let session = self.view();
        let shots = session.shots();
        if shots.is_empty() {
            ui.label("No shots yet.");
            return;
        }
        let rows: Vec<(usize, usize, Option<_>)> = shots.iter().enumerate().map(|(i, s)| (i, s.number, session.metrics(s))).collect();
        let total: f64 = rows.iter().filter_map(|r| r.2).map(|m| m.score).sum();
        ui.label(format!("{} shots, total {:.1}", rows.len(), total));
        let mut clicked = None;
        egui::ScrollArea::vertical().id_salt("shot_table").max_height(240.0).show(ui, |ui| {
            egui::Grid::new("shots").striped(true).show(ui, |ui| {
                for h in ["#", "score", "in 10 (1 s)", "speed 1 s", "speed 250 ms"] {
                    ui.strong(h);
                }
                ui.end_row();
                for (i, number, m) in rows.iter().rev() {
                    if ui.selectable_label(self.selected == Some(*i), number.to_string()).clicked() {
                        clicked = Some(*i);
                    }
                    match m {
                        Some(m) => {
                            ui.label(format!("{:.1}", m.score));
                            ui.label(format!("{:.0} %", m.in10_1s * 100.0));
                            ui.label(format!("{:.1} mm/s", m.speed_1s));
                            ui.label(format!("{:.1} mm/s", m.speed_250ms));
                        }
                        None => {
                            ui.label("no trace");
                            for _ in 0..3 {
                                ui.label("");
                            }
                        }
                    }
                    ui.end_row();
                }
            });
        });
        if clicked.is_some() {
            self.selected = clicked;
            self.replay = None;
        }
    }

    fn saved_section(&mut self, ui: &mut egui::Ui) {
        if ui.small_button("Refresh").clicked() {
            self.saved = storage::list_sessions(&self.sessions_dir);
        }
        if self.saved.is_empty() {
            ui.label("No saved sessions yet. Sessions are saved automatically after every shot.");
        }
        let mut open = None;
        egui::ScrollArea::vertical().id_salt("saved").max_height(220.0).show(ui, |ui| {
            egui::Grid::new("saved_grid").striped(true).show(ui, |ui| {
                for s in &self.saved {
                    let is_live = s.started_unix == self.live_started;
                    ui.label(format_time(s.started_unix));
                    ui.label(&s.target);
                    ui.label(format!("{} shots", s.shots));
                    ui.label(format!("{:.1}", s.total));
                    if is_live {
                        ui.label("(live)");
                    } else if ui.small_button("Open").clicked() {
                        open = Some(s.clone());
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(s) = open {
            self.open_saved(&s);
        }
        ui.small(format!("Stored in {}", self.sessions_dir.display()));
    }

    fn export_csv(&mut self) {
        let started = self.review.as_ref().map_or(self.live_started, |r| r.started_unix);
        let stamp = chrono::DateTime::from_timestamp(started as i64, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%Y%m%d-%H%M").to_string())
            .unwrap_or_else(|| started.to_string());
        let path = self.export_dir.join(format!("wayofaiming-{stamp}.csv"));
        let msg = match std::fs::write(&path, self.view().shots_csv()) {
            Ok(()) => format!("Saved {}", path.display()),
            Err(e) => format!("Saving {} failed: {e}", path.display()),
        };
        self.log(msg);
    }

    fn preview(&self, ui: &mut egui::Ui) {
        let Some(tex) = &self.preview else {
            ui.label("No camera image yet.");
            return;
        };
        let [w, h] = self.preview_size;
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
        if let Some(m) = self.last_mark {
            let mc = rect.min + Vec2::new(m.center[0] as f32, m.center[1] as f32) * scale;
            painter.circle_stroke(mc, m.radius as f32 * scale, Stroke::new(2.0, Color32::GREEN));
        }
    }

    // ----- target view ---------------------------------------------------

    fn target_view(&self, ui: &mut egui::Ui, replay_t: Option<f64>) {
        let session = self.view();
        let side = ui.available_width().min(ui.available_height());
        let (resp, painter) = ui.allocate_painter(Vec2::splat(side), Sense::hover());
        let rect = resp.rect;
        let spec = &session.spec;
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

        // Shot holes. While replaying, the replayed shot appears at its moment.
        for (i, shot) in session.shots().iter().enumerate() {
            let selected = self.selected == Some(i);
            if selected && replay_t.is_some_and(|t| t < shot.t) {
                continue;
            }
            let Some(m) = session.metrics(shot) else { continue };
            let p = to_screen(m.pos);
            let r = (spec.calibre as f32 / 2.0) * px_per_mm;
            let fill = if selected { Color32::from_rgba_unmultiplied(220, 40, 40, 200) } else { Color32::from_rgba_unmultiplied(80, 120, 220, 150) };
            painter.circle_filled(p, r, fill);
            painter.circle_stroke(p, r, Stroke::new(1.0, Color32::WHITE));
            painter.text(p, egui::Align2::CENTER_CENTER, shot.number.to_string(), egui::FontId::proportional(r.clamp(8.0, 16.0)), Color32::WHITE);
        }

        // Selected shot trace on top (cut at the replay time while replaying).
        if let Some(shot) = self.selected_shot() {
            let until = replay_t.unwrap_or(f64::INFINITY);
            let trace: Vec<AimSample> = shot.trace.iter().filter(|s| s.t <= until).map(|s| session.calibrated(*s)).collect();
            draw_polyline(&painter, &trace, &to_screen, |t| shot_colour(t - shot.t));
            if replay_t.is_some() {
                if let Some(p) = trace.iter().rev().find_map(|s| s.pos) {
                    painter.circle_stroke(to_screen(p), 7.0, Stroke::new(2.5, Color32::WHITE));
                }
            }
        }

        // Live aim.
        if self.review.is_none() && replay_t.is_none() {
            let live: Vec<AimSample> = self.live.live_trace(LIVE_TRACE).collect();
            draw_polyline(&painter, &live, &to_screen, |_| Color32::from_rgb(40, 200, 90));
            if let Some(p) = live.last().and_then(|s| s.pos) {
                painter.circle_stroke(to_screen(p), 6.0, Stroke::new(2.0, Color32::from_rgb(40, 200, 90)));
            }
        }

        if let Some(shot) = self.selected_shot() {
            let mut text = match session.metrics(shot) {
                Some(m) => format!(
                    "#{}  {:.1}   in 10: {:.0} %   speed 1 s: {:.1} mm/s   250 ms: {:.1} mm/s   spread: {:.2} mm",
                    shot.number, m.score, m.in10_1s * 100.0, m.speed_1s, m.speed_250ms, m.spread_1s
                ),
                None => format!("#{}  no aim trace at the shot", shot.number),
            };
            if let Some(t) = replay_t {
                text.push_str(&format!("\nreplay {:+.2} s", t - shot.t));
            }
            let galley = painter.layout_no_wrap(text, egui::FontId::monospace(14.0), Color32::WHITE);
            let pos = rect.left_top() + Vec2::new(8.0, 8.0);
            painter.rect_filled(Rect::from_min_size(pos, galley.size()).expand(6.0), 4.0, Color32::from_black_alpha(200));
            painter.galley(pos, galley, Color32::WHITE);
        }
    }
}

/// Trace colour by time relative to the shot.
fn shot_colour(dt: f64) -> Color32 {
    if dt > 0.0 {
        Color32::from_rgb(220, 50, 50) // follow-through
    } else if dt > -0.2 {
        Color32::from_rgb(60, 120, 240) // last 0.2 s
    } else if dt > -1.0 {
        Color32::from_rgb(240, 200, 40) // last second
    } else {
        Color32::from_rgb(60, 170, 80) // earlier
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
        self.drain_events();
        self.handle_keys(&ctx);
        let replay_t = self.replay_time();
        egui::Panel::left("side").default_size(380.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui));
        });
        egui::CentralPanel::default().show(ui, |ui| self.target_view(ui, replay_t));
    }

    fn on_exit(&mut self) {
        self.save_live();
        self.save_settings();
    }
}
