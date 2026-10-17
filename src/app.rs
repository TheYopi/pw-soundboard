//! egui user interface: the sound grid, the editor panel and routing settings.

use crate::audio::{self, AudioHandle, Cmd, Device};
use crate::config::{self, Config, Sound};
use crate::decode::{self, Clip, DecodeResult};
use crate::hotkeys::{Access, Action, Combo, Hotkeys, Recorded};
use crate::remote::{self, Press, RemoteSound, SharedLibrary};
use crate::shell::{Shell, ShellEvent};
use crate::theme::{self, Palette, ThemeWatcher};
use crossbeam_channel::{Receiver, Sender};
use egui::{Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

enum ClipState {
    Loading,
    Ready(Clip),
    Failed(String),
}

/// Results coming back from file-dialog threads.
enum Picked {
    NewSounds(Vec<PathBuf>),
    ReplaceAudio(u64, PathBuf),
    Thumbnail(u64, PathBuf),
}

pub struct SoundboardApp {
    cfg: Config,
    audio: AudioHandle,

    clips: HashMap<u64, ClipState>,
    decode_tx: Sender<(u64, PathBuf)>,
    decode_rx: Receiver<DecodeResult>,

    textures: HashMap<PathBuf, Option<egui::TextureHandle>>,

    pick_tx: Sender<Picked>,
    pick_rx: Receiver<Picked>,
    /// When the current file dialog was opened. A portal request can get lost (dialog never shown
    /// or never answered), so this only blocks new dialogs briefly instead of forever.
    dialog_open: Option<Instant>,

    selected: Option<u64>,
    search: String,
    show_settings: bool,
    pending_delete: Option<u64>,

    dirty: bool,
    last_change: Instant,
    toast: Option<(String, Instant)>,

    /// Sound list shared with the control socket (StreamController plugin etc.).
    library: SharedLibrary,
    _remote: remote::Server,

    shell: Shell,
    events: Receiver<ShellEvent>,
    tray_available: bool,
    quit_requested: bool,

    hotkeys: Hotkeys,
    /// Waiting for the user to press a combination for this target.
    recording: Option<(ShortcutTarget, Receiver<Recorded>)>,
    shortcut_error: Option<(ShortcutTarget, String)>,

    theme: ThemeWatcher,
    logo: Option<egui::TextureHandle>,
}

#[derive(Clone, Copy, PartialEq)]
enum ShortcutTarget {
    Sound(u64),
    StopAll,
}

impl SoundboardApp {
    pub fn new(shell: Shell, events: Receiver<ShellEvent>) -> Self {
        let cfg = Config::load();
        let theme = ThemeWatcher::new(&cfg.settings.theme);

        let wake = shell.wake.clone();
        let s = &cfg.settings;
        let audio = audio::start(
            audio::InitialRouting {
                master: s.master_volume,
                speaker_target: s.speaker_target.clone(),
                mic_source: s.mic_source.clone(),
                passthrough: s.mic_passthrough,
                passthrough_gain: s.mic_passthrough_volume,
                mic_output: s.mic_output.clone(),
            },
            move || wake.wake(),
        );

        let wake = shell.wake.clone();
        let (decode_tx, decode_rx) = decode::spawn_worker(move || wake.wake());
        let (pick_tx, pick_rx) = crossbeam_channel::unbounded();
        let library = SharedLibrary::default();
        let remote = remote::start(library.clone(), audio.status.clone(), audio.sender(), shell.clone());
        // Shortcuts fire on a listener thread, so they work with the window hidden or closed.
        let hotkeys = {
            let (library, status, tx) = (library.clone(), audio.status.clone(), audio.sender());
            let wake = shell.wake.clone();
            Hotkeys::start(
                move |action| match action {
                    Action::Play(id) => {
                        let _ = remote::press_id(&library, &status, &tx, id);
                    }
                    Action::StopAll => {
                        let _ = tx.send(Cmd::StopAll);
                    }
                },
                move || wake.wake(),
            )
        };

        let mut app = Self {
            cfg,
            audio,
            clips: HashMap::new(),
            decode_tx,
            decode_rx,
            textures: HashMap::new(),
            pick_tx,
            pick_rx,
            dialog_open: None,
            selected: None,
            search: String::new(),
            show_settings: false,
            pending_delete: None,
            dirty: false,
            last_change: Instant::now(),
            toast: None,
            library,
            _remote: remote,
            shell,
            events,
            tray_available: false,
            quit_requested: false,
            hotkeys,
            recording: None,
            shortcut_error: None,
            theme,
            logo: None,
        };
        app.sync_hotkeys();
        for s in app.cfg.sounds.clone() {
            app.request_decode(s.id, s.path);
        }
        app
    }

    // ---------------------------------------------------------------- window lifecycle

    pub fn set_tray_available(&mut self, available: bool) {
        self.tray_available = available;
    }

    /// Whether to start without opening the window.
    pub fn start_hidden(&self) -> bool {
        self.tray_available && self.cfg.settings.start_hidden
    }

    /// A new window (and egui context) was created.
    pub fn attach(&mut self, ctx: &egui::Context) {
        // Textures belong to the previous window's context.
        self.textures.clear();
        self.logo = None;
        theme::install_fonts(ctx);
        self.restyle(ctx);
        self.shell.wake.set(Some(ctx.clone()));
    }

    /// The window was closed. Returns true to keep running in the tray.
    pub fn detach(&mut self) -> bool {
        self.shell.wake.set(None);
        self.dialog_open = None;
        self.save_now();
        !self.quit_requested && self.tray_available && self.cfg.settings.close_to_tray
    }

    /// Run without a window until the tray (or a second launch) asks to show it.
    /// Returns false when the app should quit instead.
    pub fn run_hidden(&mut self) -> bool {
        loop {
            match self.events.recv_timeout(Duration::from_millis(200)) {
                Ok(ShellEvent::Show) => return true,
                Ok(ShellEvent::Quit) => return false,
                Ok(ShellEvent::StopAll) => self.audio.send(Cmd::StopAll),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // Finish decoding (e.g. when started hidden) so remote presses work.
                    self.poll_background();
                    self.publish_library();
                    self.autosave();
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn handle_shell_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                ShellEvent::Show => ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
                ShellEvent::StopAll => self.audio.send(Cmd::StopAll),
                ShellEvent::Quit => {
                    self.quit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    pub fn save_now(&mut self) {
        if self.dirty {
            if let Err(e) = self.cfg.save() {
                eprintln!("config: could not save: {e}");
            }
            self.dirty = false;
        }
    }

    // ---------------------------------------------------------------- helpers

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.last_change = Instant::now();
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn request_decode(&mut self, id: u64, path: PathBuf) {
        self.clips.insert(id, ClipState::Loading);
        let _ = self.decode_tx.send((id, path));
    }

    fn sound(&self, id: u64) -> Option<&Sound> {
        self.cfg.sounds.iter().find(|s| s.id == id)
    }

    fn sound_mut(&mut self, id: u64) -> Option<&mut Sound> {
        self.cfg.sounds.iter_mut().find(|s| s.id == id)
    }

    fn add_sounds(&mut self, paths: Vec<PathBuf>) {
        let mut added = 0;
        let mut last = None;
        for path in paths {
            if !decode::is_audio_file(&path) {
                continue;
            }
            let id = self.cfg.alloc_id();
            self.cfg.sounds.push(Sound {
                id,
                title: config::title_from_path(&path),
                path: path.clone(),
                thumbnail: None,
                volume: 1.0,
                shortcut: None,
            });
            self.request_decode(id, path);
            added += 1;
            last = Some(id);
        }
        if added > 0 {
            self.selected = last;
            self.mark_dirty();
            self.notify(format!("Added {added} sound{}", if added == 1 { "" } else { "s" }));
        }
    }

    fn set_thumbnail(&mut self, id: u64, src: PathBuf) {
        match config::import_thumbnail(id, &src) {
            Ok(dest) => {
                if let Some(s) = self.sound_mut(id)
                    && let Some(old) = s.thumbnail.replace(dest) {
                        config::remove_owned_thumbnail(&old);
                    }
                self.mark_dirty();
            }
            Err(e) => self.notify(format!("Could not load image: {e}")),
        }
    }

    fn remove_sound(&mut self, id: u64) {
        self.audio.send(Cmd::Stop(id));
        if let Some(s) = self.sound_mut(id) {
            s.shortcut = None;
        }
        self.sync_hotkeys();
        if let Some(pos) = self.cfg.sounds.iter().position(|s| s.id == id) {
            let s = self.cfg.sounds.remove(pos);
            if let Some(t) = s.thumbnail {
                config::remove_owned_thumbnail(&t);
                self.textures.remove(&t);
            }
        }
        self.clips.remove(&id);
        if self.selected == Some(id) {
            self.selected = None;
        }
        self.mark_dirty();
    }

    fn play(&mut self, id: u64) {
        let Some(sound) = self.sound(id).cloned() else { return };
        let clip = match self.clips.get(&id) {
            Some(ClipState::Ready(clip)) => Some(clip.samples.clone()),
            _ => None,
        };
        let settings = (&self.cfg.settings).into();
        match remote::press(&self.audio.sender(), &self.audio.status, settings, id, sound.volume, clip) {
            Press::Played | Press::Stopped => {}
            Press::OutputsOff => {
                self.notify("Both outputs are off – enable Speakers or Mic in the top bar")
            }
            Press::NotReady => match self.clips.get(&id) {
                Some(ClipState::Failed(e)) => {
                    let msg = format!("Cannot play “{}”: {e}", sound.title);
                    self.notify(msg)
                }
                Some(_) => self.notify(format!("“{}” is still loading…", sound.title)),
                None => self.request_decode(id, sound.path),
            },
        }
    }

    /// Share the current sound list with the control socket; bumps its version on changes.
    fn publish_library(&self) {
        let sounds: Vec<RemoteSound> = self
            .cfg
            .sounds
            .iter()
            .map(|s| {
                let (clip, state) = match self.clips.get(&s.id) {
                    Some(ClipState::Ready(c)) => (Some(c.samples.clone()), "ready"),
                    Some(ClipState::Failed(_)) => (None, "failed"),
                    _ => (None, "loading"),
                };
                RemoteSound {
                    id: s.id,
                    title: s.title.clone(),
                    thumbnail: s.thumbnail.clone(),
                    volume: s.volume,
                    clip,
                    state,
                }
            })
            .collect();
        let settings = (&self.cfg.settings).into();
        let mut lib = self.library.lock().unwrap();
        if lib.sounds != sounds || lib.settings != settings {
            lib.sounds = sounds;
            lib.settings = settings;
            lib.version += 1;
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context, kind: DialogKind) {
        if let Some(since) = self.dialog_open {
            if since.elapsed() < Duration::from_secs(2) {
                return; // double click
            }
            // The previous dialog may still be open somewhere (e.g. another workspace); a late
            // answer from it is still handled normally.
            self.notify("Opening a new file dialog – an earlier one did not respond");
        }
        self.dialog_open = Some(Instant::now());
        let tx = self.pick_tx.clone();
        let ctx = ctx.clone();
        // xdg-desktop-portal dialogs block, so run them off the UI thread.
        std::thread::spawn(move || {
            let audio_filter = ("Audio", decode::AUDIO_EXTENSIONS);
            let result = match kind {
                DialogKind::AddSounds => rfd::FileDialog::new()
                    .set_title("Add sounds")
                    .add_filter(audio_filter.0, audio_filter.1)
                    .add_filter("All files", &["*"])
                    .pick_files()
                    .map(Picked::NewSounds),
                DialogKind::ReplaceAudio(id) => rfd::FileDialog::new()
                    .set_title("Choose audio file")
                    .add_filter(audio_filter.0, audio_filter.1)
                    .pick_file()
                    .map(|p| Picked::ReplaceAudio(id, p)),
                DialogKind::Thumbnail(id) => rfd::FileDialog::new()
                    .set_title("Choose thumbnail image")
                    .add_filter("Images", decode::IMAGE_EXTENSIONS)
                    .pick_file()
                    .map(|p| Picked::Thumbnail(id, p)),
            };
            // Send even on cancel so the UI knows the dialog closed.
            let _ = tx.send(result.unwrap_or(Picked::NewSounds(Vec::new())));
            ctx.request_repaint();
        });
    }

    fn texture(&mut self, ctx: &egui::Context, path: &PathBuf) -> Option<egui::TextureHandle> {
        if let Some(t) = self.textures.get(path) {
            return t.clone();
        }
        let tex = image::open(path).ok().map(|img| {
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            ctx.load_texture(path.to_string_lossy(), color, egui::TextureOptions::LINEAR)
        });
        self.textures.insert(path.clone(), tex.clone());
        tex
    }

    // ---------------------------------------------------------------- per-frame plumbing

    fn poll_background(&mut self) {
        while let Ok(r) = self.decode_rx.try_recv() {
            // Ignore stale results (sound removed or its file was replaced meanwhile).
            let current = self.sound(r.id).map(|s| s.path == r.path).unwrap_or(false);
            if !current {
                continue;
            }
            let state = match r.result {
                Ok(clip) => ClipState::Ready(clip),
                Err(e) => ClipState::Failed(e),
            };
            self.clips.insert(r.id, state);
        }
        while let Ok(p) = self.pick_rx.try_recv() {
            self.dialog_open = None;
            match p {
                Picked::NewSounds(paths) => self.add_sounds(paths),
                Picked::ReplaceAudio(id, path) => {
                    if let Some(s) = self.sound_mut(id) {
                        s.path = path.clone();
                    }
                    self.audio.send(Cmd::Stop(id));
                    self.request_decode(id, path);
                    self.mark_dirty();
                }
                Picked::Thumbnail(id, path) => self.set_thumbnail(id, path),
            }
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if dropped.is_empty() {
            return;
        }
        let (images, audio): (Vec<_>, Vec<_>) =
            dropped.into_iter().partition(|p| decode::is_image_file(p));
        self.add_sounds(audio);
        // Dropping an image sets the thumbnail of the selected sound.
        if let (Some(img), Some(id)) = (images.into_iter().next(), self.selected) {
            self.set_thumbnail(id, img);
        }
    }

    fn autosave(&mut self) {
        if self.dirty && self.last_change.elapsed() > Duration::from_millis(600) {
            if let Err(e) = self.cfg.save() {
                self.notify(format!("Could not save settings: {e}"));
            }
            self.dirty = false;
        }
    }

    // ---------------------------------------------------------------- keyboard shortcuts

    fn shortcut_of(&self, target: ShortcutTarget) -> Option<Combo> {
        let s = match target {
            ShortcutTarget::Sound(id) => self.sound(id)?.shortcut.as_ref()?,
            ShortcutTarget::StopAll => self.cfg.settings.stop_all_shortcut.as_ref()?,
        };
        Combo::parse(s)
    }

    fn set_shortcut(&mut self, target: ShortcutTarget, combo: Option<&Combo>) {
        let value = combo.map(|c| c.to_string());
        match target {
            ShortcutTarget::Sound(id) => {
                if let Some(s) = self.sound_mut(id) {
                    s.shortcut = value;
                }
            }
            ShortcutTarget::StopAll => self.cfg.settings.stop_all_shortcut = value,
        }
        self.mark_dirty();
        self.sync_hotkeys();
    }

    /// Hand the current bindings to the keyboard listener.
    fn sync_hotkeys(&self) {
        let mut map: HashMap<Combo, Action> = self
            .cfg
            .sounds
            .iter()
            .filter_map(|s| Some((Combo::parse(s.shortcut.as_ref()?)?, Action::Play(s.id))))
            .collect();
        if let Some(c) = self.shortcut_of(ShortcutTarget::StopAll) {
            map.insert(c, Action::StopAll);
        }
        self.hotkeys.set_bindings(map);
    }

    /// Who already uses `combo` (other than `target`)?
    fn shortcut_user(&self, combo: &Combo, target: ShortcutTarget) -> Option<String> {
        if let Some(s) = self.cfg.sounds.iter().find(|s| {
            ShortcutTarget::Sound(s.id) != target
                && s.shortcut.as_deref().and_then(Combo::parse).as_ref() == Some(combo)
        }) {
            return Some(format!("“{}”", s.title));
        }
        (target != ShortcutTarget::StopAll
            && self.shortcut_of(ShortcutTarget::StopAll).as_ref() == Some(combo))
        .then(|| "Stop all".to_string())
    }

    /// Pick up the combination pressed while recording.
    fn poll_recording(&mut self) {
        let Some((target, rx)) = &self.recording else { return };
        let target = *target;
        let Ok(recorded) = rx.try_recv() else { return };
        self.recording = None;
        let Recorded::Combo(combo) = recorded else { return };
        let error = if !combo.is_valid() {
            Some(format!("{combo} would fire while typing – add Ctrl, Alt, Shift or Super."))
        } else {
            self.shortcut_user(&combo, target).map(|who| format!("{combo} is already used for {who}."))
        };
        match error {
            Some(e) => self.shortcut_error = Some((target, e)),
            None => {
                self.shortcut_error = None;
                self.set_shortcut(target, Some(&combo));
            }
        }
    }

    fn shortcut_editor(&mut self, ui: &mut egui::Ui, target: ShortcutTarget) {
        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;
        match self.hotkeys.access() {
            Access::Ok(_) => {}
            Access::Starting => {
                ui.spinner();
                return;
            }
            Access::NoKeyboards => {
                hint(ui, &p, "No keyboard found.");
                return;
            }
            Access::NeedsRelogin => {
                hint(ui, &p, "Almost there: log out and back in so shortcuts can be used.");
                return;
            }
            Access::NoPermission => {
                hint(
                    ui,
                    &p,
                    "Shortcuts need permission to read the keyboard. Run this once in a terminal, then log \
                     out and back in:",
                );
                let cmd = "sudo usermod -aG input $USER";
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(cmd).monospace().color(p.text));
                    if button(ui, &p, ang, Btn::Ghost, "", "Copy").clicked() {
                        ui.ctx().copy_text(cmd.into());
                        self.notify("Copied to the clipboard");
                    }
                });
                return;
            }
        }

        let recording_this = matches!(&self.recording, Some((t, _)) if *t == target);
        ui.horizontal(|ui| {
            if recording_this {
                let pulse = (ui.input(|i| i.time) * 4.0).sin().abs() as f32;
                ui.label(caps("Press the keys…", 11.0, theme::mix(p.accent, p.text_dim, pulse * 0.6)));
                ui.ctx().request_repaint_after(Duration::from_millis(50));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button(ui, &p, ang, Btn::Ghost, "", "Cancel").clicked() {
                        self.hotkeys.stop_recording();
                        self.recording = None;
                    }
                });
                return;
            }
            let record = match self.shortcut_of(target) {
                Some(combo) => {
                    key_chip(ui, &p, ang, &combo.to_string());
                    let mut change = false;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, &p, ang, Btn::Ghost, "✖", "").on_hover_text("Remove shortcut").clicked() {
                            self.set_shortcut(target, None);
                            self.shortcut_error = None;
                        }
                        change = button(ui, &p, ang, Btn::Ghost, "", "Change").clicked();
                    });
                    change
                }
                None => button(ui, &p, ang, Btn::Ghost, "⌨", "Set shortcut").clicked(),
            };
            if record {
                self.recording = Some((target, self.hotkeys.record()));
                self.shortcut_error = None;
            }
        });
        if recording_this {
            hint(ui, &p, "Hold Ctrl, Alt, Shift or Super and press a key. Esc cancels.");
        }
        if let Some((t, e)) = &self.shortcut_error
            && *t == target
        {
            ui.colored_label(p.danger, e);
        }
    }

    // ---------------------------------------------------------------- UI sections

    /// Re-apply the egui style (after a theme or shape change).
    fn restyle(&self, ctx: &egui::Context) {
        theme::apply(ctx, &self.theme.palette, self.cfg.settings.angular);
    }

    fn logo(&mut self, ctx: &egui::Context) -> egui::TextureHandle {
        self.logo
            .get_or_insert_with(|| {
                // The monochrome tray artwork; tinted with the theme's text color when drawn.
                let img = image::load_from_memory(include_bytes!("../assets/tray-icon.png"))
                    .map(|i| i.resize(64, 64, image::imageops::FilterType::Lanczos3).into_rgba8())
                    .unwrap_or_default();
                let size = [img.width() as usize, img.height() as usize];
                ctx.load_texture(
                    "logo",
                    egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()),
                    egui::TextureOptions::LINEAR,
                )
            })
            .clone()
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;
        let logo = self.logo(ui.ctx());
        ui.horizontal_centered(|ui| {
            ui.add(egui::Image::new((logo.id(), vec2(28.0, 28.0))).tint(p.text));
            ui.label(caps("Soundboard", 14.0, p.text));
            ui.add_space(14.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("🔍  Search sounds")
                    .desired_width(210.0)
                    .margin(vec2(10.0, 6.0)),
            );
            if !self.search.is_empty() && button(ui, &p, ang, Btn::Ghost, "✖", "").on_hover_text("Clear search").clicked() {
                self.search.clear();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, &p, ang, Btn::Toggle(self.show_settings), "⚙", "Settings")
                    .on_hover_text("Output devices, microphone, behaviour and appearance")
                    .clicked()
                {
                    self.show_settings = !self.show_settings;
                }
                if button(ui, &p, ang, Btn::Danger, "⏹", "Stop all")
                    .on_hover_text("Stop all sounds (Esc)")
                    .clicked()
                {
                    self.audio.send(Cmd::StopAll);
                }
                ui.add_space(10.0);

                let st = &mut self.cfg.settings;
                let mut changed = false;
                ui.label(
                    egui::RichText::new(format!("{:>3.0}%", st.master_volume * 100.0))
                        .monospace()
                        .color(p.text_dim),
                );
                ui.spacing_mut().slider_width = 110.0;
                let r = ui
                    .add(egui::Slider::new(&mut st.master_volume, 0.0..=2.0).show_value(false))
                    .on_hover_text("Master volume");
                if r.changed() {
                    self.audio.send(Cmd::SetMaster(st.master_volume));
                    changed = true;
                }
                ui.label(egui::RichText::new("🔈").color(p.text_muted));
                ui.add_space(10.0);
                if button(ui, &p, ang, Btn::Toggle(st.play_to_mic), "🎤", "Mic")
                    .on_hover_text("Send sounds to the microphone output")
                    .clicked()
                {
                    st.play_to_mic = !st.play_to_mic;
                    changed = true;
                }
                if button(ui, &p, ang, Btn::Toggle(st.play_to_speakers), "🔊", "Speakers")
                    .on_hover_text("Play sounds on your speakers/headphones")
                    .clicked()
                {
                    st.play_to_speakers = !st.play_to_speakers;
                    changed = true;
                }
                if changed {
                    self.mark_dirty();
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.theme.palette.clone();
        let status = self.audio.status.lock().unwrap();
        let shortcuts_blocked = !matches!(self.hotkeys.access(), Access::Ok(_) | Access::Starting)
            && (self.cfg.settings.stop_all_shortcut.is_some() || self.cfg.sounds.iter().any(|s| s.shortcut.is_some()));
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            if let Some(err) = &status.error {
                status_item(ui, p.danger, err, p.danger);
            } else if !status.connected {
                status_item(ui, p.warn, "Connecting to PipeWire…", p.text_muted);
            } else {
                status_item(ui, p.success, "PipeWire", p.text_muted);
                const VMIC_HINT: &str = "Select “Soundboard Virtual Mic” as the input device in Discord, OBS, \
                     your browser, etc. Sounds are only sent to the mic while an app is listening.";
                let (dot, text, hint) = match &self.cfg.settings.mic_output {
                    None if status.mic_in_use => (p.accent, "Virtual mic · in use".to_string(), VMIC_HINT),
                    None => (p.text_muted, "Virtual mic · idle".to_string(), VMIC_HINT),
                    Some(name) => {
                        let desc = status
                            .mic_targets
                            .iter()
                            .find(|d| &d.name == name)
                            .map_or(name.as_str(), |d| d.description.as_str());
                        if !status.mic_output_linked {
                            (
                                p.warn,
                                format!("{desc} · unavailable"),
                                "The selected mic output is not running (e.g. EasyEffects is closed), \
                                 so sounds are not reaching any microphone.",
                            )
                        } else if status.mic_in_use {
                            (p.accent, format!("{desc} · in use"), "Sounds are mixed into this microphone.")
                        } else {
                            (p.text_muted, format!("{desc} · idle"), "No app is recording from this microphone.")
                        }
                    }
                };
                status_item(ui, dot, &text, p.text_muted).on_hover_text(hint);
                if self.cfg.settings.mic_passthrough && self.cfg.settings.mic_output.is_none() {
                    status_item(ui, p.accent2, &format!("Passthrough · {}", status.capture_state), p.text_muted);
                }
                if shortcuts_blocked {
                    status_item(ui, p.warn, "Shortcuts need setup", p.text_muted)
                        .on_hover_text("Open a sound's editor to see what's missing.");
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let n = self.cfg.sounds.len();
                ui.label(caps(&format!("{n} sound{}", if n == 1 { "" } else { "s" }), 10.5, p.text_muted));
            });
        });
    }

    /// Settings page shown in the main area: two columns on wide windows, one on narrow ones.
    fn settings_page(&mut self, ui: &mut egui::Ui) {
        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let width = ui.available_width().min(1100.0);
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - width) / 2.0);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    if panel_header(ui, &p, ang, "Settings") {
                        self.show_settings = false;
                    }
                    if width >= 760.0 {
                        ui.columns(2, |cols| {
                            self.settings_audio(&mut cols[0]);
                            self.settings_app(&mut cols[1]);
                        });
                    } else {
                        self.settings_audio(ui);
                        self.settings_app(ui);
                    }
                    ui.add_space(16.0);
                    hint(ui, &p, &format!("Library: {}", config::config_path().display()));
                });
            });
        });
    }

    fn settings_audio(&mut self, ui: &mut egui::Ui) {
        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;
        let (sinks, sources, mic_targets) = {
            let s = self.audio.status.lock().unwrap();
            (s.sinks.clone(), s.sources.clone(), s.mic_targets.clone())
        };

        section_header(ui, &p, "Output");
        side_card(ui, &p, ang, |ui| {
            field_label(ui, &p, "Speakers / headphones");
            let mut target = self.cfg.settings.speaker_target.clone();
            if device_combo(ui, "spk_combo", "System default", &mut target, &sinks) {
                self.cfg.settings.speaker_target = target.clone();
                self.audio.send(Cmd::SetSpeakerTarget(target));
                self.mark_dirty();
            }
        });

        section_header(ui, &p, "Microphone");
        side_card(ui, &p, ang, |ui| {
            field_label(ui, &p, "Send sounds into");
            let mut out = self.cfg.settings.mic_output.clone();
            if device_combo(ui, "mic_out_combo", "Soundboard Virtual Mic", &mut out, &mic_targets) {
                self.cfg.settings.mic_output = out.clone();
                self.audio.send(Cmd::SetMicOutput(out));
                self.mark_dirty();
            }
            if self.cfg.settings.mic_output.is_some() {
                hint(
                    ui,
                    &p,
                    "Sounds are mixed into this microphone, after its effects. Apps that already use it \
                     hear them without changing their input device.",
                );
            } else {
                self.passthrough_section(ui, &sources);
            }
        });

    }

    fn settings_app(&mut self, ui: &mut egui::Ui) {
        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;

        section_header(ui, &p, "Behaviour");
        side_card(ui, &p, ang, |ui| self.behaviour_section(ui));

        section_header(ui, &p, "Shortcuts");
        side_card(ui, &p, ang, |ui| {
            field_label(ui, &p, "Stop all sounds");
            self.shortcut_editor(ui, ShortcutTarget::StopAll);
        });

        section_header(ui, &p, "Appearance");
        side_card(ui, &p, ang, |ui| self.appearance_section(ui));
    }

    fn passthrough_section(&mut self, ui: &mut egui::Ui, sources: &[Device]) {
        let p = self.theme.palette.clone();
        hint(
            ui,
            &p,
            "Apps see a microphone called “Soundboard Virtual Mic”. Pick it as the input device in your \
             voice or streaming app.",
        );
        ui.add_space(4.0);

        let st = &mut self.cfg.settings;
        let mut pass_changed = ui.checkbox(&mut st.mic_passthrough, "Mix my real microphone into it").changed();
        ui.add_enabled_ui(st.mic_passthrough, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Mic level").color(p.text_dim));
                pass_changed |= ui
                    .add(
                        egui::Slider::new(&mut st.mic_passthrough_volume, 0.0..=2.0)
                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                    )
                    .changed();
            });
        });
        if pass_changed {
            self.audio.send(Cmd::SetPassthrough {
                enabled: st.mic_passthrough,
                gain: st.mic_passthrough_volume,
            });
            self.mark_dirty();
        }

        let enabled = self.cfg.settings.mic_passthrough;
        ui.add_enabled_ui(enabled, |ui| {
            field_label(ui, &p, "Real microphone");
            let mut src = self.cfg.settings.mic_source.clone();
            if device_combo(ui, "mic_combo", "System default", &mut src, sources) {
                self.cfg.settings.mic_source = src.clone();
                self.audio.send(Cmd::SetMicSource(src));
                self.mark_dirty();
            }
        });
    }

    fn behaviour_section(&mut self, ui: &mut egui::Ui) {
        let st = &mut self.cfg.settings;
        let mut changed = false;
        changed |= ui
            .checkbox(&mut st.allow_overlap, "Allow sounds to overlap")
            .on_hover_text("When off, starting a sound stops all others")
            .changed();
        changed |= ui
            .checkbox(&mut st.click_playing_stops, "Click a playing sound to stop it")
            .on_hover_text("When off, clicking restarts it from the beginning")
            .changed();
        let tray = self.tray_available;
        ui.add_enabled_ui(tray, |ui| {
            changed |= ui
                .checkbox(&mut st.close_to_tray, "Keep running in the tray when closed")
                .on_hover_text("Sounds, the virtual mic, shortcuts and StreamController keep working")
                .on_disabled_hover_text("Your desktop has no system tray")
                .changed();
            ui.add_enabled_ui(st.close_to_tray, |ui| {
                changed |= ui.checkbox(&mut st.start_hidden, "Start hidden in the tray").changed();
            });
        });
        if changed {
            self.mark_dirty();
        }
    }

    fn appearance_section(&mut self, ui: &mut egui::Ui) {
        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;
        let ctx = ui.ctx().clone();

        field_label(ui, &p, "Colors");
        let current = self.cfg.settings.theme.clone();
        let desktop = format!("Follow desktop ({})", self.theme.current.as_deref().unwrap_or("none found"));
        let label_of = |src: &str| match src {
            "" => desktop.clone(),
            "builtin" => "Built-in".to_string(),
            name => name.to_string(),
        };
        let mut choice = current.clone();
        egui::ComboBox::from_id_salt("theme_combo")
            .selected_text(label_of(&current))
            .width(ui.available_width() - 8.0)
            .truncate()
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut choice, String::new(), label_of(""));
                for name in theme::list_schemes() {
                    ui.selectable_value(&mut choice, name.clone(), name);
                }
                ui.selectable_value(&mut choice, "builtin".to_string(), "Built-in");
            });
        hint(
            ui,
            &p,
            "Uses the qt6ct/qt5ct color schemes your desktop theme writes, and follows it live when you \
             switch themes.",
        );
        if choice != current {
            self.cfg.settings.theme = choice.clone();
            self.theme.set_source(&choice);
            self.restyle(&ctx);
            self.mark_dirty();
        }

        ui.add_space(6.0);
        field_label(ui, &p, "Shape");
        ui.horizontal(|ui| {
            let mut pick = None;
            if button(ui, &p, ang, Btn::Toggle(ang), "", "Angular").clicked() {
                pick = Some(true);
            }
            if button(ui, &p, ang, Btn::Toggle(!ang), "", "Rounded").clicked() {
                pick = Some(false);
            }
            if let Some(a) = pick
                && a != ang
            {
                self.cfg.settings.angular = a;
                self.restyle(&ctx);
                self.mark_dirty();
            }
        });

        ui.add_space(6.0);
        field_label(ui, &p, "Tile size");
        ui.spacing_mut().slider_width = ui.available_width() - 8.0;
        if ui.add(egui::Slider::new(&mut self.cfg.settings.tile_size, 80.0..=220.0).show_value(false)).changed() {
            self.mark_dirty();
        }
    }

    fn editor_panel(&mut self, ui: &mut egui::Ui, id: u64) {
        let ctx = ui.ctx().clone();
        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;
        let Some(sound) = self.sound(id).cloned() else {
            self.selected = None;
            return;
        };
        if panel_header(ui, &p, ang, "Edit sound") {
            self.selected = None;
        }

        section_header(ui, &p, "Picture");
        side_card(ui, &p, ang, |ui| {
            let side = ui.available_width().min(240.0);
            let (rect, resp) = ui.allocate_exact_size(vec2(side, side), Sense::click());
            let tex = sound.thumbnail.as_ref().and_then(|t| self.texture(&ctx, t));
            let radius = if ang { 0 } else { 8 };
            let notch = 16.0;
            let painter = ui.painter();
            paint_cover(painter, rect, tex.as_ref(), id, radius);
            if resp.hovered() {
                painter.rect_filled(rect, radius, theme::alpha(p.bg_deep, 0.65));
                painter.text(rect.center(), Align2::CENTER_CENTER, "🖼  CHANGE PICTURE", theme::bold(12.0), p.accent);
            }
            if ang {
                theme::cut_corner(painter, rect, notch, p.surface);
                painter.add(egui::Shape::closed_line(theme::notched(rect.shrink(0.5), notch), Stroke::new(1.0, p.border)));
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.open_dialog(&ctx, DialogKind::Thumbnail(id));
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if button(ui, &p, ang, Btn::Ghost, "🖼", "Choose…").clicked() {
                    self.open_dialog(&ctx, DialogKind::Thumbnail(id));
                }
                if sound.thumbnail.is_some() && button(ui, &p, ang, Btn::Ghost, "", "Remove").clicked() {
                    if let Some(s) = self.sound_mut(id)
                        && let Some(old) = s.thumbnail.take()
                    {
                        config::remove_owned_thumbnail(&old);
                    }
                    self.mark_dirty();
                }
            });
            hint(ui, &p, "Tip: drop an image onto the window to use it here.");
        });

        section_header(ui, &p, "Details");
        side_card(ui, &p, ang, |ui| {
            field_label(ui, &p, "Title");
            let mut title = sound.title.clone();
            if ui
                .add(egui::TextEdit::singleline(&mut title).desired_width(f32::INFINITY).margin(vec2(8.0, 5.0)))
                .changed()
            {
                if let Some(s) = self.sound_mut(id) {
                    s.title = title;
                }
                self.mark_dirty();
            }

            ui.add_space(4.0);
            field_label(ui, &p, "Volume");
            let mut vol = sound.volume;
            ui.spacing_mut().slider_width = (ui.available_width() - 60.0).max(80.0);
            if ui
                .add(egui::Slider::new(&mut vol, 0.0..=2.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)))
                .changed()
            {
                if let Some(s) = self.sound_mut(id) {
                    s.volume = vol;
                }
                self.audio.send(Cmd::SetVoiceGain { id, gain: vol });
                self.mark_dirty();
            }

            ui.add_space(4.0);
            field_label(ui, &p, "Audio file");
            ui.add(
                egui::Label::new(egui::RichText::new(sound.path.display().to_string()).small().color(p.text_dim))
                    .wrap(),
            );
            match self.clips.get(&id) {
                Some(ClipState::Ready(c)) => {
                    ui.label(egui::RichText::new(format!("{:.1} s", c.seconds)).small().color(p.text_muted));
                }
                Some(ClipState::Loading) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new("Decoding…").small().color(p.text_muted));
                    });
                }
                Some(ClipState::Failed(e)) => {
                    ui.colored_label(p.danger, format!("⚠ {e}"));
                }
                None => {}
            }
            if button(ui, &p, ang, Btn::Ghost, "📂", "Replace audio…").clicked() {
                self.open_dialog(&ctx, DialogKind::ReplaceAudio(id));
            }
        });

        section_header(ui, &p, "Keyboard shortcut");
        side_card(ui, &p, ang, |ui| self.shortcut_editor(ui, ShortcutTarget::Sound(id)));

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let playing = self.audio.status.lock().unwrap().progress(id).is_some();
            if playing {
                if button(ui, &p, ang, Btn::Primary, "⏹", "Stop").clicked() {
                    self.audio.send(Cmd::Stop(id));
                }
            } else if button(ui, &p, ang, Btn::Primary, "▶", "Play").clicked() {
                self.play(id);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, &p, ang, Btn::Danger, "🗑", "Remove").clicked() {
                    self.pending_delete = Some(id);
                }
            });
        });
    }

    fn grid(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;
        let size = self.cfg.settings.tile_size;
        let query = self.search.trim().to_lowercase();
        let visible: Vec<Sound> = self
            .cfg
            .sounds
            .iter()
            .filter(|s| {
                query.is_empty()
                    || s.title.to_lowercase().contains(&query)
                    || s.path
                        .file_name()
                        .map(|f| f.to_string_lossy().to_lowercase().contains(&query))
                        .unwrap_or(false)
            })
            .cloned()
            .collect();

        let progress: HashMap<u64, f32> = {
            let st = self.audio.status.lock().unwrap();
            visible.iter().filter_map(|s| st.progress(s.id).map(|p| (s.id, p))).collect()
        };
        let time = ui.input(|i| i.time);

        ui.horizontal(|ui| {
            ui.label(caps("Sounds", 12.0, p.text_dim));
            ui.label(caps(&visible.len().to_string(), 12.0, p.accent));
            if !progress.is_empty() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(caps(&format!("{} playing", progress.len()), 11.0, p.accent));
                });
            }
        });
        ui.add_space(10.0);

        let mut clicked = None;
        let mut action: Option<(u64, TileAction)> = None;

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(14.0, 14.0);
                for s in &visible {
                    let tex = s.thumbnail.as_ref().and_then(|t| self.texture(&ctx, t));
                    let state = match self.clips.get(&s.id) {
                        Some(ClipState::Ready(_)) => TileState::Ready,
                        Some(ClipState::Failed(_)) => TileState::Failed,
                        _ => TileState::Loading,
                    };
                    let resp = sound_tile(
                        ui,
                        &p,
                        ang,
                        size,
                        Tile {
                            sound: s,
                            tex: tex.as_ref(),
                            state,
                            progress: progress.get(&s.id).copied(),
                            selected: self.selected == Some(s.id),
                        },
                        time,
                    );
                    if resp.clicked() {
                        clicked = Some(s.id);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("▶  Play").clicked() {
                            action = Some((s.id, TileAction::Play));
                        }
                        if ui.button("⏹  Stop").clicked() {
                            action = Some((s.id, TileAction::Stop));
                        }
                        ui.separator();
                        if ui.button("✏  Edit…").clicked() {
                            action = Some((s.id, TileAction::Edit));
                        }
                        if ui.button("🖼  Set picture…").clicked() {
                            action = Some((s.id, TileAction::Image));
                        }
                        ui.separator();
                        if ui.button(egui::RichText::new("🗑  Remove").color(p.danger)).clicked() {
                            action = Some((s.id, TileAction::Remove));
                        }
                    });
                }
                if query.is_empty() && add_tile(ui, &p, ang, size).clicked() {
                    action = Some((0, TileAction::Add));
                }
            });

            if self.cfg.sounds.is_empty() {
                ui.add_space(36.0);
                ui.vertical_centered(|ui| {
                    ui.label(caps("No sounds yet", 16.0, p.text));
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Drag audio files onto this window, or add them with the button.")
                            .color(p.text_dim),
                    );
                    ui.add_space(10.0);
                    if button(ui, &p, ang, Btn::Primary, "+", "Add sounds").clicked() {
                        action = Some((0, TileAction::Add));
                    }
                });
            } else if visible.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(format!("No sounds match “{}”", self.search)).color(p.text_dim));
                });
            }
        });

        if let Some(id) = clicked {
            self.selected = Some(id);
            self.play(id);
        }
        if let Some((id, a)) = action {
            match a {
                TileAction::Play => {
                    self.audio.send(Cmd::Stop(id));
                    self.play(id);
                }
                TileAction::Stop => self.audio.send(Cmd::Stop(id)),
                TileAction::Edit => self.selected = Some(id),
                TileAction::Image => {
                    self.selected = Some(id);
                    self.open_dialog(&ctx, DialogKind::Thumbnail(id));
                }
                TileAction::Remove => self.pending_delete = Some(id),
                TileAction::Add => self.open_dialog(&ctx, DialogKind::AddSounds),
            }
        }
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.pending_delete else { return };
        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;
        let title = self.sound(id).map(|s| s.title.clone()).unwrap_or_default();
        let mut decision = None;
        egui::Modal::new(egui::Id::new("confirm_delete")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.label(caps("Remove sound?", 14.0, p.text));
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "“{title}” will be removed from the board. The audio file itself is not deleted."
                ))
                .color(p.text_dim),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if button(ui, &p, ang, Btn::Danger, "🗑", "Remove").clicked() {
                    decision = Some(true);
                }
                if button(ui, &p, ang, Btn::Ghost, "", "Cancel").clicked()
                    || ui.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                self.remove_sound(id);
                self.pending_delete = None;
            }
            Some(false) => self.pending_delete = None,
            None => {}
        }
    }
}

#[derive(Clone, Copy)]
enum DialogKind {
    AddSounds,
    ReplaceAudio(u64),
    Thumbnail(u64),
}

enum TileAction {
    Play,
    Stop,
    Edit,
    Image,
    Remove,
    Add,
}

#[derive(PartialEq)]
enum TileState {
    Loading,
    Ready,
    Failed,
}

impl eframe::App for SoundboardApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Follow the desktop's color scheme live (checked about once a second).
        if self.theme.poll() {
            self.restyle(&ctx);
        }
        ctx.request_repaint_after(Duration::from_secs(1));
        self.handle_shell_events(&ctx);
        self.poll_background();
        self.handle_drops(&ctx);

        self.poll_recording();

        // Keyboard: Delete removes the selected sound, Escape stops everything.
        if self.recording.is_none() && ctx.memory(|m| m.focused().is_none()) {
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && self.pending_delete.is_none() {
                self.audio.send(Cmd::StopAll);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
                self.pending_delete = self.selected;
            }
        }

        let p = self.theme.palette.clone();
        let ang = self.cfg.settings.angular;
        egui::Panel::top("top")
            .exact_size(54.0)
            .frame(egui::Frame::new().fill(p.bg_deep).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status")
            .exact_size(28.0)
            .frame(egui::Frame::new().fill(p.bg_deep).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ui, |ui| self.status_bar(ui));

        // The sound editor is a fixed-width side panel (a resizable one grows with its
        // full-width cards); settings take over the main area.
        if let Some(id) = self.selected {
            egui::Panel::right("side")
                .resizable(false)
                .exact_size(330.0)
                .frame(egui::Frame::new().fill(p.bg_deep).inner_margin(egui::Margin::same(14)))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                        self.editor_panel(ui, id);
                        ui.add_space(8.0);
                    });
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(if self.show_settings { p.bg_deep } else { p.bg }).inner_margin(egui::Margin::same(18)))
            .show(ui, |ui| {
                if self.show_settings {
                    self.settings_page(ui);
                } else {
                    self.grid(ui);
                }
            });

        // Drag-and-drop hint overlay
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop_overlay")));
            painter.rect_filled(screen, 0, theme::alpha(p.bg, 0.9));
            let mut pts = theme::notched(screen.shrink(28.0), 28.0);
            pts.push(pts[0]);
            painter.extend(egui::Shape::dashed_line(&pts, Stroke::new(2.0, p.accent), 10.0, 6.0));
            painter.text(screen.center() - vec2(0.0, 14.0), Align2::CENTER_CENTER, "DROP TO ADD SOUNDS", theme::bold(22.0), p.accent);
            painter.text(
                screen.center() + vec2(0.0, 18.0),
                Align2::CENTER_CENTER,
                "Drop an image to set the selected sound's picture",
                FontId::proportional(13.0),
                p.text_dim,
            );
        }

        self.delete_dialog(&ctx);

        if let Some((msg, at)) = &self.toast {
            if at.elapsed() < Duration::from_secs(3) {
                egui::Area::new(egui::Id::new("toast"))
                    .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -44.0))
                    .show(&ctx, |ui| {
                        egui::Frame::new()
                            .fill(p.bg_deep)
                            .stroke(Stroke::new(1.0, p.accent))
                            .corner_radius(theme::radius(ang))
                            .inner_margin(egui::Margin::symmetric(16, 9))
                            .shadow(ui.visuals().popup_shadow)
                            .show(ui, |ui| ui.label(egui::RichText::new(msg.as_str()).color(p.text)));
                    });
                ctx.request_repaint_after(Duration::from_millis(200));
            } else {
                self.toast = None;
            }
        }

        // Animate progress bars while something plays.
        if self.audio.status.lock().unwrap().anything_playing() {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        if self.dirty {
            ctx.request_repaint_after(Duration::from_millis(700));
        }
        self.publish_library();
        self.autosave();
    }
}

impl Drop for SoundboardApp {
    fn drop(&mut self) {
        if self.dirty {
            let _ = self.cfg.save();
        }
    }
}

// -------------------------------------------------------------------- widgets

/// Combo box choosing a PipeWire node by `node.name`; `None` = follow the system default.
/// Returns true if the selection changed.
fn device_combo(
    ui: &mut egui::Ui,
    id: &str,
    none_label: &str,
    value: &mut Option<String>,
    devices: &[Device],
) -> bool {
    let label = match value {
        None => none_label.to_string(),
        Some(name) => devices
            .iter()
            .find(|d| &d.name == name)
            .map(|d| d.description.clone())
            .unwrap_or_else(|| format!("{name} (not connected)")),
    };
    let before = value.clone();
    egui::ComboBox::from_id_salt(id)
        .selected_text(label)
        .width(ui.available_width() - 8.0)
        .truncate()
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, none_label);
            for d in devices {
                ui.selectable_value(value, Some(d.name.clone()), &d.description)
                    .on_hover_text(&d.name);
            }
        });
    *value != before
}

/// Deterministic pleasant colour for tiles without a thumbnail (mirrored by the StreamController
/// plugin, keep in sync).
fn tile_color(id: u64) -> Color32 {
    let h = (id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as f32 / (1u64 << 24) as f32;
    let hsva = egui::ecolor::Hsva::new(h, 0.45, 0.55, 1.0);
    hsva.into()
}

/// Uppercase, letter-spaced bold label (the shell's caption style).
fn caps(text: &str, size: f32, color: Color32) -> egui::RichText {
    egui::RichText::new(text.to_uppercase())
        .font(theme::bold(size))
        .color(color)
        .extra_letter_spacing(size * 0.09)
}

fn field_label(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.label(caps(text, 10.0, p.text_muted));
}

fn hint(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).small().color(p.text_muted)).wrap());
}

/// Accent caption followed by a thin rule, like the shell's section headers.
fn section_header(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add_space(12.0);
    let resp = ui.label(caps(text, 10.5, p.accent));
    let y = resp.rect.center().y;
    let right = ui.max_rect().right();
    if resp.rect.right() + 8.0 < right {
        ui.painter().hline(resp.rect.right() + 8.0..=right, y, Stroke::new(1.0, p.border));
    }
    ui.add_space(2.0);
}

/// Panel title with a close button. Returns true if closed.
fn panel_header(ui: &mut egui::Ui, p: &Palette, angular: bool, title: &str) -> bool {
    ui.horizontal(|ui| {
        ui.label(caps(title, 13.0, p.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            button(ui, p, angular, Btn::Ghost, "✖", "").on_hover_text("Close").clicked()
        })
        .inner
    })
    .inner
}

/// Bordered card for a group of settings; notched in the angular style.
fn side_card<R>(ui: &mut egui::Ui, p: &Palette, angular: bool, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let shown = egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(if angular { 0 } else { 10 })
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    if angular {
        let rect = shown.response.rect;
        let n = 10.0;
        theme::cut_corner(ui.painter(), rect, n, p.bg_deep);
        ui.painter().line_segment(
            [pos2(rect.right() - n, rect.bottom() - 0.5), pos2(rect.right() - 0.5, rect.bottom() - n)],
            Stroke::new(1.0, p.border),
        );
    }
    shown.inner
}

fn status_item(ui: &mut egui::Ui, dot: Color32, text: &str, color: Color32) -> egui::Response {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 3.5, dot);
        ui.label(caps(text, 10.0, color));
    })
    .response
}

#[derive(Clone, Copy)]
enum Btn {
    Primary,
    Danger,
    Ghost,
    Toggle(bool),
}

/// Icon + uppercase label button in the app's style.
fn button(ui: &mut egui::Ui, p: &Palette, angular: bool, kind: Btn, icon: &str, text: &str) -> egui::Response {
    let mut job = egui::text::LayoutJob::default();
    if !icon.is_empty() {
        job.append(
            icon,
            0.0,
            egui::TextFormat {
                font_id: FontId::proportional(13.0),
                color: Color32::PLACEHOLDER,
                valign: egui::Align::Center,
                ..Default::default()
            },
        );
    }
    if !text.is_empty() {
        job.append(
            &text.to_uppercase(),
            if icon.is_empty() { 0.0 } else { 7.0 },
            egui::TextFormat {
                font_id: theme::bold(11.0),
                color: Color32::PLACEHOLDER,
                extra_letter_spacing: 1.0,
                valign: egui::Align::Center,
                ..Default::default()
            },
        );
    }
    let galley = ui.painter().layout_job(job);
    let pad = if text.is_empty() { 16.0 } else { 24.0 };
    let size = vec2((galley.size().x + pad).max(28.0), 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if ui.is_rect_visible(rect) {
        let h = resp.hovered();
        let (fill, stroke, fg) = match kind {
            Btn::Primary => (
                if h { theme::mix(p.accent, Color32::WHITE, 0.15) } else { p.accent },
                p.accent,
                p.on_accent,
            ),
            Btn::Danger => (theme::alpha(p.danger, if h { 0.24 } else { 0.10 }), p.danger, p.danger),
            Btn::Ghost => (
                if h { p.surface_hi } else { p.surface },
                if h { p.accent } else { p.border },
                if h { p.text } else { p.text_dim },
            ),
            Btn::Toggle(true) => (theme::alpha(p.accent, if h { 0.30 } else { 0.18 }), p.accent, p.accent),
            Btn::Toggle(false) => (
                if h { p.surface_hi } else { p.surface },
                if h { p.border_strong } else { p.border },
                p.text_dim,
            ),
        };
        let rect = if resp.is_pointer_button_down_on() { rect.shrink(1.0) } else { rect };
        theme::card(ui.painter(), rect, angular, 7.0, fill, Stroke::new(1.0, stroke));
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
    }
    resp
}

/// A key combination shown like keyboard keys: one box per key.
fn key_chip(ui: &mut egui::Ui, p: &Palette, angular: bool, combo: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, key) in combo.split('+').enumerate() {
            if i > 0 {
                ui.label(egui::RichText::new("+").color(p.text_muted));
            }
            let g = ui.painter().layout_no_wrap(key.to_string(), FontId::monospace(12.0), p.accent);
            let (rect, _) = ui.allocate_exact_size(g.size() + vec2(14.0, 10.0), Sense::hover());
            theme::card(ui.painter(), rect, angular, 5.0, theme::alpha(p.accent, 0.10), Stroke::new(1.0, p.accent));
            ui.painter().galley(rect.center() - g.size() / 2.0, g, p.accent);
        }
    });
}

/// Paint `tex` cropped to fill `rect` (like CSS `object-fit: cover`), or a placeholder.
fn paint_cover(painter: &egui::Painter, rect: Rect, tex: Option<&egui::TextureHandle>, id: u64, radius: u8) {
    match tex {
        Some(tex) => {
            let [w, h] = tex.size();
            let target = rect.width() / rect.height();
            let src = w as f32 / h as f32;
            let uv = if src > target {
                let f = target / src;
                Rect::from_min_max(pos2((1.0 - f) / 2.0, 0.0), pos2((1.0 + f) / 2.0, 1.0))
            } else {
                let f = src / target;
                Rect::from_min_max(pos2(0.0, (1.0 - f) / 2.0), pos2(1.0, (1.0 + f) / 2.0))
            };
            painter.add(egui::epaint::RectShape::filled(rect, radius, Color32::WHITE).with_texture(tex.id(), uv));
        }
        None => {
            painter.rect_filled(rect, radius, tile_color(id));
            painter.text(
                rect.center() - vec2(0.0, rect.height() * 0.08),
                Align2::CENTER_CENTER,
                "♫",
                FontId::proportional(rect.height() * 0.35),
                Color32::from_white_alpha(170),
            );
        }
    }
}

struct Tile<'a> {
    sound: &'a Sound,
    tex: Option<&'a egui::TextureHandle>,
    state: TileState,
    progress: Option<f32>,
    selected: bool,
}

fn sound_tile(ui: &mut egui::Ui, p: &Palette, angular: bool, size: f32, t: Tile, time: f64) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hover"), resp.hovered(), 0.12);
    let mut rect = rect.translate(vec2(0.0, -2.0 * hover));
    if resp.is_pointer_button_down_on() {
        rect = rect.shrink(2.0);
    }
    let painter = ui.painter_at(rect.expand(24.0));
    let notch = if angular { (size * 0.11).clamp(10.0, 20.0) } else { 0.0 };
    let radius: u8 = if angular { 0 } else { 10 };
    let corners_bottom = CornerRadius { nw: 0, ne: 0, sw: radius, se: radius };

    if t.progress.is_some() {
        theme::glow(&painter, rect, theme::alpha(p.accent, 0.45));
    } else if t.selected {
        theme::glow(&painter, rect, theme::alpha(p.accent, 0.16));
    }

    paint_cover(&painter, rect, t.tex, t.sound.id, radius);
    if hover > 0.0 {
        painter.rect_filled(rect, radius, Color32::from_white_alpha((10.0 * hover) as u8));
    }

    // Title band with a soft fade above it.
    let band_h = (size * 0.30).clamp(32.0, 54.0);
    let band = Rect::from_min_max(pos2(rect.left(), rect.bottom() - band_h), rect.max);
    theme::scrim(
        &painter,
        Rect::from_min_max(pos2(rect.left(), band.top() - band_h * 0.8), pos2(rect.right(), band.top())),
        theme::alpha(p.bg_deep, 0.85),
    );
    painter.rect_filled(band, corners_bottom, theme::alpha(p.bg_deep, 0.85));
    let mut job = egui::text::LayoutJob::simple(
        t.sound.title.clone(),
        theme::bold((size * 0.1).clamp(12.0, 15.0)),
        p.text,
        band.width() - 20.0,
    );
    job.wrap.max_rows = 2;
    job.wrap.overflow_character = Some('…');
    let galley = ui.painter().layout_job(job);
    painter.galley(pos2(band.left() + 10.0, band.center().y - galley.size().y / 2.0), galley, p.text);

    // Chips: shortcut first (more useful when there's only room for one), then volume.
    let mut chips = Vec::new();
    if let Some(sc) = &t.sound.shortcut {
        chips.push((sc.clone(), FontId::monospace(10.0)));
    }
    if (t.sound.volume - 1.0).abs() > 0.01 {
        chips.push((format!("{:.0}%", t.sound.volume * 100.0), FontId::proportional(10.5)));
    }
    let mut x = rect.left() + 7.0;
    for (text, font) in chips {
        let g = ui.painter().layout_no_wrap(text, font, p.text_dim);
        let r = Rect::from_min_size(pos2(x, rect.top() + 7.0), g.size() + vec2(10.0, 5.0));
        if r.right() > rect.right() - 26.0 {
            break;
        }
        painter.rect(r, radius.min(4), theme::alpha(p.bg_deep, 0.82), Stroke::new(1.0, p.border), StrokeKind::Inside);
        painter.galley(r.min + vec2(5.0, 2.5), g, p.text_dim);
        x = r.right() + 5.0;
    }

    match t.state {
        TileState::Loading => {
            painter.rect_filled(rect, radius, theme::alpha(p.bg_deep, 0.55));
            painter.text(rect.center(), Align2::CENTER_CENTER, "LOADING…", theme::bold(10.5), p.text_muted);
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
        TileState::Failed => {
            painter.rect_filled(rect, radius, theme::alpha(p.danger, 0.22));
            painter.text(rect.center(), Align2::CENTER_CENTER, "⚠ CAN'T PLAY", theme::bold(11.0), p.danger);
        }
        TileState::Ready => {}
    }

    if let Some(prog) = t.progress {
        // Equalizer bars in the top-right corner.
        let base = rect.top() + 22.0;
        for i in 0..3 {
            let h = (0.3 + 0.7 * ((time * 7.0 + i as f64 * 1.9).sin().abs() as f32)) * 12.0;
            let x = rect.right() - 12.0 - (2 - i) as f32 * 5.0;
            painter.rect_filled(Rect::from_min_max(pos2(x, base - h), pos2(x + 3.0, base)), 0, p.accent);
        }
        // Progress along the bottom edge.
        let inset = radius as f32 * 0.6;
        let track = Rect::from_min_max(
            pos2(rect.left() + inset, rect.bottom() - 3.0),
            pos2(rect.right() - inset - notch, rect.bottom()),
        );
        painter.rect_filled(track, 0, theme::alpha(p.accent, 0.22));
        let fill = Rect::from_min_size(track.min, vec2(track.width() * prog.clamp(0.0, 1.0), track.height()));
        painter.rect_filled(fill, 0, p.accent);
    }

    if angular {
        theme::cut_corner(&painter, rect, notch, p.bg);
    }
    let stroke = if t.progress.is_some() {
        Stroke::new(2.0, p.accent)
    } else if t.selected {
        Stroke::new(1.5, p.accent)
    } else if hover > 0.5 {
        Stroke::new(1.0, p.border_strong)
    } else {
        Stroke::new(1.0, p.border)
    };
    if angular {
        painter.add(egui::Shape::closed_line(theme::notched(rect.shrink(stroke.width / 2.0), notch), stroke));
    } else {
        painter.rect_stroke(rect, radius, stroke, StrokeKind::Inside);
    }
    if angular && (t.selected || hover > 0.5) {
        let tick = if t.selected { p.accent } else { p.border_strong };
        theme::corner_ticks(&painter, rect, 9.0, Stroke::new(1.5, tick));
    }

    resp.on_hover_text(format!("{}\nLeft click: play · Right click: options", t.sound.title))
}

fn add_tile(ui: &mut egui::Ui, p: &Palette, angular: bool, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let hovered = resp.hovered();
    let color = if hovered { p.accent } else { p.border_strong };
    let notch = if angular { (size * 0.11).clamp(10.0, 20.0) } else { 0.0 };
    let painter = ui.painter();
    if hovered {
        theme::card(painter, rect, angular, notch, theme::alpha(p.accent, 0.06), Stroke::NONE);
    }
    let r = rect.shrink(1.0);
    let mut pts = if angular {
        theme::notched(r, notch)
    } else {
        vec![r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()]
    };
    pts.push(pts[0]);
    painter.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.5, color), 6.0, 4.0));
    painter.text(
        rect.center() - vec2(0.0, size * 0.06),
        Align2::CENTER_CENTER,
        "+",
        FontId::proportional(size * 0.32),
        color,
    );
    painter.text(
        rect.center() + vec2(0.0, size * 0.24),
        Align2::CENTER_CENTER,
        "ADD SOUND",
        theme::bold(10.5),
        if hovered { p.accent } else { p.text_muted },
    );
    resp.on_hover_text("Add audio files (mp3, wav, ogg, flac, …)")
}
