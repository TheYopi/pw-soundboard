//! Keyboard shortcuts that work while the soundboard isn't focused.
//!
//! Wayland doesn't let apps see keys sent to other windows, and the GlobalShortcuts portal isn't
//! available everywhere (not on niri, for one). So the app reads the keyboards itself from
//! `/dev/input/event*` (evdev), which works on every compositor. That needs read access to the
//! input devices: the user has to be in the `input` group.
//!
//! Keys are only observed, never grabbed: they still reach the focused app. Nothing is stored or
//! logged except the combinations the user binds.

use crossbeam_channel::{Receiver, Sender};
use evdev::{EventSummary, KeyCode};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A key combination, e.g. `Ctrl+Alt+F9`. `key` is an evdev key without the `KEY_` prefix.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Combo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
    pub key: String,
}

impl Combo {
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('+').collect();
        let (key, mods) = parts.split_last()?;
        let mut c = Combo::default();
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" => c.ctrl = true,
                "alt" => c.alt = true,
                "shift" => c.shift = true,
                "super" => c.super_key = true,
                _ => return None,
            }
        }
        KeyCode::from_str(&format!("KEY_{key}")).ok()?;
        c.key = key.to_string();
        Some(c)
    }

    fn has_modifier(&self) -> bool {
        self.ctrl || self.alt || self.shift || self.super_key
    }

    /// Keys that type text would fire while typing anywhere, so they need a modifier. Function
    /// keys, media keys and the like may be used on their own.
    pub fn is_valid(&self) -> bool {
        if self.has_modifier() {
            return true;
        }
        let k = self.key.as_str();
        let typing = k.len() == 1
            || matches!(
                k,
                "SPACE" | "ENTER" | "TAB" | "BACKSPACE" | "DELETE" | "ESC" | "UP" | "DOWN" | "LEFT"
                    | "RIGHT" | "HOME" | "END" | "PAGEUP" | "PAGEDOWN" | "MINUS" | "EQUAL" | "COMMA"
                    | "DOT" | "SLASH" | "SEMICOLON" | "APOSTROPHE" | "GRAVE" | "BACKSLASH"
                    | "LEFTBRACE" | "RIGHTBRACE"
            );
        !typing
    }
}

impl std::fmt::Display for Combo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (on, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.super_key, "Super"),
        ] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        f.write_str(&self.key)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Play(u64),
    StopAll,
}

/// What the listener reports while recording a new shortcut.
pub enum Recorded {
    Combo(Combo),
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Access {
    /// Still looking at the devices.
    Starting,
    /// Listening to this many keyboards.
    Ok(usize),
    /// Keyboards exist but can't be opened: not in the `input` group.
    NoPermission,
    /// In the `input` group, but this login session predates it.
    NeedsRelogin,
    NoKeyboards,
}

struct Shared {
    bindings: HashMap<Combo, Action>,
    recording: Option<Sender<Recorded>>,
    /// Keys currently held, across all keyboards.
    held: HashSet<KeyCode>,
    access: Access,
}

#[derive(Clone)]
pub struct Hotkeys {
    shared: Arc<Mutex<Shared>>,
}

impl Hotkeys {
    /// Start watching keyboards. `on_action` runs on a listener thread; `wake` asks the UI to
    /// redraw (recording results, access changes).
    pub fn start(
        on_action: impl Fn(Action) + Send + Sync + 'static,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            bindings: HashMap::new(),
            recording: None,
            held: HashSet::new(),
            access: Access::Starting,
        }));
        let hotkeys = Self { shared: shared.clone() };
        let on_action: Arc<dyn Fn(Action) + Send + Sync> = Arc::new(on_action);
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let _ = std::thread::Builder::new()
            .name("hotkeys-scan".into())
            .spawn(move || scan_loop(shared, on_action, wake));
        hotkeys
    }

    pub fn set_bindings(&self, bindings: HashMap<Combo, Action>) {
        self.shared.lock().unwrap().bindings = bindings;
    }

    pub fn access(&self) -> Access {
        self.shared.lock().unwrap().access.clone()
    }

    /// The next key combination pressed (on any keyboard) is sent to the returned receiver
    /// instead of triggering a binding.
    pub fn record(&self) -> Receiver<Recorded> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.shared.lock().unwrap().recording = Some(tx);
        rx
    }

    pub fn stop_recording(&self) {
        self.shared.lock().unwrap().recording = None;
    }
}

/// Opens keyboards as they appear (hot-plug) and keeps the access status current.
fn scan_loop(shared: Arc<Mutex<Shared>>, on_action: Arc<dyn Fn(Action) + Send + Sync>, wake: Arc<dyn Fn() + Send + Sync>) {
    let watched: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
    loop {
        let mut denied = false;
        let mut any_device = false;
        if let Ok(dir) = std::fs::read_dir("/dev/input") {
            for entry in dir.flatten() {
                let path = entry.path();
                if !path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event")) {
                    continue;
                }
                any_device = true;
                if watched.lock().unwrap().contains(&path) {
                    continue;
                }
                match evdev::Device::open(&path) {
                    Ok(dev) if is_keyboard(&dev) => {
                        watched.lock().unwrap().insert(path.clone());
                        let (shared, on_action, wake, watched) =
                            (shared.clone(), on_action.clone(), wake.clone(), watched.clone());
                        let _ = std::thread::Builder::new().name("hotkeys-dev".into()).spawn(move || {
                            read_device(dev, &shared, &on_action, &wake);
                            watched.lock().unwrap().remove(&path); // unplugged
                        });
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
                    Err(_) => {}
                }
            }
        }
        let count = watched.lock().unwrap().len();
        let access = if count > 0 {
            Access::Ok(count)
        } else if denied {
            if in_input_group_file() { Access::NeedsRelogin } else { Access::NoPermission }
        } else if any_device {
            Access::NoKeyboards
        } else {
            Access::NoPermission
        };
        let changed = {
            let mut s = shared.lock().unwrap();
            let changed = s.access != access;
            s.access = access;
            changed
        };
        if changed {
            wake();
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn is_keyboard(dev: &evdev::Device) -> bool {
    dev.supported_keys().is_some_and(|keys| {
        [KeyCode::KEY_A, KeyCode::KEY_ENTER, KeyCode::KEY_F1, KeyCode::KEY_LEFTCTRL]
            .iter()
            .any(|k| keys.contains(*k))
    })
}

fn modifier(key: KeyCode) -> bool {
    matches!(
        key,
        KeyCode::KEY_LEFTCTRL
            | KeyCode::KEY_RIGHTCTRL
            | KeyCode::KEY_LEFTALT
            | KeyCode::KEY_RIGHTALT
            | KeyCode::KEY_LEFTSHIFT
            | KeyCode::KEY_RIGHTSHIFT
            | KeyCode::KEY_LEFTMETA
            | KeyCode::KEY_RIGHTMETA
    )
}

fn read_device(
    mut dev: evdev::Device,
    shared: &Mutex<Shared>,
    on_action: &Arc<dyn Fn(Action) + Send + Sync>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    loop {
        let Ok(events) = dev.fetch_events() else { return };
        for ev in events {
            let EventSummary::Key(_, key, value) = ev.destructure() else { continue };
            let pressed = match value {
                1 => true,
                0 => false,
                _ => continue, // auto-repeat
            };
            let mut s = shared.lock().unwrap();
            if !pressed {
                s.held.remove(&key);
                continue;
            }
            s.held.insert(key);
            if modifier(key) || !format!("{key:?}").starts_with("KEY_") {
                continue; // mouse buttons etc.
            }
            let h = &s.held;
            let combo = Combo {
                ctrl: h.contains(&KeyCode::KEY_LEFTCTRL) || h.contains(&KeyCode::KEY_RIGHTCTRL),
                alt: h.contains(&KeyCode::KEY_LEFTALT) || h.contains(&KeyCode::KEY_RIGHTALT),
                shift: h.contains(&KeyCode::KEY_LEFTSHIFT) || h.contains(&KeyCode::KEY_RIGHTSHIFT),
                super_key: h.contains(&KeyCode::KEY_LEFTMETA) || h.contains(&KeyCode::KEY_RIGHTMETA),
                key: format!("{key:?}").trim_start_matches("KEY_").to_string(),
            };
            if let Some(tx) = s.recording.take() {
                let plain_esc = key == KeyCode::KEY_ESC && !combo.has_modifier();
                let _ = tx.send(if plain_esc { Recorded::Cancelled } else { Recorded::Combo(combo) });
                drop(s);
                wake();
                continue;
            }
            let action = s.bindings.get(&combo).copied();
            drop(s);
            if let Some(action) = action {
                on_action(action);
            }
        }
    }
}

/// Is the user listed in the `input` group in /etc/group (even if this session doesn't have it)?
fn in_input_group_file() -> bool {
    let Ok(user) = std::env::var("USER") else { return false };
    std::fs::read_to_string("/etc/group").is_ok_and(|text| {
        text.lines().any(|l| {
            let mut f = l.split(':');
            f.next() == Some("input") && f.nth(2).is_some_and(|m| m.split(',').any(|u| u == user))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_round_trip() {
        let c = Combo::parse("Ctrl+Alt+F9").unwrap();
        assert!(c.ctrl && c.alt && !c.shift && !c.super_key);
        assert_eq!(c.to_string(), "Ctrl+Alt+F9");
        assert_eq!(Combo::parse("Shift+Super+KP1").unwrap().to_string(), "Shift+Super+KP1");
        assert!(Combo::parse("Ctrl+NOTAKEY").is_none());
        assert!(Combo::parse("Hyper+A").is_none());
    }

    #[test]
    fn typing_keys_need_a_modifier() {
        assert!(!Combo::parse("A").unwrap().is_valid());
        assert!(!Combo::parse("SPACE").unwrap().is_valid());
        assert!(Combo::parse("F9").unwrap().is_valid());
        assert!(Combo::parse("Ctrl+A").unwrap().is_valid());
    }
}
