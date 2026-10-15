//! egui user interface: the sound grid, the editor panel and routing settings.

use crate::audio::{self, AudioHandle, Cmd, Device};
use crate::config::{self, Config, Sound};
use crate::decode::{self, Clip, DecodeResult};
use crate::remote::{self, Press, RemoteSound, SharedLibrary};
use crate::shell::{Shell, ShellEvent};
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
}

impl SoundboardApp {
    pub fn new(shell: Shell, events: Receiver<ShellEvent>) -> Self {
        let cfg = Config::load();

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
        };
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

    // ---------------------------------------------------------------- UI sections

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("♫ Soundboard");
            ui.add_space(12.0);

            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("🔍 Search sounds…")
                    .desired_width(180.0),
            );
            if !self.search.is_empty() && ui.small_button("✖").clicked() {
                self.search.clear();
            }
            ui.separator();

            let st = &mut self.cfg.settings;
            let mut changed = false;
            changed |= ui
                .toggle_value(&mut st.play_to_speakers, "🔊 Speakers")
                .on_hover_text("Play sounds on your speakers/headphones")
                .changed();
            changed |= ui
                .toggle_value(&mut st.play_to_mic, "🎤 Mic")
                .on_hover_text("Play sounds into “Soundboard Virtual Mic”")
                .changed();
            ui.separator();

            ui.label("Volume");
            let r = ui.add(
                egui::Slider::new(&mut st.master_volume, 0.0..=2.0)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
            );
            if r.changed() {
                self.audio.send(Cmd::SetMaster(st.master_volume));
                changed = true;
            }
            if changed {
                self.mark_dirty();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.toggle_value(&mut self.show_settings, "⚙ Routing")
                    .on_hover_text("Output device, microphone passthrough and behaviour");
                if ui.button("⏹ Stop all").clicked() {
                    self.audio.send(Cmd::StopAll);
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let status = self.audio.status.lock().unwrap();
        ui.horizontal(|ui| {
            if let Some(err) = &status.error {
                ui.colored_label(Color32::from_rgb(230, 90, 80), format!("⚠ {err}"));
            } else if status.connected {
                ui.colored_label(Color32::from_rgb(110, 200, 120), "✔ PipeWire connected");
                ui.separator();
                match &self.cfg.settings.mic_output {
                    None => {
                        let mic = if status.mic_in_use {
                            "🎤 Virtual mic: in use"
                        } else {
                            "🎤 Virtual mic: no app listening"
                        };
                        ui.label(mic).on_hover_text(
                            "Select “Soundboard Virtual Mic” as the input device in Discord, OBS, \
                             your browser, etc. Sounds are only sent to the mic while an app is listening.",
                        );
                    }
                    Some(name) => {
                        let desc = status
                            .mic_targets
                            .iter()
                            .find(|d| &d.name == name)
                            .map_or(name.as_str(), |d| d.description.as_str());
                        if !status.mic_output_linked {
                            ui.colored_label(
                                Color32::from_rgb(230, 170, 80),
                                format!("🎤 {desc}: not available"),
                            )
                            .on_hover_text(
                                "The selected mic output is not running (e.g. EasyEffects is closed), \
                                 so sounds are not reaching any microphone.",
                            );
                        } else if status.mic_in_use {
                            ui.label(format!("🎤 {desc}: in use"));
                        } else {
                            ui.label(format!("🎤 {desc}: no app listening"));
                        }
                    }
                }
                if self.cfg.settings.mic_passthrough && self.cfg.settings.mic_output.is_none() {
                    ui.separator();
                    ui.label(format!("Passthrough: {}", status.capture_state));
                }
            } else {
                ui.label("Connecting to PipeWire…");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{} sounds", self.cfg.sounds.len()));
            });
        });
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        let (sinks, sources, mic_targets) = {
            let s = self.audio.status.lock().unwrap();
            (s.sinks.clone(), s.sources.clone(), s.mic_targets.clone())
        };
        ui.heading("Routing");
        ui.add_space(4.0);

        ui.label("Speaker output");
        let mut target = self.cfg.settings.speaker_target.clone();
        if device_combo(ui, "spk_combo", "System default", &mut target, &sinks) {
            self.cfg.settings.speaker_target = target.clone();
            self.audio.send(Cmd::SetSpeakerTarget(target));
            self.mark_dirty();
        }

        ui.add_space(8.0);
        ui.label("Microphone output");
        let mut out = self.cfg.settings.mic_output.clone();
        if device_combo(ui, "mic_out_combo", "Soundboard Virtual Mic", &mut out, &mic_targets) {
            self.cfg.settings.mic_output = out.clone();
            self.audio.send(Cmd::SetMicOutput(out));
            self.mark_dirty();
        }
        if self.cfg.settings.mic_output.is_some() {
            ui.small(
                "Sounds are mixed into this microphone, after its effects. Apps that already \
                 use it hear them without changing their input device. Your voice is already \
                 in it, so passthrough is not needed.",
            );
        } else {
            self.passthrough_section(ui, &sources);
        }

        ui.add_space(10.0);
        ui.separator();
        self.behaviour_section(ui);
    }

    fn passthrough_section(&mut self, ui: &mut egui::Ui, sources: &[Device]) {
        ui.small(
            "Apps see a microphone called “Soundboard Virtual Mic”. Pick it as the input \
             device in your voice/streaming app.",
        );
        ui.add_space(4.0);

        let st = &mut self.cfg.settings;
        let mut pass_changed = ui
            .checkbox(&mut st.mic_passthrough, "Mix my real microphone into it")
            .changed();
        ui.add_enabled_ui(st.mic_passthrough, |ui| {
            ui.horizontal(|ui| {
                ui.label("Mic level");
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
            ui.label("Real microphone");
            let mut src = self.cfg.settings.mic_source.clone();
            if device_combo(ui, "mic_combo", "System default", &mut src, sources) {
                self.cfg.settings.mic_source = src.clone();
                self.audio.send(Cmd::SetMicSource(src));
                self.mark_dirty();
            }
        });
    }

    fn behaviour_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Behaviour");
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
        ui.horizontal(|ui| {
            ui.label("Tile size");
            changed |= ui.add(egui::Slider::new(&mut st.tile_size, 80.0..=220.0)).changed();
        });
        let tray = self.tray_available;
        ui.add_enabled_ui(tray, |ui| {
            changed |= ui
                .checkbox(&mut st.close_to_tray, "Keep running in the tray when closed")
                .on_hover_text("Sounds, the virtual mic and StreamController keep working")
                .on_disabled_hover_text("Your desktop has no system tray")
                .changed();
            ui.add_enabled_ui(st.close_to_tray, |ui| {
                changed |= ui
                    .checkbox(&mut st.start_hidden, "Start hidden in the tray")
                    .changed();
            });
        });
        if changed {
            self.mark_dirty();
        }

        ui.add_space(10.0);
        ui.separator();
        ui.small(format!("Library: {}", config::config_path().display()));
    }

    fn editor_panel(&mut self, ui: &mut egui::Ui, id: u64) {
        let ctx = ui.ctx().clone();
        let Some(sound) = self.sound(id).cloned() else {
            self.selected = None;
            return;
        };

        ui.horizontal(|ui| {
            ui.heading("Edit sound");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✖").on_hover_text("Close").clicked() {
                    self.selected = None;
                }
            });
        });
        ui.add_space(6.0);

        // Thumbnail preview / picker
        let side = ui.available_width().min(200.0);
        let (rect, resp) = ui.allocate_exact_size(vec2(side, side), Sense::click());
        let tex = sound.thumbnail.as_ref().and_then(|p| self.texture(&ctx, p));
        paint_cover(ui, rect, tex.as_ref(), id, 8);
        if resp.hovered() {
            ui.painter().rect_filled(rect, 8, Color32::from_black_alpha(120));
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "🖼 Change image",
                FontId::proportional(15.0),
                Color32::WHITE,
            );
        }
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.open_dialog(&ctx, DialogKind::Thumbnail(id));
        }
        ui.horizontal(|ui| {
            if ui.button("🖼 Image…").clicked() {
                self.open_dialog(&ctx, DialogKind::Thumbnail(id));
            }
            if sound.thumbnail.is_some() && ui.button("Remove image").clicked() {
                if let Some(s) = self.sound_mut(id)
                    && let Some(old) = s.thumbnail.take() {
                        config::remove_owned_thumbnail(&old);
                    }
                self.mark_dirty();
            }
        });
        ui.small("Tip: drop an image onto the window to use it here.");
        ui.add_space(8.0);

        ui.label("Title");
        let mut title = sound.title.clone();
        if ui
            .add(egui::TextEdit::singleline(&mut title).desired_width(f32::INFINITY))
            .changed()
        {
            if let Some(s) = self.sound_mut(id) {
                s.title = title;
            }
            self.mark_dirty();
        }

        ui.add_space(6.0);
        ui.label("Volume");
        let mut vol = sound.volume;
        if ui
            .add(
                egui::Slider::new(&mut vol, 0.0..=2.0)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
            )
            .changed()
        {
            if let Some(s) = self.sound_mut(id) {
                s.volume = vol;
            }
            self.audio.send(Cmd::SetVoiceGain { id, gain: vol });
            self.mark_dirty();
        }

        ui.add_space(6.0);
        ui.label("Audio file");
        ui.add(
            egui::Label::new(egui::RichText::new(sound.path.display().to_string()).small())
                .wrap(),
        );
        match self.clips.get(&id) {
            Some(ClipState::Ready(c)) => {
                ui.small(format!("Duration: {:.1} s", c.seconds));
            }
            Some(ClipState::Loading) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.small("Decoding…");
                });
            }
            Some(ClipState::Failed(e)) => {
                ui.colored_label(Color32::from_rgb(230, 90, 80), format!("⚠ {e}"));
            }
            None => {}
        }
        if ui.button("📂 Replace audio…").clicked() {
            self.open_dialog(&ctx, DialogKind::ReplaceAudio(id));
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let playing = self.audio.status.lock().unwrap().progress(id).is_some();
            if playing {
                if ui.button("⏹ Stop").clicked() {
                    self.audio.send(Cmd::Stop(id));
                }
            } else if ui.button("▶ Play").clicked() {
                self.play(id);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(egui::RichText::new("🗑 Remove").color(Color32::from_rgb(230, 100, 90)))
                    .clicked()
                {
                    self.pending_delete = Some(id);
                }
            });
        });
    }

    fn grid(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
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

        let mut clicked = None;
        let mut action: Option<(u64, TileAction)> = None;

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
                for s in &visible {
                    let tex = s.thumbnail.as_ref().and_then(|p| self.texture(&ctx, p));
                    let state = match self.clips.get(&s.id) {
                        Some(ClipState::Ready(_)) => TileState::Ready,
                        Some(ClipState::Failed(_)) => TileState::Failed,
                        _ => TileState::Loading,
                    };
                    let resp = sound_tile(
                        ui,
                        size,
                        s,
                        tex.as_ref(),
                        state,
                        progress.get(&s.id).copied(),
                        self.selected == Some(s.id),
                    );
                    if resp.clicked() {
                        clicked = Some(s.id);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("▶ Play").clicked() {
                            action = Some((s.id, TileAction::Play));
                        }
                        if ui.button("⏹ Stop").clicked() {
                            action = Some((s.id, TileAction::Stop));
                        }
                        ui.separator();
                        if ui.button("✏ Edit…").clicked() {
                            action = Some((s.id, TileAction::Edit));
                        }
                        if ui.button("🖼 Set image…").clicked() {
                            action = Some((s.id, TileAction::Image));
                        }
                        ui.separator();
                        if ui.button("🗑 Remove").clicked() {
                            action = Some((s.id, TileAction::Remove));
                        }
                    });
                }
                if query.is_empty() && add_tile(ui, size).clicked() {
                    action = Some((0, TileAction::Add));
                }
            });

            if self.cfg.sounds.is_empty() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(
                            "Your soundboard is empty.\nClick ➕ or drag audio files onto this window.",
                        )
                        .size(16.0)
                        .weak(),
                    );
                });
            } else if visible.is_empty() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| ui.weak(format!("No sounds match “{}”", self.search)));
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
        let title = self.sound(id).map(|s| s.title.clone()).unwrap_or_default();
        let mut decision = None;
        egui::Modal::new(egui::Id::new("confirm_delete")).show(ctx, |ui| {
            ui.set_width(280.0);
            ui.heading("Remove sound?");
            ui.label(format!("“{title}” will be removed from the board. The audio file itself is not deleted."));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Remove").clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
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
        self.handle_shell_events(&ctx);
        self.poll_background();
        self.handle_drops(&ctx);

        // Keyboard: Delete removes the selected sound, Escape stops everything.
        if ctx.memory(|m| m.focused().is_none()) {
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && self.pending_delete.is_none() {
                self.audio.send(Cmd::StopAll);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
                self.pending_delete = self.selected;
            }
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            self.top_bar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(2.0);
            self.status_bar(ui);
        });

        if self.show_settings || self.selected.is_some() {
            egui::Panel::right("side")
                .resizable(true)
                .default_size(270.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.add_space(6.0);
                        if let Some(id) = self.selected {
                            self.editor_panel(ui, id);
                            if self.show_settings {
                                ui.add_space(10.0);
                                ui.separator();
                            }
                        }
                        if self.show_settings {
                            self.settings_panel(ui);
                        }
                    });
                });
        }

        egui::CentralPanel::default().show(ui, |ui| self.grid(ui));

        // Drag-and-drop hint overlay
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop_overlay"),
            ));
            painter.rect_filled(screen, 0, Color32::from_black_alpha(160));
            painter.text(
                screen.center(),
                Align2::CENTER_CENTER,
                "Drop audio files to add them\n(drop an image to set the selected sound's thumbnail)",
                FontId::proportional(20.0),
                Color32::WHITE,
            );
        }

        self.delete_dialog(&ctx);

        if let Some((msg, at)) = &self.toast {
            if at.elapsed() < Duration::from_secs(3) {
                egui::Area::new(egui::Id::new("toast"))
                    .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -40.0))
                    .show(&ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(msg.as_str()));
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

/// Deterministic pleasant colour for tiles without a thumbnail.
fn tile_color(id: u64) -> Color32 {
    let h = (id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as f32 / (1u64 << 24) as f32;
    let hsva = egui::ecolor::Hsva::new(h, 0.45, 0.55, 1.0);
    hsva.into()
}

/// Paint `tex` cropped to fill `rect` (like CSS `object-fit: cover`), or a placeholder.
fn paint_cover(ui: &egui::Ui, rect: Rect, tex: Option<&egui::TextureHandle>, id: u64, radius: u8) {
    match tex {
        Some(tex) => {
            let [w, h] = tex.size();
            let (iw, ih) = (w as f32, h as f32);
            let target = rect.width() / rect.height();
            let src = iw / ih;
            let uv = if src > target {
                let f = target / src;
                Rect::from_min_max(pos2((1.0 - f) / 2.0, 0.0), pos2((1.0 + f) / 2.0, 1.0))
            } else {
                let f = src / target;
                Rect::from_min_max(pos2(0.0, (1.0 - f) / 2.0), pos2(1.0, (1.0 + f) / 2.0))
            };
            egui::Image::new((tex.id(), rect.size()))
                .uv(uv)
                .corner_radius(radius)
                .paint_at(ui, rect);
        }
        None => {
            ui.painter().rect_filled(rect, radius, tile_color(id));
            ui.painter().text(
                rect.center() - vec2(0.0, rect.height() * 0.08),
                Align2::CENTER_CENTER,
                "♫",
                FontId::proportional(rect.height() * 0.35),
                Color32::from_white_alpha(180),
            );
        }
    }
}

fn sound_tile(
    ui: &mut egui::Ui,
    size: f32,
    sound: &Sound,
    tex: Option<&egui::TextureHandle>,
    state: TileState,
    progress: Option<f32>,
    selected: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let radius: u8 = 10;
    let hovered = resp.hovered();
    let pressed = resp.is_pointer_button_down_on();
    let rect = if pressed { rect.shrink(2.0) } else { rect };
    let painter = ui.painter_at(rect.expand(3.0));

    paint_cover(ui, rect, tex, sound.id, radius);

    // Title band at the bottom
    let band_h = (size * 0.30).clamp(28.0, 52.0);
    let band = Rect::from_min_max(pos2(rect.left(), rect.bottom() - band_h), rect.max);
    painter.rect_filled(
        band,
        CornerRadius {
            nw: 0,
            ne: 0,
            sw: radius,
            se: radius,
        },
        Color32::from_black_alpha(170),
    );
    let mut job = egui::text::LayoutJob::simple(
        sound.title.clone(),
        FontId::proportional((size * 0.105).clamp(11.0, 15.0)),
        Color32::WHITE,
        band.width() - 10.0,
    );
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('…');
    job.halign = egui::Align::Center;
    let galley = ui.painter().layout_job(job);
    let text_pos = pos2(band.center().x, band.center().y - galley.size().y / 2.0);
    painter.galley(text_pos, galley, Color32::WHITE);

    // Volume badge when not 100 %
    if (sound.volume - 1.0).abs() > 0.01 {
        let badge = format!("{:.0}%", sound.volume * 100.0);
        let pos = rect.left_top() + vec2(6.0, 6.0);
        let g = ui
            .painter()
            .layout_no_wrap(badge, FontId::proportional(11.0), Color32::WHITE);
        let r = Rect::from_min_size(pos, g.size() + vec2(8.0, 4.0));
        painter.rect_filled(r, 4, Color32::from_black_alpha(150));
        painter.galley(pos + vec2(4.0, 2.0), g, Color32::WHITE);
    }

    match state {
        TileState::Loading => {
            painter.rect_filled(rect, radius, Color32::from_black_alpha(90));
            painter.text(rect.center(), Align2::CENTER_CENTER, "…", FontId::proportional(28.0), Color32::WHITE);
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
        TileState::Failed => {
            painter.rect_filled(rect, radius, Color32::from_rgba_unmultiplied(120, 0, 0, 110));
            painter.text(
                rect.right_top() + vec2(-14.0, 14.0),
                Align2::CENTER_CENTER,
                "⚠",
                FontId::proportional(18.0),
                Color32::from_rgb(255, 200, 80),
            );
        }
        TileState::Ready => {}
    }

    // Playback progress: tinted overlay + bar on top of the title band.
    if let Some(p) = progress {
        painter.rect_filled(rect, radius, Color32::from_rgba_unmultiplied(40, 200, 120, 40));
        let bar = Rect::from_min_size(
            pos2(rect.left(), band.top() - 4.0),
            vec2(rect.width() * p.clamp(0.0, 1.0), 4.0),
        );
        painter.rect_filled(bar, 0, Color32::from_rgb(60, 220, 130));
        painter.text(
            rect.right_top() + vec2(-14.0, 14.0),
            Align2::CENTER_CENTER,
            "🔊",
            FontId::proportional(16.0),
            Color32::WHITE,
        );
    }

    let stroke = if selected {
        Stroke::new(3.0, ui.visuals().selection.stroke.color)
    } else if hovered {
        Stroke::new(2.0, Color32::from_white_alpha(140))
    } else {
        Stroke::new(1.0, Color32::from_white_alpha(30))
    };
    painter.rect_stroke(rect, radius, stroke, StrokeKind::Inside);

    resp.on_hover_text(format!(
        "{}\nLeft click: play · Right click: options",
        sound.title
    ))
}

fn add_tile(ui: &mut egui::Ui, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let hovered = resp.hovered();
    let color = if hovered {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    ui.painter()
        .rect_filled(rect, 10, ui.visuals().faint_bg_color);
    ui.painter().rect_stroke(
        rect.shrink(1.0),
        10,
        Stroke::new(if hovered { 2.0 } else { 1.5 }, color),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center() - vec2(0.0, 10.0),
        Align2::CENTER_CENTER,
        "+",
        FontId::proportional(size * 0.35),
        color,
    );
    ui.painter().text(
        rect.center() + vec2(0.0, size * 0.22),
        Align2::CENTER_CENTER,
        "Add sound",
        FontId::proportional(13.0),
        color,
    );
    resp.on_hover_text("Add audio files (mp3, wav, ogg, flac, …)")
}
