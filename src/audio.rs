//! Native PipeWire audio engine.
//!
//! Runs a PipeWire main loop on its own thread and owns three streams:
//!
//! * **speakers** – a normal playback stream (`Stream/Output/Audio`) that auto-connects to the
//!   default sink, or to the sink chosen in the UI. This is how *you* hear the sounds.
//! * **virtual mic** – a `support.null-audio-sink` node with `media.class = Audio/Source/Virtual`
//!   ("Soundboard Virtual Mic"), owned by this client and removed when the app exits. Voice apps
//!   such as Discord, OBS, Zoom or a browser select it as their microphone. A dedicated playback
//!   stream (the *mic feed*) is pinned to that node and carries the sounds (+ optional mic).
//!   Alternatively the feed can be linked into another *virtual* source that has input ports,
//!   e.g. EasyEffects' "Easy Effects Source", so apps already using it hear the sounds without
//!   switching devices (the *mic output* setting).
//! * **mic capture** (optional) – captures your real microphone and mixes it into the virtual
//!   mic, so people hear both your voice and the sounds through one device.
//!
//! All streams run on the PipeWire main-loop thread (no RT_PROCESS), so they can share state
//! through `Rc<RefCell<..>>` without locks. The UI talks to this thread through a
//! `pipewire::channel`, and reads device lists / playback progress through a `Mutex`.

use pipewire as pw;
use pw::properties::{PropertiesBox, properties};
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use spa::pod::Pod;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
const STRIDE: usize = std::mem::size_of::<f32>() * CHANNELS as usize;

pub const SPEAKER_NODE: &str = "pw_soundboard_playback";
pub const VMIC_NODE: &str = "pw_soundboard_virtual_mic";
pub const FEED_NODE: &str = "pw_soundboard_mic_feed";
pub const CAPTURE_NODE: &str = "pw_soundboard_mic_capture";
const OWN_NODES: &[&str] = &[SPEAKER_NODE, VMIC_NODE, FEED_NODE, CAPTURE_NODE];

/// Keep at most ~60 ms of captured mic audio buffered; older audio is dropped so that clock
/// drift between the mic and the virtual-mic consumer never builds up latency.
const MIC_BUFFER_MAX: usize = (SAMPLE_RATE as usize * CHANNELS as usize) * 60 / 1000;

/// Commands sent from the UI to the audio thread.
pub enum Cmd {
    Play {
        id: u64,
        samples: Arc<Vec<f32>>,
        gain: f32,
        to_speakers: bool,
        to_mic: bool,
        exclusive: bool,
    },
    Stop(u64),
    StopAll,
    SetMaster(f32),
    /// Change the volume of a sound while it is playing.
    SetVoiceGain { id: u64, gain: f32 },
    SetSpeakerTarget(Option<String>),
    SetMicSource(Option<String>),
    SetPassthrough { enabled: bool, gain: f32 },
    /// Node the mic feed is linked into. `None` = our own virtual mic.
    SetMicOutput(Option<String>),
    Quit,
    /// Internal: the virtual-mic node appeared in the registry, attach the feed stream.
    #[doc(hidden)]
    VmicReady,
    /// Internal: the session manager's default microphone changed.
    #[doc(hidden)]
    DefaultSource(Option<String>),
    /// Internal: a microphone appeared or disappeared.
    #[doc(hidden)]
    SourcesChanged,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: u32,
    /// `node.name` – stable identifier used as `target.object`.
    pub name: String,
    /// Human readable `node.description`.
    pub description: String,
}

/// State shared with the UI thread.
#[derive(Debug, Default)]
pub struct Status {
    pub connected: bool,
    pub error: Option<String>,
    pub sinks: Vec<Device>,
    pub sources: Vec<Device>,
    /// sound id -> playback progress 0..1 (speaker side)
    pub playing_speakers: HashMap<u64, f32>,
    /// sound id -> playback progress 0..1 (virtual-mic side)
    pub playing_mic: HashMap<u64, f32>,
    pub speaker_state: String,
    /// True while some application is recording from the mic output node.
    pub mic_in_use: bool,
    /// True while the mic feed is actually linked into the mic output node.
    pub mic_output_linked: bool,
    /// Other virtual sources the mic feed can be linked into (they have input ports).
    pub mic_targets: Vec<Device>,
    pub capture_state: String,
}

impl Status {
    pub fn progress(&self, id: u64) -> Option<f32> {
        match (self.playing_speakers.get(&id), self.playing_mic.get(&id)) {
            (Some(a), Some(b)) => Some(a.max(*b)),
            (Some(a), None) | (None, Some(a)) => Some(*a),
            (None, None) => None,
        }
    }
    pub fn anything_playing(&self) -> bool {
        !self.playing_speakers.is_empty() || !self.playing_mic.is_empty()
    }
}

pub type SharedStatus = Arc<Mutex<Status>>;

/// Handle owned by the UI. Dropping it stops the audio thread.
pub struct AudioHandle {
    tx: pw::channel::Sender<Cmd>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub status: SharedStatus,
}

impl AudioHandle {
    pub fn send(&self, cmd: Cmd) {
        // If the audio thread died the error is already shown via `status.error`.
        let _ = self.tx.send(cmd);
    }

    /// A sender usable from other threads (e.g. the control socket).
    pub fn sender(&self) -> pw::channel::Sender<Cmd> {
        self.tx.clone()
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct InitialRouting {
    pub master: f32,
    pub speaker_target: Option<String>,
    pub mic_source: Option<String>,
    pub passthrough: bool,
    pub passthrough_gain: f32,
    /// `node.name` the mic feed goes into. `None` = our own virtual mic.
    pub mic_output: Option<String>,
}

pub fn start(init: InitialRouting, repaint: impl Fn() + Send + 'static) -> AudioHandle {
    let status: SharedStatus = Arc::new(Mutex::new(Status::default()));
    let (tx, rx) = pw::channel::channel::<Cmd>();
    let st = status.clone();
    let self_tx = tx.clone();
    let thread = std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            let repaint: Rc<dyn Fn()> = Rc::new(repaint);
            if let Err(e) = run(rx, self_tx, init, st.clone(), repaint.clone()) {
                let mut s = st.lock().unwrap();
                s.connected = false;
                s.error = Some(format!(
                    "PipeWire error: {e}. Is the PipeWire server running? (systemctl --user status pipewire)"
                ));
                drop(s);
                repaint();
            }
        })
        .expect("spawn pipewire thread");
    AudioHandle {
        tx,
        thread: Some(thread),
        status,
    }
}

// ---------------------------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------------------------

struct Voice {
    id: u64,
    data: Arc<Vec<f32>>,
    /// Position in interleaved samples.
    pos: usize,
    gain: f32,
}

#[derive(Default)]
struct Mixer {
    voices: Vec<Voice>,
}

impl Mixer {
    fn mix_into(&mut self, out: &mut [f32], master: f32) {
        for v in &mut self.voices {
            let remaining = v.data.len().saturating_sub(v.pos);
            let n = remaining.min(out.len());
            let g = v.gain * master;
            for (o, s) in out[..n].iter_mut().zip(&v.data[v.pos..v.pos + n]) {
                *o += s * g;
            }
            v.pos += n;
        }
        self.voices.retain(|v| v.pos < v.data.len());
    }

    fn report(&self, into: &mut HashMap<u64, f32>) {
        into.clear();
        for v in &self.voices {
            let p = v.pos as f32 / v.data.len().max(1) as f32;
            let e = into.entry(v.id).or_insert(0.0);
            *e = e.max(p);
        }
    }

    fn stop(&mut self, id: u64) {
        self.voices.retain(|v| v.id != id);
    }

    fn set_gain(&mut self, id: u64, gain: f32) {
        self.voices.iter_mut().filter(|v| v.id == id).for_each(|v| v.gain = gain);
    }
}

struct Engine {
    master: f32,
    speakers: Mixer,
    mic: Mixer,
    passthrough: bool,
    passthrough_gain: f32,
    mic_in: VecDeque<f32>,
    /// The virtual mic is only scheduled while some app records from it. Sounds triggered while
    /// nobody listens must not queue up and play later, so we only add mic voices while active.
    vmic_active: bool,
}

/// Soft limiter: transparent below ~0.8, smoothly saturates above so overlapping loud clips
/// distort gently instead of hard-clipping.
#[inline]
fn soft_clip(x: f32) -> f32 {
    const T: f32 = 0.8;
    let a = x.abs();
    if a <= T {
        x
    } else {
        let over = (a - T) / (1.0 - T);
        x.signum() * (T + (1.0 - T) * (over / (1.0 + over)))
    }
}

// ---------------------------------------------------------------------------------------------
// PipeWire plumbing
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Speakers,
    VirtualMic,
    Capture,
}

struct StreamData {
    kind: Kind,
    engine: Rc<RefCell<Engine>>,
    status: SharedStatus,
    repaint: Rc<dyn Fn()>,
    scratch: Vec<f32>,
    was_playing: bool,
}

/// Field order matters: the listener must be dropped before the stream.
struct StreamSlot {
    _listener: StreamListener<StreamData>,
    _stream: StreamRc,
}

#[derive(Default)]
struct Streams {
    speakers: Option<StreamSlot>,
    vmic: Option<StreamSlot>,
    capture: Option<StreamSlot>,
}

fn format_params() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .expect("serialize audio format")
    .0
    .into_inner()
}

fn state_name(s: &StreamState) -> String {
    match s {
        StreamState::Error(e) => format!("error: {e}"),
        StreamState::Unconnected => "unconnected".into(),
        StreamState::Connecting => "connecting".into(),
        StreamState::Paused => "idle".into(),
        StreamState::Streaming => "streaming".into(),
    }
}

fn make_stream(
    core: &pw::core::CoreRc,
    kind: Kind,
    target: Option<&str>,
    engine: &Rc<RefCell<Engine>>,
    status: &SharedStatus,
    repaint: &Rc<dyn Fn()>,
) -> Result<StreamSlot, pw::Error> {
    let (name, mut props, direction, flags): (&str, PropertiesBox, _, _) = match kind {
        Kind::Speakers => (
            "soundboard-playback",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Music",
                *pw::keys::NODE_NAME => SPEAKER_NODE,
                *pw::keys::NODE_DESCRIPTION => "Soundboard (speakers)",
                *pw::keys::APP_NAME => "PW Soundboard",
                *pw::keys::NODE_LATENCY => "512/48000",
            },
            spa::utils::Direction::Output,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        ),
        Kind::VirtualMic => (
            "soundboard-mic-feed",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::NODE_NAME => FEED_NODE,
                *pw::keys::NODE_DESCRIPTION => "Soundboard (virtual mic feed)",
                *pw::keys::APP_NAME => "PW Soundboard",
                *pw::keys::NODE_LATENCY => "512/48000",
            },
            spa::utils::Direction::Output,
            // Not auto-connected: session managers only route playback streams to *sinks*,
            // so we link this stream to the virtual mic's input ports ourselves (see `Patch`).
            StreamFlags::MAP_BUFFERS,
        ),
        Kind::Capture => (
            "soundboard-mic-capture",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::NODE_NAME => CAPTURE_NODE,
                *pw::keys::NODE_DESCRIPTION => "Soundboard (mic passthrough)",
                *pw::keys::APP_NAME => "PW Soundboard",
                *pw::keys::NODE_LATENCY => "512/48000",
                // We always pass an explicit target (never our own virtual mic). If it goes
                // away we pick a new one ourselves instead of letting the session manager
                // fall back to a default that might be the virtual mic (feedback loop).
                "node.dont-fallback" => "true",
                "node.dont-reconnect" => "true",
            },
            spa::utils::Direction::Input,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        ),
    };
    if let Some(t) = target {
        props.insert(*pw::keys::TARGET_OBJECT, t);
    }

    let stream = StreamRc::new(core.clone(), name, props)?;
    let data = StreamData {
        kind,
        engine: engine.clone(),
        status: status.clone(),
        repaint: repaint.clone(),
        scratch: Vec::new(),
        was_playing: false,
    };

    let listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, d, _old, new| on_state(d, &new))
        .process(on_process)
        .register()?;

    let values = format_params();
    let mut params = [Pod::from_bytes(&values).expect("valid pod")];
    stream.connect(direction, None, flags, &mut params)?;

    Ok(StreamSlot {
        _listener: listener,
        _stream: stream,
    })
}

fn on_state(d: &mut StreamData, new: &StreamState) {
    let streaming = matches!(new, StreamState::Streaming);
    {
        let mut s = d.status.lock().unwrap();
        match d.kind {
            Kind::Speakers => s.speaker_state = state_name(new),
            Kind::VirtualMic => {
                if !streaming {
                    s.playing_mic.clear();
                }
            }
            Kind::Capture => s.capture_state = state_name(new),
        }
    }
    if d.kind == Kind::VirtualMic {
        // State callbacks can fire while the command handler is busy; never panic on re-borrow.
        if let Ok(mut e) = d.engine.try_borrow_mut() {
            e.vmic_active = streaming;
            if !streaming {
                e.mic.voices.clear();
            }
        }
    }
    (d.repaint)();
}

fn on_process(stream: &pw::stream::Stream, d: &mut StreamData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = buffer.requested() as usize;
    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }
    let data = &mut datas[0];
    let Ok(mut engine) = d.engine.try_borrow_mut() else {
        return;
    };

    if d.kind == Kind::Capture {
        let chunk = data.chunk();
        let offset = chunk.offset() as usize;
        let size = chunk.size() as usize;
        if let Some(bytes) = data.data() {
            let end = (offset + size).min(bytes.len());
            if engine.passthrough && offset < end {
                for b in bytes[offset..end].as_chunks::<4>().0 {
                    engine.mic_in.push_back(f32::from_le_bytes(*b));
                }
                let len = engine.mic_in.len();
                if len > MIC_BUFFER_MAX {
                    // Drop whole frames from the front to keep L/R aligned.
                    let excess = (len - MIC_BUFFER_MAX / 2) & !1;
                    engine.mic_in.drain(..excess);
                }
            }
        }
        return;
    }

    let Some(bytes) = data.data() else { return };
    let mut frames = bytes.len() / STRIDE;
    if requested > 0 {
        frames = frames.min(requested);
    }
    let n = frames * CHANNELS as usize;
    d.scratch.clear();
    d.scratch.resize(n, 0.0);

    let master = engine.master;
    let playing_now;
    match d.kind {
        Kind::Speakers => {
            engine.speakers.mix_into(&mut d.scratch, master);
            playing_now = !engine.speakers.voices.is_empty();
            if playing_now || d.was_playing {
                let mut s = d.status.lock().unwrap();
                engine.speakers.report(&mut s.playing_speakers);
            }
        }
        Kind::VirtualMic => {
            engine.vmic_active = true;
            if engine.passthrough {
                let g = engine.passthrough_gain;
                let take = n.min(engine.mic_in.len());
                for (o, s) in d.scratch.iter_mut().zip(engine.mic_in.drain(..take)) {
                    *o = s * g;
                }
            }
            engine.mic.mix_into(&mut d.scratch, master);
            playing_now = !engine.mic.voices.is_empty();
            if playing_now || d.was_playing {
                let mut s = d.status.lock().unwrap();
                engine.mic.report(&mut s.playing_mic);
            }
        }
        Kind::Capture => unreachable!(),
    }
    drop(engine);

    for (dst, s) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(&d.scratch) {
        *dst = soft_clip(*s).to_le_bytes();
    }
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = STRIDE as _;
    *chunk.size_mut() = (frames * STRIDE) as _;

    if d.was_playing && !playing_now {
        (d.repaint)(); // let the UI clear the progress bar promptly
    }
    d.was_playing = playing_now;
}

struct NodeInfo {
    name: String,
    description: String,
    class: String,
}

struct PortInfo {
    node: u32,
    output: bool,
    channel: String,
}

/// Manual patching of the mic-feed stream into the *mic output* node: our own virtual mic, or
/// another virtual source with input ports (e.g. "Easy Effects Source"). Mirrors the parts of
/// the graph (nodes, ports, links) needed for that.
struct Patch {
    /// `node.name` the feed is linked into.
    target: String,
    nodes: HashMap<u32, NodeInfo>,
    ports: HashMap<u32, PortInfo>,
    /// Every link in the graph: id -> (output node, input node).
    graph_links: HashMap<u32, (u32, u32)>,
    /// Links we created. Leftovers from a previous target are destroyed through the registry.
    links: Vec<pw::link::Link>,
}

impl Patch {
    fn new(target: String) -> Self {
        Self {
            target,
            nodes: HashMap::new(),
            ports: HashMap::new(),
            graph_links: HashMap::new(),
            links: Vec::new(),
        }
    }

    fn node_id(&self, name: &str) -> Option<u32> {
        self.nodes.iter().find(|(_, n)| n.name == name).map(|(id, _)| *id)
    }

    fn is_own(&self, node: u32) -> bool {
        self.nodes.get(&node).is_some_and(|n| OWN_NODES.contains(&n.name.as_str()))
    }

    fn has_input_ports(&self, node: u32) -> bool {
        self.ports.values().any(|p| p.node == node && !p.output)
    }

    fn try_link(&mut self, core: &pw::core::CoreRc) {
        if !self.links.is_empty() {
            return;
        }
        let (Some(target), Some(feed)) = (self.node_id(&self.target), self.node_id(FEED_NODE)) else {
            return;
        };
        let feed_port = |ch: &str| {
            self.ports
                .iter()
                .find(|(_, p)| p.node == feed && p.output && p.channel == ch)
                .map(|(id, _)| *id)
        };
        let inputs: Vec<(u32, &str)> = self
            .ports
            .iter()
            .filter(|(_, p)| p.node == target && !p.output)
            .map(|(id, p)| (*id, p.channel.as_str()))
            .collect();
        let mut pairs = Vec::new();
        for ch in ["FL", "FR"] {
            let input = match inputs.as_slice() {
                // Mono target: both channels go into its single port.
                [(only, c)] if *c != "FL" && *c != "FR" => Some(*only),
                _ => inputs.iter().find(|(_, c)| *c == ch).map(|(id, _)| *id),
            };
            match (feed_port(ch), input) {
                (Some(o), Some(i)) => pairs.push((o, i)),
                _ => return, // wait until all ports are known
            }
        }
        for (out_port, in_port) in pairs {
            let props = properties! {
                "link.output.node" => feed.to_string(),
                "link.output.port" => out_port.to_string(),
                "link.input.node" => target.to_string(),
                "link.input.port" => in_port.to_string(),
                "object.linger" => "false",
            };
            match core.create_object::<pw::link::Link>("link-factory", &props) {
                Ok(link) => self.links.push(link),
                Err(e) => eprintln!("soundboard: could not link mic feed: {e}"),
            }
        }
    }

    /// A global disappeared; drop bookkeeping so links are rebuilt if the ports come back
    /// (e.g. EasyEffects restarting).
    fn forget(&mut self, id: u32) {
        let watched = [self.node_id(&self.target), self.node_id(FEED_NODE)];
        self.graph_links.remove(&id);
        let node = match self.ports.remove(&id) {
            Some(p) => Some(p.node),
            None => self.nodes.remove(&id).map(|_| id),
        };
        if node.is_some_and(|n| watched.contains(&Some(n))) {
            self.links.clear();
        }
    }

    /// Links from the feed into anything other than the current target.
    fn stray_feed_links(&self) -> Vec<u32> {
        let feed = self.node_id(FEED_NODE);
        let target = self.node_id(&self.target);
        self.graph_links
            .iter()
            .filter(|(_, (o, i))| Some(*o) == feed && Some(*i) != target)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Publish mic-output state to the UI. Returns true if anything changed.
    fn sync(&self, status: &SharedStatus) -> bool {
        let target = self.node_id(&self.target);
        let feed = self.node_id(FEED_NODE);
        let in_use = self
            .graph_links
            .values()
            .any(|(o, i)| Some(*o) == target && !self.is_own(*i));
        let linked = self
            .graph_links
            .values()
            .any(|(o, i)| Some(*o) == feed && Some(*i) == target);
        let mut targets: Vec<Device> = self
            .nodes
            .iter()
            .filter(|(id, n)| {
                n.class.starts_with("Audio/Source")
                    && !OWN_NODES.contains(&n.name.as_str())
                    && self.has_input_ports(**id)
            })
            .map(|(id, n)| Device {
                id: *id,
                name: n.name.clone(),
                description: n.description.clone(),
            })
            .collect();
        targets.sort_by(|a, b| a.description.cmp(&b.description));

        let mut s = status.lock().unwrap();
        let changed = s.mic_in_use != in_use || s.mic_output_linked != linked || s.mic_targets != targets;
        s.mic_in_use = in_use;
        s.mic_output_linked = linked;
        s.mic_targets = targets;
        changed
    }
}

fn run(
    rx: pw::channel::Receiver<Cmd>,
    self_tx: pw::channel::Sender<Cmd>,
    init: InitialRouting,
    status: SharedStatus,
    repaint: Rc<dyn Fn()>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    // Notice if the PipeWire daemon goes away.
    let _core_listener = {
        let status = status.clone();
        let repaint = repaint.clone();
        let ml = mainloop.clone();
        core.add_listener_local()
            .error(move |id, _seq, res, message| {
                if id == pw::core::PW_ID_CORE {
                    let mut s = status.lock().unwrap();
                    s.connected = false;
                    s.error = Some(format!("Lost connection to PipeWire ({res}): {message}"));
                    drop(s);
                    repaint();
                    ml.quit();
                }
            })
            .register()
    };

    // --- device discovery -------------------------------------------------------------------
    let registry = core.get_registry_rc()?;
    let metadata: Rc<RefCell<Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>> =
        Rc::new(RefCell::new(None));
    let patch = Rc::new(RefCell::new(Patch::new(
        init.mic_output.clone().unwrap_or_else(|| VMIC_NODE.to_string()),
    )));
    let _registry_listener = {
        let metadata = metadata.clone();
        let status_add = status.clone();
        let status_rm = status.clone();
        let repaint_add = repaint.clone();
        let repaint_rm = repaint.clone();
        let patch_add = patch.clone();
        let patch_rm = patch.clone();
        let core_reg = core.clone();
        let registry_weak = registry.downgrade();
        let tx_rm = self_tx.clone();
        registry
            .add_listener_local()
            .global(move |g| {
                let Some(props) = g.props else { return };
                if g.type_ == pw::types::ObjectType::Metadata {
                    if props.get("metadata.name") != Some("default") {
                        return;
                    }
                    let Some(reg) = registry_weak.upgrade() else { return };
                    let Ok(md) = reg.bind::<pw::metadata::Metadata, _>(g) else { return };
                    let tx = self_tx.clone();
                    let listener = md
                        .add_listener_local()
                        .property(move |_subject, key, _type, value| {
                            if key == Some("default.audio.source") {
                                let _ = tx.send(Cmd::DefaultSource(value.and_then(json_name)));
                            }
                            0
                        })
                        .register();
                    *metadata.borrow_mut() = Some((md, listener));
                    return;
                }
                if g.type_ == pw::types::ObjectType::Link {
                    let node = |k: &str| props.get(k).and_then(|v| v.parse::<u32>().ok());
                    let (Some(out_node), Some(in_node)) =
                        (node("link.output.node"), node("link.input.node"))
                    else {
                        return;
                    };
                    let mut p = patch_add.borrow_mut();
                    // Safety net: never let mic passthrough record the virtual mic itself.
                    let feedback = Some(out_node) == p.node_id(VMIC_NODE)
                        && Some(in_node) == p.node_id(CAPTURE_NODE);
                    // Leftover link from the feed into a previously selected mic output.
                    let stray = Some(out_node) == p.node_id(FEED_NODE)
                        && Some(in_node) != p.node_id(&p.target);
                    if feedback || stray {
                        if let Some(reg) = registry_weak.upgrade() {
                            reg.destroy_global(g.id);
                        }
                        return;
                    }
                    p.graph_links.insert(g.id, (out_node, in_node));
                    if p.sync(&status_add) {
                        drop(p);
                        repaint_add();
                    }
                    return;
                }
                if g.type_ == pw::types::ObjectType::Port {
                    let Some(node) = props.get("node.id").and_then(|v| v.parse::<u32>().ok()) else {
                        return;
                    };
                    let mut p = patch_add.borrow_mut();
                    p.ports.insert(
                        g.id,
                        PortInfo {
                            node,
                            output: props.get("port.direction") == Some("out"),
                            channel: props.get("audio.channel").unwrap_or("").to_string(),
                        },
                    );
                    p.try_link(&core_reg);
                    if p.sync(&status_add) {
                        drop(p);
                        repaint_add();
                    }
                    return;
                }
                if g.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let class = props.get("media.class").unwrap_or("");
                let name = props.get("node.name").unwrap_or("").to_string();
                let description = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .unwrap_or(&name)
                    .to_string();
                {
                    let mut p = patch_add.borrow_mut();
                    p.nodes.insert(
                        g.id,
                        NodeInfo {
                            name: name.clone(),
                            description: description.clone(),
                            class: class.to_string(),
                        },
                    );
                    p.try_link(&core_reg);
                }
                if name == VMIC_NODE {
                    let _ = self_tx.send(Cmd::VmicReady);
                    return;
                }
                if name.is_empty() || OWN_NODES.contains(&name.as_str()) {
                    return;
                }
                let dev = Device {
                    id: g.id,
                    name,
                    description,
                };
                let mut s = status_add.lock().unwrap();
                if class == "Audio/Sink" {
                    s.sinks.push(dev);
                } else if class.starts_with("Audio/Source") {
                    s.sources.push(dev);
                    let _ = self_tx.send(Cmd::SourcesChanged);
                } else {
                    return;
                }
                drop(s);
                repaint_add();
            })
            .global_remove(move |id| {
                {
                    let mut s = status_rm.lock().unwrap();
                    s.sinks.retain(|d| d.id != id);
                    let n = s.sources.len();
                    s.sources.retain(|d| d.id != id);
                    if s.sources.len() != n {
                        let _ = tx_rm.send(Cmd::SourcesChanged);
                    }
                }
                let mut p = patch_rm.borrow_mut();
                p.forget(id);
                p.sync(&status_rm);
                drop(p);
                repaint_rm();
            })
            .register()
    };

    // --- virtual mic + streams ----------------------------------------------------------------
    // The virtual microphone device itself. It lives as long as this proxy (i.e. the app).
    let _vmic_node: pw::node::Node = core.create_object(
        "adapter",
        &properties! {
            "factory.name" => "support.null-audio-sink",
            *pw::keys::NODE_NAME => VMIC_NODE,
            *pw::keys::NODE_DESCRIPTION => "Soundboard Virtual Mic",
            *pw::keys::MEDIA_CLASS => "Audio/Source/Virtual",
            "audio.position" => "[ FL FR ]",
            "audio.rate" => "48000",
            "monitor.channel-volumes" => "true",
            "node.virtual" => "true",
            // Keep it from being chosen as the system's default microphone.
            "priority.session" => "1",
            "priority.driver" => "1",
        },
    )?;

    let router = Rc::new(Router {
        core: core.clone(),
        engine: Rc::new(RefCell::new(Engine {
            master: init.master,
            speakers: Mixer::default(),
            mic: Mixer::default(),
            passthrough: init.passthrough,
            passthrough_gain: init.passthrough_gain,
            mic_in: VecDeque::new(),
            vmic_active: false,
        })),
        streams: RefCell::new(Streams::default()),
        status: status.clone(),
        repaint: repaint.clone(),
        speaker_target: RefCell::new(init.speaker_target),
        mic_choice: RefCell::new(init.mic_source),
        default_source: RefCell::new(None),
        capture_target: RefCell::new(None),
        patch,
        registry: registry.clone(),
    });
    router.recreate_speakers();
    // The mic feed is created once the virtual-mic node shows up (Cmd::VmicReady);
    // the capture stream once we know which microphone to use (Cmd::SourcesChanged / DefaultSource).

    {
        let mut s = status.lock().unwrap();
        s.connected = true;
        s.error = None;
    }
    repaint();

    // --- command handling ---------------------------------------------------------------------
    let _attached = {
        let ml = mainloop.clone();
        let r = router.clone();
        rx.attach(mainloop.loop_(), move |cmd| match cmd {
            Cmd::Play {
                id,
                samples,
                gain,
                to_speakers,
                to_mic,
                exclusive,
            } => {
                let mut e = r.engine.borrow_mut();
                if exclusive {
                    e.speakers.voices.clear();
                    e.mic.voices.clear();
                } else {
                    // Re-triggering a sound restarts it instead of stacking copies.
                    e.speakers.stop(id);
                    e.mic.stop(id);
                }
                if to_speakers {
                    e.speakers.voices.push(Voice {
                        id,
                        data: samples.clone(),
                        pos: 0,
                        gain,
                    });
                }
                if to_mic && e.vmic_active {
                    e.mic.voices.push(Voice {
                        id,
                        data: samples,
                        pos: 0,
                        gain,
                    });
                }
            }
            Cmd::Stop(id) => {
                let mut e = r.engine.borrow_mut();
                e.speakers.stop(id);
                e.mic.stop(id);
            }
            Cmd::StopAll => {
                let mut e = r.engine.borrow_mut();
                e.speakers.voices.clear();
                e.mic.voices.clear();
            }
            Cmd::SetMaster(v) => r.engine.borrow_mut().master = v,
            Cmd::SetVoiceGain { id, gain } => {
                let mut e = r.engine.borrow_mut();
                e.speakers.set_gain(id, gain);
                e.mic.set_gain(id, gain);
            }
            Cmd::SetSpeakerTarget(t) => {
                *r.speaker_target.borrow_mut() = t;
                r.recreate_speakers();
            }
            Cmd::SetMicSource(t) => {
                *r.mic_choice.borrow_mut() = t;
                r.refresh_capture(true);
            }
            Cmd::SetPassthrough { enabled, gain } => {
                {
                    let mut e = r.engine.borrow_mut();
                    e.passthrough = enabled;
                    e.passthrough_gain = gain;
                    if !enabled {
                        e.mic_in.clear();
                    }
                }
                r.refresh_capture(false);
            }
            Cmd::SetMicOutput(t) => r.set_mic_output(t),
            Cmd::VmicReady => r.ensure_feed(),
            Cmd::DefaultSource(name) => {
                *r.default_source.borrow_mut() = name;
                r.refresh_capture(false);
            }
            Cmd::SourcesChanged => r.refresh_capture(false),
            Cmd::Quit => ml.quit(),
        })
    };

    mainloop.run();

    // Tear down streams and the virtual mic before the core/context go away.
    drop(_attached);
    *router.streams.borrow_mut() = Streams::default();
    drop(_vmic_node);
    status.lock().unwrap().connected = false;
    Ok(())
}

/// Parse `{"name":"alsa_input.usb-..."}` as stored in the `default` metadata.
fn json_name(value: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

/// Owns the streams and decides where they connect. Lives on the PipeWire thread.
struct Router {
    core: pw::core::CoreRc,
    engine: Rc<RefCell<Engine>>,
    streams: RefCell<Streams>,
    status: SharedStatus,
    repaint: Rc<dyn Fn()>,
    speaker_target: RefCell<Option<String>>,
    /// Microphone picked in the UI (`None` = follow the system default).
    mic_choice: RefCell<Option<String>>,
    /// The session manager's current default microphone.
    default_source: RefCell<Option<String>>,
    /// Node the capture stream is currently connected to.
    capture_target: RefCell<Option<String>>,
    patch: Rc<RefCell<Patch>>,
    registry: pw::registry::RegistryRc,
}

impl Router {
    fn build(&self, kind: Kind, target: Option<&str>) -> Option<StreamSlot> {
        match make_stream(&self.core, kind, target, &self.engine, &self.status, &self.repaint) {
            Ok(slot) => Some(slot),
            Err(e) => {
                let what = match kind {
                    Kind::Speakers => "speaker stream",
                    Kind::VirtualMic => "virtual mic feed",
                    Kind::Capture => "microphone capture",
                };
                self.status.lock().unwrap().error = Some(format!("Could not create {what}: {e}"));
                (self.repaint)();
                None
            }
        }
    }

    fn recreate_speakers(&self) {
        self.streams.borrow_mut().speakers = None;
        let target = self.speaker_target.borrow().clone();
        let slot = self.build(Kind::Speakers, target.as_deref());
        self.streams.borrow_mut().speakers = slot;
    }

    fn ensure_feed(&self) {
        if self.streams.borrow().vmic.is_none() {
            let slot = self.build(Kind::VirtualMic, None);
            self.streams.borrow_mut().vmic = slot;
        }
    }

    /// Link the mic feed into another node (`None` = our virtual mic).
    fn set_mic_output(&self, name: Option<String>) {
        {
            let mut p = self.patch.borrow_mut();
            let target = name.unwrap_or_else(|| VMIC_NODE.to_string());
            if p.target == target {
                return;
            }
            p.target = target;
            p.links.clear();
            for id in p.stray_feed_links() {
                self.registry.destroy_global(id);
            }
            p.try_link(&self.core);
            p.sync(&self.status);
        }
        (self.repaint)();
        self.refresh_capture(false);
    }

    /// The microphone passthrough should record from, never our own virtual mic.
    fn resolve_mic(&self) -> Option<String> {
        let s = self.status.lock().unwrap();
        let exists = |n: &str| s.sources.iter().any(|d| d.name == n);
        if let Some(choice) = self.mic_choice.borrow().as_deref()
            && exists(choice)
        {
            return Some(choice.to_string());
        }
        if let Some(def) = self.default_source.borrow().as_deref()
            && def != VMIC_NODE
            && exists(def)
        {
            return Some(def.to_string());
        }
        // Default is missing or is the virtual mic: prefer a real (non-monitor) input.
        s.sources
            .iter()
            .find(|d| !d.name.ends_with(".monitor"))
            .or(s.sources.first())
            .map(|d| d.name.clone())
    }

    /// (Re)connect the passthrough capture stream to the right microphone.
    fn refresh_capture(&self, force: bool) {
        // Passthrough only makes sense into our own virtual mic. When the feed goes into another
        // source (e.g. EasyEffects), that source already carries the voice, and recording it
        // while feeding it would loop the soundboard back into itself.
        let own_output = self.patch.borrow().target == VMIC_NODE;
        let enabled = self.engine.borrow().passthrough && own_output;
        if !enabled {
            if self.streams.borrow_mut().capture.take().is_some() {
                *self.capture_target.borrow_mut() = None;
            }
            self.engine.borrow_mut().mic_in.clear();
            self.status.lock().unwrap().capture_state = String::from(if own_output {
                "off"
            } else {
                "off (not used with this mic output)"
            });
            (self.repaint)();
            return;
        }
        let want = self.resolve_mic();
        let have_stream = self.streams.borrow().capture.is_some();
        if !force && have_stream && *self.capture_target.borrow() == want {
            return;
        }
        self.streams.borrow_mut().capture = None;
        self.engine.borrow_mut().mic_in.clear();
        *self.capture_target.borrow_mut() = want.clone();
        match want {
            Some(target) => {
                let slot = self.build(Kind::Capture, Some(&target));
                self.streams.borrow_mut().capture = slot;
            }
            None => {
                self.status.lock().unwrap().capture_state = "no microphone found".into();
                (self.repaint)();
            }
        }
    }
}
