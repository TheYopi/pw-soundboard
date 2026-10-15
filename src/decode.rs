//! Audio file decoding (via Symphonia) into the engine's native format:
//! interleaved stereo f32 at 48 kHz.

use crossbeam_channel::{Receiver, Sender};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::TrackType;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::audio::{CHANNELS, SAMPLE_RATE};

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "wav", "wave", "flac", "ogg", "oga", "opus", "m4a", "aac", "mp4", "aiff", "aif", "caf",
    "mka", "webm",
];
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp"];

pub fn is_audio_file(path: &Path) -> bool {
    has_ext(path, AUDIO_EXTENSIONS)
}

pub fn is_image_file(path: &Path) -> bool {
    has_ext(path, IMAGE_EXTENSIONS)
}

fn has_ext(path: &Path, list: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Decoded clip, ready for the mixer.
#[derive(Clone)]
pub struct Clip {
    pub samples: Arc<Vec<f32>>,
    pub seconds: f32,
}

pub struct DecodeResult {
    pub id: u64,
    pub path: PathBuf,
    pub result: Result<Clip, String>,
}

/// Start a background decoder thread. Send `(sound_id, path)` jobs in, get results out.
pub fn spawn_worker(
    on_done: impl Fn() + Send + 'static,
) -> (Sender<(u64, PathBuf)>, Receiver<DecodeResult>) {
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<(u64, PathBuf)>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || {
            for (id, path) in job_rx {
                let result = decode_file(&path).map_err(|e| e.to_string());
                if res_tx.send(DecodeResult { id, path, result }).is_err() {
                    break;
                }
                on_done();
            }
        })
        .expect("spawn decoder thread");
    (job_tx, res_rx)
}

pub fn decode_file(path: &Path) -> anyhow::Result<Clip> {
    let file = Box::new(File::open(path)?);
    let mss = MediaSourceStream::new(file, Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format = symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow::anyhow!("no audio track found"))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow::anyhow!("track has no audio codec parameters"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())?;
    let track_id = track.id;

    let mut interleaved: Vec<f32> = Vec::new();
    let mut packet_buf: Vec<f32> = Vec::new();
    let mut src_rate = 0u32;
    let mut src_channels = 0usize;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            // Some containers signal the end with an unexpected-EOF IO error.
            Err(SymError::IoError(_)) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = buf.spec();
                src_rate = spec.rate();
                src_channels = spec.channels().count().max(1);
                packet_buf.resize(buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut packet_buf);
                interleaved.extend_from_slice(&packet_buf);
            }
            // Corrupt frames are skipped, like most players do.
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }

    if interleaved.is_empty() || src_rate == 0 {
        anyhow::bail!("file contains no decodable audio");
    }

    let stereo = to_stereo(&interleaved, src_channels);
    let out = resample_linear(&stereo, src_rate, SAMPLE_RATE);
    let seconds = out.len() as f32 / (SAMPLE_RATE as f32 * CHANNELS as f32);
    Ok(Clip {
        samples: Arc::new(out),
        seconds,
    })
}

/// Convert interleaved audio with any channel count to interleaved stereo.
fn to_stereo(input: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        2 => input.to_vec(),
        1 => input.iter().flat_map(|&s| [s, s]).collect(),
        n => input
            .chunks_exact(n)
            // Front-left / front-right are the first two channels in every common layout.
            .flat_map(|f| [f[0], f[1]])
            .collect(),
    }
}

/// Linear-interpolation resampler for interleaved stereo. Good enough for sound effects
/// and has no extra dependencies.
fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return input.to_vec();
    }
    let in_frames = input.len() / 2;
    if in_frames < 2 {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_frames = ((in_frames as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for i in 0..out_frames {
        let pos = i as f64 * ratio;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        let next = (idx + 1).min(in_frames - 1);
        for c in 0..2 {
            let a = input[idx * 2 + c];
            let b = input[next * 2 + c];
            out.push(a + (b - a) * frac);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_becomes_stereo() {
        assert_eq!(to_stereo(&[0.1, 0.2], 1), vec![0.1, 0.1, 0.2, 0.2]);
    }

    #[test]
    fn resample_doubles_length() {
        let input: Vec<f32> = (0..200).map(|i| i as f32).collect(); // 100 frames
        let out = resample_linear(&input, 24_000, 48_000);
        assert_eq!(out.len(), 400);
    }
}
