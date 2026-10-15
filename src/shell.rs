//! Background mode: the tray icon, single-instance handoff and window lifecycle.
//!
//! The app keeps running without a window. Wayland has no "hide window", so closing the window
//! really closes it (eframe returns from `run_native`) and showing it again opens a new one; the
//! app state, audio engine and control socket live on in between.

use crossbeam_channel::{Receiver, Sender};
use ksni::blocking::TrayMethods;
use std::sync::{Arc, LazyLock, Mutex};

pub enum ShellEvent {
    Show,
    StopAll,
    Quit,
}

/// Wakes the UI of whichever window is currently open (if any).
#[derive(Clone, Default)]
pub struct Wake(Arc<Mutex<Option<egui::Context>>>);

impl Wake {
    pub fn set(&self, ctx: Option<egui::Context>) {
        *self.0.lock().unwrap() = ctx;
    }

    pub fn wake(&self) {
        if let Some(ctx) = &*self.0.lock().unwrap() {
            ctx.request_repaint();
        }
    }
}

/// Sends events to the app from other threads (tray, control socket).
#[derive(Clone)]
pub struct Shell {
    tx: Sender<ShellEvent>,
    pub wake: Wake,
}

impl Shell {
    pub fn new() -> (Self, Receiver<ShellEvent>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (Self { tx, wake: Wake::default() }, rx)
    }

    pub fn send(&self, ev: ShellEvent) {
        let _ = self.tx.send(ev);
        self.wake.wake();
    }
}

struct Tray {
    shell: Shell,
}

static TRAY_ICONS: LazyLock<Vec<ksni::Icon>> = LazyLock::new(|| {
    let Ok(img) = image::load_from_memory(include_bytes!("../assets/tray-icon.png")) else {
        return Vec::new();
    };
    [22, 32, 48, 64]
        .into_iter()
        .map(|size| {
            let mut data = img
                .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
                .into_rgba8()
                .into_vec();
            for px in data.as_chunks_mut::<4>().0 {
                px.rotate_right(1); // RGBA -> ARGB
            }
            ksni::Icon {
                width: size as i32,
                height: size as i32,
                data,
            }
        })
        .collect()
});

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "pw-soundboard".into()
    }

    fn title(&self) -> String {
        "PW Soundboard".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        TRAY_ICONS.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "PW Soundboard".into(),
            description: "Running in the background".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.shell.send(ShellEvent::Show);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Show Soundboard".into(),
                activate: Box::new(|t: &mut Self| t.shell.send(ShellEvent::Show)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Stop all sounds".into(),
                icon_name: "media-playback-stop".into(),
                activate: Box::new(|t: &mut Self| t.shell.send(ShellEvent::StopAll)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| t.shell.send(ShellEvent::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Show the tray icon. `None` if the desktop has no tray (StatusNotifierWatcher); then closing
/// the window quits the app as before.
pub fn spawn_tray(shell: Shell) -> Option<ksni::blocking::Handle<impl ksni::Tray>> {
    match (Tray { shell }).spawn() {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("tray: not available ({e}); closing the window will quit");
            None
        }
    }
}
