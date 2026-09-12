// SPDX-License-Identifier: MPL-2.0

//! Microphone capture, reduced to the smallest format a speech-to-text
//! service will accept without losing intelligibility: 16 kHz, mono,
//! signed 16-bit PCM in a WAV container (32 kB per second of speech).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig};
use tokio::sync::oneshot;

/// Whisper-family models are trained on 16 kHz audio; more is wasted bytes.
pub const TARGET_RATE: u32 = 16_000;

/// Below this peak amplitude we assume nothing was actually captured.
const SILENCE_PEAK: f32 = 0.004;
/// Refuse to transcribe a recording shorter than this.
const MIN_DURATION: Duration = Duration::from_millis(250);
/// Keep this much audio either side of speech when trimming silence.
const SILENCE_PAD: Duration = Duration::from_millis(120);

/// A finished recording, ready to post.
#[derive(Clone)]
pub struct Recording {
    pub wav: Vec<u8>,
    pub duration: Duration,
    pub peak: f32,
}

impl std::fmt::Debug for Recording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recording")
            .field("bytes", &self.wav.len())
            .field("duration", &self.duration)
            .field("peak", &self.peak)
            .finish()
    }
}

enum Cmd {
    Stop,
    Cancel,
}

/// Handle to a capture thread. Dropping it cancels the recording.
pub struct Handle {
    cmd: mpsc::Sender<Cmd>,
    level: Arc<AtomicU32>,
    started: Instant,
}

impl Handle {
    /// Stop capturing and let the worker encode what it has.
    pub fn stop(&self) {
        let _ = self.cmd.send(Cmd::Stop);
    }

    /// Stop capturing and throw the audio away.
    pub fn cancel(&self) {
        let _ = self.cmd.send(Cmd::Cancel);
    }

    /// Most recent peak amplitude, 0.0..=1.0, for the level meter.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Channels a caller awaits: `ready` reports whether the device opened,
/// `finished` delivers the encoded recording once stopped.
pub struct Channels {
    pub ready: oneshot::Receiver<Result<String, String>>,
    pub finished: oneshot::Receiver<Result<Recording, String>>,
}

/// Open the microphone on a dedicated thread and start capturing.
///
/// Capture runs on its own thread because a `cpal::Stream` is neither `Send`
/// nor safe to hold across the applet's event loop.
pub fn start(device_name: Option<String>, max: Duration) -> (Handle, Channels) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    let level = Arc::new(AtomicU32::new(0));

    let thread_level = Arc::clone(&level);
    std::thread::Builder::new()
        .name("whispr-capture".into())
        .spawn(move || {
            capture_thread(
                device_name,
                max,
                cmd_rx,
                thread_level,
                ready_tx,
                finished_tx,
            )
        })
        .expect("spawn capture thread");

    let handle = Handle {
        cmd: cmd_tx,
        level,
        started: Instant::now(),
    };
    (
        handle,
        Channels {
            ready: ready_rx,
            finished: finished_rx,
        },
    )
}

fn capture_thread(
    device_name: Option<String>,
    max: Duration,
    cmd_rx: mpsc::Receiver<Cmd>,
    level: Arc<AtomicU32>,
    ready_tx: oneshot::Sender<Result<String, String>>,
    finished_tx: oneshot::Sender<Result<Recording, String>>,
) {
    let samples = Arc::new(Mutex::new(Vec::<f32>::new()));

    let opened = open_stream(
        device_name.as_deref(),
        Arc::clone(&samples),
        Arc::clone(&level),
    );
    let (stream, config, name) = match opened {
        Ok(parts) => parts,
        Err(error) => {
            let message = format!("{error:#}");
            let _ = ready_tx.send(Err(message.clone()));
            let _ = finished_tx.send(Err(message));
            return;
        }
    };
    let _ = ready_tx.send(Ok(name));

    let started = Instant::now();
    let cancelled = loop {
        match cmd_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Cmd::Stop) => break false,
            // A disconnected channel means the handle was dropped.
            Ok(Cmd::Cancel) | Err(RecvTimeoutError::Disconnected) => break true,
            Err(RecvTimeoutError::Timeout) => {
                if started.elapsed() >= max {
                    tracing::warn!(?max, "recording hit the duration cap");
                    break false;
                }
            }
        }
    };

    // Dropping the stream here stops the callback before we take the buffer.
    drop(stream);
    if cancelled {
        return;
    }

    let captured = std::mem::take(&mut *samples.lock().expect("capture buffer poisoned"));
    let result = encode(captured, config.sample_rate).map_err(|error| format!("{error:#}"));
    let _ = finished_tx.send(result);
}

fn open_stream(
    device_name: Option<&str>,
    samples: Arc<Mutex<Vec<f32>>>,
    level: Arc<AtomicU32>,
) -> Result<(cpal::Stream, StreamConfig, String)> {
    let host = cpal::default_host();
    let device = match device_name.filter(|name| !name.is_empty()) {
        Some(wanted) => host
            .input_devices()
            .context("cannot enumerate input devices")?
            .find(|device| device_name_of(device).is_some_and(|name| name == wanted))
            .ok_or_else(|| anyhow!("input device {wanted:?} is not available"))?,
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device — is PipeWire running?"))?,
    };
    let name = device_name_of(&device).unwrap_or_else(|| "unknown".into());

    let supported = pick_config(&device)?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let channels = config.channels as usize;
    tracing::info!(%name, rate = config.sample_rate, channels, ?format, "capturing");

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, channels, samples, level),
        SampleFormat::I16 => build::<i16>(&device, &config, channels, samples, level),
        SampleFormat::U16 => build::<u16>(&device, &config, channels, samples, level),
        SampleFormat::I32 => build::<i32>(&device, &config, channels, samples, level),
        SampleFormat::I8 => build::<i8>(&device, &config, channels, samples, level),
        SampleFormat::U8 => build::<u8>(&device, &config, channels, samples, level),
        other => bail!("unsupported sample format {other:?}"),
    }
    .context("cannot open the microphone")?;

    stream.play().context("cannot start the capture stream")?;
    Ok((stream, config, name))
}

/// Prefer a configuration the device can deliver at 16 kHz in mono, so the
/// resampler and downmix below become no-ops.
fn pick_config(device: &Device) -> Result<SupportedStreamConfig> {
    let ranges = device
        .supported_input_configs()
        .context("cannot query supported input configs")?;

    let native_16k = ranges
        .filter(|range| {
            range.min_sample_rate() <= TARGET_RATE && TARGET_RATE <= range.max_sample_rate()
        })
        .min_by_key(|range| (range.channels(), format_rank(range.sample_format())))
        .map(|range| range.with_sample_rate(TARGET_RATE));

    match native_16k {
        Some(config) => Ok(config),
        None => device
            .default_input_config()
            .context("no usable input configuration"),
    }
}

/// Lower is better: float avoids a conversion, 16-bit is the common native.
fn format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::I32 => 2,
        _ => 3,
    }
}

fn build<T>(
    device: &Device,
    config: &StreamConfig,
    channels: usize,
    samples: Arc<Mutex<Vec<f32>>>,
    level: Arc<AtomicU32>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _| {
            let mut peak = 0.0f32;
            let mut buffer = samples.lock().expect("capture buffer poisoned");
            buffer.reserve(data.len() / channels.max(1));
            for frame in data.chunks(channels.max(1)) {
                // Downmix to mono: speech models take one channel anyway.
                let mono = frame
                    .iter()
                    .map(|sample| sample.to_sample::<f32>())
                    .sum::<f32>()
                    / frame.len() as f32;
                peak = peak.max(mono.abs());
                buffer.push(mono);
            }
            level.store(peak.to_bits(), Ordering::Relaxed);
        },
        |error| tracing::error!(%error, "capture stream error"),
        None,
    )
}

/// Trim, resample to 16 kHz, normalize, and wrap in a WAV header.
fn encode(samples: Vec<f32>, rate: u32) -> Result<Recording> {
    if rate == 0 {
        bail!("device reported a zero sample rate");
    }
    let duration = Duration::from_secs_f64(samples.len() as f64 / rate as f64);
    if duration < MIN_DURATION {
        bail!("recording was too short");
    }

    let peak = samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    if peak < SILENCE_PEAK {
        bail!("no audio captured — check that the microphone is not muted");
    }

    let trimmed = trim_silence(&samples, rate, peak);
    let resampled = resample(trimmed, rate, TARGET_RATE);
    let leveled = normalize(resampled, peak);

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::with_capacity(leveled.len() * 2 + 44));
    let mut writer =
        hound::WavWriter::new(&mut cursor, spec).context("cannot start the WAV encoder")?;
    for sample in &leveled {
        let clamped = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        writer.write_sample(clamped).context("cannot write audio")?;
    }
    writer.finalize().context("cannot finalize the WAV file")?;

    Ok(Recording {
        wav: cursor.into_inner(),
        duration: Duration::from_secs_f64(leveled.len() as f64 / TARGET_RATE as f64),
        peak,
    })
}

/// Drop leading and trailing silence, keeping a short pad so words are not
/// clipped. Saves upload bytes on push-to-talk recordings.
fn trim_silence(samples: &[f32], rate: u32, peak: f32) -> &[f32] {
    let threshold = (peak * 0.02).max(SILENCE_PEAK);
    let first = samples.iter().position(|s| s.abs() > threshold);
    let Some(first) = first else {
        return samples;
    };
    let last = samples
        .iter()
        .rposition(|s| s.abs() > threshold)
        .unwrap_or(samples.len() - 1);

    let pad = (SILENCE_PAD.as_secs_f64() * rate as f64) as usize;
    let start = first.saturating_sub(pad);
    let end = (last + pad + 1).min(samples.len());
    &samples[start..end]
}

/// Band-limited resampling with a Blackman-windowed sinc kernel.
///
/// Written out rather than pulled from a crate because the whole job is one
/// offline pass over a few seconds of mono speech, and aliasing above 8 kHz
/// is the only thing that has to be got right.
fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }

    let ratio = from as f64 / to as f64;
    // Cutoff sits just under the lower Nyquist of the two rates.
    let cutoff = 0.5 * 0.95 / ratio.max(1.0);
    // Widen the kernel when decimating so the transition band stays sharp.
    let half = (16.0 * ratio.max(1.0)).ceil() as isize;
    let output_len = (input.len() as f64 / ratio).floor() as usize;
    let mut output = Vec::with_capacity(output_len);

    for n in 0..output_len {
        let center = n as f64 * ratio;
        let base = center.floor() as isize;
        let mut acc = 0.0f64;
        let mut norm = 0.0f64;

        for k in (base - half + 1)..=(base + half) {
            let Ok(index) = usize::try_from(k) else {
                continue;
            };
            let Some(sample) = input.get(index) else {
                continue;
            };
            let x = center - k as f64;
            let weight = sinc(2.0 * cutoff * x) * blackman(x / half as f64);
            acc += *sample as f64 * weight;
            norm += weight;
        }

        // Normalizing by the realized kernel sum keeps the gain flat at the
        // edges, where the window is truncated.
        output.push(if norm.abs() > f64::EPSILON {
            (acc / norm) as f32
        } else {
            0.0
        });
    }

    output
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let pi_x = std::f64::consts::PI * x;
        pi_x.sin() / pi_x
    }
}

/// Blackman window over `x` in -1.0..=1.0; zero outside.
fn blackman(x: f64) -> f64 {
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let t = std::f64::consts::PI * (x + 1.0);
    0.42 - 0.5 * t.cos() + 0.08 * (2.0 * t).cos()
}

/// Gentle make-up gain for quiet microphones, capped so room noise in a
/// near-silent recording is not amplified into garbage.
fn normalize(mut samples: Vec<f32>, peak: f32) -> Vec<f32> {
    if peak <= 0.0 {
        return samples;
    }
    let gain = (0.9 / peak).clamp(1.0, 8.0);
    if gain > 1.01 {
        for sample in &mut samples {
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }
    }
    samples
}

/// One-line summary of the device a recording would use, for `--check`.
pub fn describe_input(device_name: Option<&str>) -> Result<String> {
    let host = cpal::default_host();
    let device = match device_name.filter(|name| !name.is_empty()) {
        Some(wanted) => host
            .input_devices()
            .context("cannot enumerate input devices")?
            .find(|device| device_name_of(device).is_some_and(|name| name == wanted))
            .ok_or_else(|| anyhow!("input device {wanted:?} is not available"))?,
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?,
    };

    let name = device_name_of(&device).unwrap_or_else(|| "unknown".into());
    let config = pick_config(&device)?;
    let resampling = if config.sample_rate() == TARGET_RATE {
        "native 16 kHz"
    } else {
        "resampled to 16 kHz"
    };
    Ok(format!(
        "{name} — {} Hz, {} ch, {:?} ({resampling})",
        config.sample_rate(),
        config.channels(),
        config.sample_format(),
    ))
}

/// cpal reports the name inside a structured device description.
fn device_name_of(device: &Device) -> Option<String> {
    device
        .description()
        .ok()
        .map(|description| description.name().to_string())
}

/// Names of available input devices, for the settings dropdown.
pub fn input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut names: Vec<String> = devices
        .filter_map(|device| device_name_of(&device))
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, freq: f64, seconds: f64) -> Vec<f32> {
        let count = (rate as f64 * seconds) as usize;
        (0..count)
            .map(|n| {
                let t = n as f64 / rate as f64;
                (2.0 * std::f64::consts::PI * freq * t).sin() as f32 * 0.5
            })
            .collect()
    }

    #[test]
    fn resample_preserves_length_and_amplitude() {
        let input = tone(48_000, 440.0, 0.5);
        let output = resample(&input, 48_000, TARGET_RATE);

        assert_eq!(output.len(), 8_000, "0.5 s at 16 kHz");
        // Ignore the kernel's ramp-in/out at the very edges.
        let peak = output[200..output.len() - 200]
            .iter()
            .fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.02, "peak drifted to {peak}");
    }

    #[test]
    fn resample_rejects_above_nyquist() {
        // 7 kHz survives at 16 kHz; 20 kHz must not alias back into the band.
        let keep = resample(&tone(48_000, 7_000.0, 0.2), 48_000, TARGET_RATE);
        let reject = resample(&tone(48_000, 20_000.0, 0.2), 48_000, TARGET_RATE);

        let energy = |s: &[f32]| s.iter().map(|v| (v * v) as f64).sum::<f64>() / s.len() as f64;
        assert!(energy(&keep) > 0.1, "passband was attenuated");
        assert!(energy(&reject) < 0.001, "stopband leaked through");
    }

    #[test]
    fn resample_is_identity_at_matching_rates() {
        let input = tone(16_000, 300.0, 0.1);
        assert_eq!(resample(&input, TARGET_RATE, TARGET_RATE), input);
    }

    #[test]
    fn trim_drops_leading_and_trailing_silence() {
        let rate = 16_000;
        let mut samples = vec![0.0f32; rate as usize];
        samples.extend(tone(rate, 440.0, 0.5));
        samples.extend(vec![0.0f32; rate as usize]);

        let trimmed = trim_silence(&samples, rate, 0.5);
        let padded = (SILENCE_PAD.as_secs_f64() * rate as f64) as usize;
        // 0.5 s of speech plus one pad either side, give or take a sample.
        let expected = (rate / 2) as usize + 2 * padded;
        assert!(
            trimmed.len().abs_diff(expected) < 16,
            "got {}",
            trimmed.len()
        );
    }

    #[test]
    fn encode_rejects_silence() {
        let silence = vec![0.0f32; 16_000];
        let error = encode(silence, TARGET_RATE).unwrap_err().to_string();
        assert!(error.contains("no audio"), "{error}");
    }

    #[test]
    fn encode_produces_16k_mono_wav() {
        let recording = encode(tone(48_000, 440.0, 1.0), 48_000).expect("encodes");
        let mut reader =
            hound::WavReader::new(std::io::Cursor::new(recording.wav)).expect("valid wav");

        let spec = reader.spec();
        assert_eq!(spec.sample_rate, TARGET_RATE);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
        assert!(reader.samples::<i16>().count() > 15_000);
    }
}
