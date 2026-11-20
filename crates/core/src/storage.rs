//! Saving and loading sessions and settings as JSON files.
//!
//! Each session is one file `session-<unix seconds>.json` in a sessions
//! directory. It stores the uncalibrated shot traces plus the calibration
//! offset, so a saved session can be re-analysed and replayed exactly.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::{Session, SessionConfig, Shot};
use crate::target::TargetSpec;

/// Bumped when the file layout changes incompatibly.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug)]
pub enum StorageError {
    Io(io::Error),
    Json(serde_json::Error),
    UnknownTarget(String),
    UnsupportedVersion(u32),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "invalid session file: {e}"),
            Self::UnknownTarget(t) => write!(f, "unknown target {t:?}"),
            Self::UnsupportedVersion(v) => write!(f, "session file version {v} is newer than this program supports"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for StorageError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// On-disk form of a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub format_version: u32,
    /// When the session started, seconds since the Unix epoch.
    pub started_unix: u64,
    /// [`TargetSpec::name`].
    pub target: String,
    /// Calibration offset (mm).
    pub offset: [f64; 2],
    /// Video latency used when recording (s).
    pub video_latency: f64,
    pub shots: Vec<Shot>,
}

impl SessionRecord {
    pub fn from_session(session: &Session, started_unix: u64) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            started_unix,
            target: session.spec.name.to_string(),
            offset: session.offset,
            video_latency: session.config.video_latency,
            shots: session.shots().to_vec(),
        }
    }

    pub fn into_session(self) -> Result<Session, StorageError> {
        let spec = TargetSpec::by_name(&self.target).ok_or(StorageError::UnknownTarget(self.target))?;
        let config = SessionConfig { video_latency: self.video_latency, ..Default::default() };
        Ok(Session::from_shots(spec, config, self.offset, self.shots))
    }

    pub fn file_name(&self) -> String {
        format!("session-{}.json", self.started_unix)
    }
}

/// Short description of a saved session for listing.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub started_unix: u64,
    pub target: String,
    pub shots: usize,
    /// Sum of decimal scores of shots with a position.
    pub total: f64,
}

/// Writes the session into `dir` (created if needed) and returns the file path.
/// The file is written to a temporary name first, so a crash never leaves a
/// half-written session behind.
pub fn save_session(dir: &Path, record: &SessionRecord) -> Result<PathBuf, StorageError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(record.file_name());
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(record)?)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn load_session(path: &Path) -> Result<SessionRecord, StorageError> {
    let record: SessionRecord = serde_json::from_slice(&fs::read(path)?)?;
    if record.format_version > FORMAT_VERSION {
        return Err(StorageError::UnsupportedVersion(record.format_version));
    }
    Ok(record)
}

/// Saved sessions in `dir`, newest first. Unreadable files are skipped.
pub fn list_sessions(dir: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<SessionSummary> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|path| {
            let record = load_session(&path).ok()?;
            let (started_unix, target) = (record.started_unix, record.target.clone());
            let session = record.into_session().ok()?;
            let total = session.shots().iter().filter_map(|s| session.metrics(s)).map(|m| m.score).sum();
            Some(SessionSummary { path, started_unix, target, shots: session.shots().len(), total })
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.started_unix));
    out
}

/// User settings kept between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub target: String,
    pub camera: u32,
    /// Microphone name; `None` uses the system default.
    pub mic: Option<String>,
    pub shot_threshold: f32,
    /// Seconds.
    pub video_latency: f64,
    /// Calibration offset (mm).
    pub offset: [f64; 2],
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target: TargetSpec::AIR_RIFLE_10M.name.to_string(),
            camera: 0,
            mic: None,
            shot_threshold: 0.25,
            video_latency: 0.05,
            offset: [0.0, 0.0],
        }
    }
}

/// Loads settings; a missing or broken file gives the defaults.
pub fn load_settings(path: &Path) -> Settings {
    fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_settings(path: &Path, settings: &Settings) -> Result<(), StorageError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_vec_pretty(settings)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wayofaiming-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn session_with_shot(x: f64) -> Session {
        let mut s = Session::new(TargetSpec::AIR_PISTOL_10M, SessionConfig { video_latency: 0.0, ..Default::default() });
        for i in 0..200 {
            s.push_frame(i as f64 * 0.01, Some([x, 1.0]));
        }
        s.push_shot(1.0);
        s.offset = [0.5, -0.5];
        assert_eq!(s.shots().len(), 1);
        s
    }

    #[test]
    fn session_round_trip_keeps_shots_and_metrics() {
        let dir = temp_dir("roundtrip");
        let session = session_with_shot(3.0);
        let path = save_session(&dir, &SessionRecord::from_session(&session, 1_700_000_000)).unwrap();
        assert!(path.ends_with("session-1700000000.json"));

        let loaded = load_session(&path).unwrap().into_session().unwrap();
        assert_eq!(loaded.spec, TargetSpec::AIR_PISTOL_10M);
        assert_eq!(loaded.offset, [0.5, -0.5]);
        assert_eq!(loaded.shots(), session.shots());
        assert_eq!(loaded.metrics(&loaded.shots()[0]), session.metrics(&session.shots()[0]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_is_newest_first_and_skips_junk() {
        let dir = temp_dir("list");
        save_session(&dir, &SessionRecord::from_session(&session_with_shot(0.0), 100)).unwrap();
        save_session(&dir, &SessionRecord::from_session(&session_with_shot(50.0), 200)).unwrap();
        fs::write(dir.join("broken.json"), b"{not json").unwrap();

        let list = list_sessions(&dir);
        assert_eq!(list.iter().map(|s| s.started_unix).collect::<Vec<_>>(), vec![200, 100]);
        assert_eq!(list[1].shots, 1);
        assert_eq!(list[1].total, 10.9);
        assert_eq!(list[0].total, 4.6); // 50.5 mm out on the pistol target
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_format_is_rejected() {
        let dir = temp_dir("version");
        let mut rec = SessionRecord::from_session(&session_with_shot(0.0), 1);
        rec.format_version = FORMAT_VERSION + 1;
        let path = save_session(&dir, &rec).unwrap();
        assert!(matches!(load_session(&path), Err(StorageError::UnsupportedVersion(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_default_when_missing_and_round_trip() {
        let dir = temp_dir("settings");
        let path = dir.join("settings.json");
        assert_eq!(load_settings(&path), Settings::default());
        let s = Settings { camera: 2, shot_threshold: 0.4, offset: [1.0, 2.0], ..Default::default() };
        save_settings(&path, &s).unwrap();
        assert_eq!(load_settings(&path), s);
        // Unknown/missing fields fall back to defaults.
        fs::write(&path, br#"{"camera": 3}"#).unwrap();
        assert_eq!(load_settings(&path), Settings { camera: 3, ..Default::default() });
        let _ = fs::remove_dir_all(&dir);
    }
}
