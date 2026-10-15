//! Persistent configuration: the sound library and user settings.
//!
//! Stored as JSON in `$XDG_CONFIG_HOME/pw-soundboard/config.json`.
//! Thumbnails are copied (and downscaled) into `$XDG_DATA_HOME/pw-soundboard/thumbnails/`
//! so the board keeps working even if the original image is moved or deleted.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const APP_DIR: &str = "pw-soundboard";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sound {
    pub id: u64,
    pub title: String,
    pub path: PathBuf,
    #[serde(default)]
    pub thumbnail: Option<PathBuf>,
    /// Per-sound volume, 0.0 ..= 2.0 (1.0 = unchanged).
    #[serde(default = "one")]
    pub volume: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Global soundboard volume applied to every sound, 0.0 ..= 2.0.
    pub master_volume: f32,
    /// Send sounds to the speaker/headphone sink (so you hear them).
    pub play_to_speakers: bool,
    /// Send sounds to the virtual microphone (so others hear them).
    pub play_to_mic: bool,
    /// Mix the real microphone into the virtual microphone.
    pub mic_passthrough: bool,
    pub mic_passthrough_volume: f32,
    /// `node.name` of the sink to play to. `None` = system default.
    pub speaker_target: Option<String>,
    /// `node.name` of the microphone to pass through. `None` = system default.
    pub mic_source: Option<String>,
    /// `node.name` of the virtual source sounds are mixed into (e.g. "easyeffects_source").
    /// `None` = our own "Soundboard Virtual Mic".
    pub mic_output: Option<String>,
    /// Allow several sounds to play at once. If false, a new sound stops the others.
    pub allow_overlap: bool,
    /// Clicking a sound that is already playing stops it (instead of restarting).
    pub click_playing_stops: bool,
    /// Edge length of a grid tile in logical pixels.
    pub tile_size: f32,
    /// Closing the window keeps the app running in the tray (if the desktop has one).
    pub close_to_tray: bool,
    /// Start without opening the window (only in the tray).
    pub start_hidden: bool,
}

fn one() -> f32 {
    1.0
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            master_volume: 0.8,
            play_to_speakers: true,
            play_to_mic: true,
            mic_passthrough: false,
            mic_passthrough_volume: 1.0,
            speaker_target: None,
            mic_source: None,
            mic_output: None,
            allow_overlap: true,
            click_playing_stops: true,
            tile_size: 128.0,
            close_to_tray: true,
            start_hidden: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub settings: Settings,
    pub sounds: Vec<Sound>,
    pub next_id: u64,
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
        .join("config.json")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join(APP_DIR)
}

pub fn thumbnail_dir() -> PathBuf {
    data_dir().join("thumbnails")
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    let max = cfg.sounds.iter().map(|s| s.id).max().unwrap_or(0);
                    cfg.next_id = cfg.next_id.max(max + 1);
                    cfg
                }
                Err(e) => {
                    eprintln!("config: could not parse {}: {e}; starting fresh", path.display());
                    // Keep the broken file around instead of silently overwriting it.
                    let _ = std::fs::rename(&path, path.with_extension("json.bak"));
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write atomically so a crash never leaves a half-written config.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn alloc_id(&mut self) -> u64 {
        self.next_id = self.next_id.max(1);
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// Default title derived from a file name: "air_horn-01.mp3" -> "air horn 01".
pub fn title_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().replace(['_', '-'], " ").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Sound".into())
}

/// Downscale `src` to at most 256x256 and store it as PNG in the thumbnail dir.
/// Returns the stored file's path.
pub fn import_thumbnail(sound_id: u64, src: &Path) -> anyhow::Result<PathBuf> {
    let img = image::open(src)?;
    let thumb = img.thumbnail(256, 256);
    let dir = thumbnail_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // Unique name per import so the texture cache never shows a stale image.
    let dest = dir.join(format!("{sound_id}-{stamp}.png"));
    thumb.save_with_format(&dest, image::ImageFormat::Png)?;
    Ok(dest)
}

/// Delete a thumbnail only if it lives in our own thumbnail directory.
pub fn remove_owned_thumbnail(path: &Path) {
    if path.starts_with(thumbnail_dir()) {
        let _ = std::fs::remove_file(path);
    }
}
