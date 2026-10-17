//! Local control socket, so other programs (e.g. the StreamController plugin) can list and
//! trigger sounds.
//!
//! A Unix socket at `$XDG_DATA_HOME/pw-soundboard/control.sock` (mode 0600) speaking one JSON
//! object per line. It lives in the data dir rather than `$XDG_RUNTIME_DIR` because Flatpak apps
//! with home access (like StreamController) can reach it there.
//!
//! Requests and replies:
//! * `{"cmd":"list"}` → `{"ok":true,"version":N,"sounds":[{"id","title","thumbnail","state"}]}`
//! * `{"cmd":"press","id":N}` → behaves exactly like clicking the tile
//! * `{"cmd":"stop","id":N}`, `{"cmd":"stop_all"}`
//!   (`press` and `stop` also accept `"title":"…"` instead of an id)
//! * `{"cmd":"show"}` → opens the window (used when the app is launched a second time)
//! * `{"cmd":"state"}` → `{"ok":true,"version":N,"playing":{"<id>":progress}}`
//! * `{"cmd":"subscribe"}` → the connection then receives a `state` line whenever playback or the
//!   library changes (plus a `ping` every few seconds to detect closed clients).
//!
//! Requests are served on their own threads, not the UI thread, so they work while the window is
//! hidden or on another workspace (where the UI is not redrawn).

use crate::audio::{Cmd, SharedStatus};
use crate::config;
use crate::shell::{Shell, ShellEvent};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A sound as seen by remote clients. Published by the UI whenever the library changes.
#[derive(Clone)]
pub struct RemoteSound {
    pub id: u64,
    pub title: String,
    pub thumbnail: Option<PathBuf>,
    pub volume: f32,
    /// Decoded samples, once ready.
    pub clip: Option<Arc<Vec<f32>>>,
    /// "ready", "loading" or "failed".
    pub state: &'static str,
}

impl PartialEq for RemoteSound {
    fn eq(&self, o: &Self) -> bool {
        let same_clip = match (&self.clip, &o.clip) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        self.id == o.id
            && self.title == o.title
            && self.thumbnail == o.thumbnail
            && self.volume == o.volume
            && self.state == o.state
            && same_clip
    }
}

/// The settings that decide what pressing a sound does.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct PlaySettings {
    pub to_speakers: bool,
    pub to_mic: bool,
    pub allow_overlap: bool,
    pub click_playing_stops: bool,
}

impl From<&config::Settings> for PlaySettings {
    fn from(s: &config::Settings) -> Self {
        Self {
            to_speakers: s.play_to_speakers,
            to_mic: s.play_to_mic,
            allow_overlap: s.allow_overlap,
            click_playing_stops: s.click_playing_stops,
        }
    }
}

#[derive(Default)]
pub struct Library {
    pub sounds: Vec<RemoteSound>,
    pub settings: PlaySettings,
    /// Bumped on every change so clients know when to re-list.
    pub version: u64,
}

pub type SharedLibrary = Arc<Mutex<Library>>;

pub enum Press {
    Played,
    Stopped,
    OutputsOff,
    NotReady,
}

/// What pressing a sound does. Shared by tile clicks and remote presses so they behave the same.
pub fn press(
    tx: &pipewire::channel::Sender<Cmd>,
    status: &SharedStatus,
    settings: PlaySettings,
    id: u64,
    gain: f32,
    clip: Option<Arc<Vec<f32>>>,
) -> Press {
    let playing = status.lock().unwrap().progress(id).is_some();
    if playing && settings.click_playing_stops {
        let _ = tx.send(Cmd::Stop(id));
        return Press::Stopped;
    }
    let Some(samples) = clip else {
        return Press::NotReady;
    };
    if !settings.to_speakers && !settings.to_mic {
        return Press::OutputsOff;
    }
    let _ = tx.send(Cmd::Play {
        id,
        samples,
        gain,
        to_speakers: settings.to_speakers,
        to_mic: settings.to_mic,
        exclusive: !settings.allow_overlap,
    });
    Press::Played
}

/// Press a sound by id using the published library (for callers outside the UI thread).
pub fn press_id(
    library: &SharedLibrary,
    status: &SharedStatus,
    tx: &pipewire::channel::Sender<Cmd>,
    id: u64,
) -> Result<Press, &'static str> {
    let (sound, settings) = {
        let lib = library.lock().unwrap();
        (lib.sounds.iter().find(|s| s.id == id).cloned(), lib.settings)
    };
    let sound = sound.ok_or("unknown sound")?;
    Ok(press(tx, status, settings, id, sound.volume, sound.clip))
}

pub fn socket_path() -> PathBuf {
    config::data_dir().join("control.sock")
}

/// Removes the socket file when the app exits.
pub struct Server {
    path: Option<PathBuf>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(p) = &self.path {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[derive(Clone)]
struct Ctx {
    library: SharedLibrary,
    status: SharedStatus,
    tx: pipewire::channel::Sender<Cmd>,
    shell: Shell,
}

const CLI_HELP: &str = "\
Usage: pw-soundboard [OPTION]

Without options the soundboard window opens (or the running one is shown).

  --play SOUND    play SOUND (its number or exact title), like clicking its tile
  --stop SOUND    stop SOUND
  --stop-all      stop all sounds
  --hidden        start in the system tray without opening the window
  --help          show this help

--play/--stop/--stop-all talk to the running soundboard; bind them to keys in your desktop's
keyboard settings for global shortcuts.";

/// Handle command-line actions that are sent to a running instance. Returns the exit code, or
/// None to start the app normally.
pub fn run_cli(args: &[String]) -> Option<i32> {
    let target = |s: &str| match s.parse::<u64>() {
        Ok(id) => json!({"id": id}),
        Err(_) => json!({"title": s}),
    };
    let mut req = match args.first().map(String::as_str) {
        Some("--help" | "-h") => {
            println!("{CLI_HELP}");
            return Some(0);
        }
        Some("--stop-all") => json!({"cmd": "stop_all"}),
        Some(flag @ ("--play" | "--stop")) => {
            let Some(sound) = args.get(1) else {
                eprintln!("{flag} needs a sound number or title");
                return Some(2);
            };
            target(sound)
        }
        _ => return None,
    };
    if req.get("cmd").is_none() {
        req["cmd"] = json!(if args[0] == "--play" { "press" } else { "stop" });
    }
    let reply = UnixStream::connect(socket_path()).and_then(|mut conn| {
        conn.set_read_timeout(Some(Duration::from_secs(2)))?;
        send(&mut conn, &req)?;
        let mut line = String::new();
        BufReader::new(conn).read_line(&mut line)?;
        Ok(line)
    });
    match reply {
        Err(_) => {
            eprintln!("PW Soundboard is not running");
            Some(1)
        }
        Ok(line) => match serde_json::from_str::<Value>(&line) {
            Ok(v) if v["ok"] == json!(true) => Some(0),
            Ok(v) => {
                eprintln!("{}", v["error"].as_str().unwrap_or("failed"));
                Some(1)
            }
            Err(_) => Some(1),
        },
    }
}

/// If another instance is running, ask it to show its window. Returns true if it did.
pub fn show_existing_instance() -> bool {
    let Ok(mut conn) = UnixStream::connect(socket_path()) else { return false };
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    if send(&mut conn, &json!({"cmd": "show"})).is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(conn).read_line(&mut reply).is_ok() && reply.contains("\"ok\":true")
}

pub fn start(
    library: SharedLibrary,
    status: SharedStatus,
    tx: pipewire::channel::Sender<Cmd>,
    shell: Shell,
) -> Server {
    let path = socket_path();
    match bind(&path) {
        Ok(listener) => {
            let ctx = Ctx { library, status, tx, shell };
            let spawned = std::thread::Builder::new().name("remote".into()).spawn(move || {
                for conn in listener.incoming().flatten() {
                    let ctx = ctx.clone();
                    let _ = std::thread::Builder::new()
                        .name("remote-client".into())
                        .spawn(move || serve(conn, ctx));
                }
            });
            match spawned {
                Ok(_) => Server { path: Some(path) },
                Err(e) => {
                    eprintln!("remote: could not start: {e}");
                    Server { path: Some(path) }
                }
            }
        }
        Err(e) => {
            eprintln!("remote: control socket disabled: {e}");
            Server { path: None }
        }
    }
}

fn bind(path: &PathBuf) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        // Another instance still answering? Then leave its socket alone.
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "another PW Soundboard instance owns the control socket",
            ));
        }
        std::fs::remove_file(path)?; // stale, from a crash
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn serve(conn: UnixStream, ctx: Ctx) {
    let Ok(write_half) = conn.try_clone() else { return };
    let mut out = write_half;
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                let _ = send(&mut out, &json!({"ok": false, "error": "invalid JSON"}));
                return; // probably not one of our clients
            }
        };
        let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
        if cmd == "subscribe" {
            subscribe(out, &ctx);
            return;
        }
        let reply = handle(cmd, &req, &ctx);
        if send(&mut out, &reply).is_err() {
            return;
        }
    }
}

fn send(out: &mut UnixStream, v: &Value) -> std::io::Result<()> {
    let mut s = v.to_string();
    s.push('\n');
    out.write_all(s.as_bytes())
}

fn handle(cmd: &str, req: &Value, ctx: &Ctx) -> Value {
    let id = req.get("id").and_then(Value::as_u64).or_else(|| {
        let title = req.get("title")?.as_str()?;
        let lib = ctx.library.lock().unwrap();
        lib.sounds
            .iter()
            .find(|s| s.title.eq_ignore_ascii_case(title.trim()))
            .map(|s| s.id)
    });
    match (cmd, id) {
        ("list", _) => {
            let lib = ctx.library.lock().unwrap();
            let sounds: Vec<Value> = lib
                .sounds
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "title": s.title,
                        "thumbnail": s.thumbnail.as_ref().map(|p| p.to_string_lossy()),
                        "state": s.state,
                    })
                })
                .collect();
            json!({"ok": true, "version": lib.version, "sounds": sounds})
        }
        ("press", Some(id)) => match press_id(&ctx.library, &ctx.status, &ctx.tx, id) {
            Ok(Press::Played) => json!({"ok": true, "result": "played"}),
            Ok(Press::Stopped) => json!({"ok": true, "result": "stopped"}),
            Ok(Press::OutputsOff) => json!({"ok": false, "error": "both outputs are off"}),
            Ok(Press::NotReady) => json!({"ok": false, "error": "sound is not ready"}),
            Err(e) => json!({"ok": false, "error": e}),
        },
        ("stop", Some(id)) => {
            let _ = ctx.tx.send(Cmd::Stop(id));
            json!({"ok": true})
        }
        ("stop_all", _) => {
            let _ = ctx.tx.send(Cmd::StopAll);
            json!({"ok": true})
        }
        ("show", _) => {
            ctx.shell.send(ShellEvent::Show);
            json!({"ok": true})
        }
        ("state", _) => state(ctx),
        _ => json!({"ok": false, "error": "unknown command"}),
    }
}

fn state(ctx: &Ctx) -> Value {
    let version = ctx.library.lock().unwrap().version;
    let playing: HashMap<String, f32> = {
        let s = ctx.status.lock().unwrap();
        s.playing_speakers
            .keys()
            .chain(s.playing_mic.keys())
            .filter_map(|&id| s.progress(id).map(|p| (id.to_string(), (p * 100.0).round() / 100.0)))
            .collect()
    };
    json!({"ok": true, "version": version, "playing": playing})
}

fn subscribe(mut out: UnixStream, ctx: &Ctx) {
    let mut last = Value::Null;
    let mut last_sent = Instant::now();
    loop {
        let mut st = state(ctx);
        st["event"] = json!("state");
        let result = if st != last {
            last = st.clone();
            last_sent = Instant::now();
            send(&mut out, &st)
        } else if last_sent.elapsed() > Duration::from_secs(3) {
            last_sent = Instant::now();
            send(&mut out, &json!({"event": "ping"}))
        } else {
            Ok(())
        };
        if result.is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
