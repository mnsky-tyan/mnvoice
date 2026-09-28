// Audio capture over cpal, shared by the Linux and macOS backends.
//
// The WASAPI engine on Windows earns its complexity with the standby trick:
// the client is initialized once and starts in ~4 ms. cpal offers no
// equivalent of "open now, configure later", so this backend opens the device
// per session. That is the honest cost of the port: capture start on Linux and
// macOS is a device open rather than 4 ms. The VAD, the chunking and the
// channel contract are the same as Windows, so everything downstream of the
// trait is identical.
//
// Two structural choices are forced by the libraries, and both live here so
// no caller ever trips on them:
//
//   - cpal's `Stream` is not `Send` (it stores its callback as a plain
//     `dyn FnMut`, which may hold raw pointers), so the stream is created,
//     played and dropped entirely inside one thread. Device errors therefore
//     arrive through the done-channel rather than at construction time.
//   - Devices rarely capture at 16 kHz mono, and the realtime callback must
//     never block, so the callback only appends raw samples to a shared
//     buffer. A 20 ms ticker does the rest: mix to mono, resample to the
//     provider rate, run the silence detector, feed the transcriber. A slow
//     transcriber consumer can never make the callback overrun.

use crate::platform::audio::{Audio, SAMPLE_RATE};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub struct CpalAudio;

impl CpalAudio {
    pub fn new() -> Result<Self, String> {
        // Touching the host once at startup warms the audio stack, which is
        // as close to the standby trick as cpal allows.
        let _ = cpal::default_host();
        Ok(Self)
    }
}

impl Audio for CpalAudio {
    fn capture_to_channel(
        &self,
        stop: Arc<AtomicBool>,
        max_seconds: u32,
        vad_silence_ms: u32,
        vad_rms_threshold: f64,
        tx: Sender<Vec<i16>>,
    ) -> Result<Receiver<Result<(), String>>, String> {
        let (done_tx, done_rx) = channel();

        thread::spawn(move || {
            let result = run_session(stop, max_seconds, vad_silence_ms, vad_rms_threshold, tx);
            // Whatever happened, the caller learns about it here: a clean
            // stop, the VAD firing, or the error that ended the session.
            let _ = done_tx.send(result);
        });

        Ok(done_rx)
    }
}

fn run_session(
    stop: Arc<AtomicBool>,
    max_seconds: u32,
    vad_silence_ms: u32,
    vad_rms_threshold: f64,
    tx: Sender<Vec<i16>>,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let device = cpal::default_host()
        .default_input_device()
        .ok_or("no input audio device found")?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("cannot query input device ({e})"))?;
    let in_rate = supported.sample_rate().0;
    let channels = supported.channels().max(1) as usize;
    // Input samples per output sample. Fractional on purpose: a device rate
    // that is not a multiple of the provider rate (44.1 kHz, the macOS
    // built-in input's nominal rate, is not) has to be resampled rather than
    // decimated, or the provider is told 16 kHz and handed audio running at
    // some other rate entirely.
    let resample_step = in_rate as f64 / SAMPLE_RATE as f64;

    let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));

    let err_fn = move |e| eprintln!("mnvoice audio stream error: {e}");
    let cb_buffer = Arc::clone(&buffer);
    let push = move |data: &[f32]| {
        if let Ok(mut buf) = cb_buffer.lock() {
            buf.extend_from_slice(data);
        }
    };

    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &supported.into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| push(data),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &supported.into(),
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                push(&data.iter().map(|s| *s as f32 / i16::MAX as f32).collect::<Vec<f32>>())
            },
            err_fn,
            None,
        ),
        other => {
            return Err(format!(
                "unsupported input sample format ({other:?}); expected f32 or i16"
            ))
        }
    }
    .map_err(|e| format!("cannot open input stream ({e})"))?;

    stream.play().map_err(|e| format!("cannot start capture ({e})"))?;

    let started = Instant::now();
    let mut speech_started = false;
    let mut since_voice_ms = 0u64;
    let mut no_speech_ms = 0u64;
    loop {
        thread::sleep(Duration::from_millis(20));

        let raw: Vec<f32> = buffer
            .lock()
            .map(|mut buf| std::mem::take(&mut *buf))
            .unwrap_or_default();

        // Frames arrive interleaved, so the channels are averaged into mono
        // before resampling. Taking every nth sample of a stereo stream
        // instead would alternate left and right, and the provider would be
        // sent an alternating signal rather than the mixed-down voice the
        // Windows engine hands it.
        let mono: Vec<f32> = raw
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect();

        let chunk = resample(&mono, resample_step);

        // The silence detector reads the same window that goes downstream,
        // so it runs before the hand-off (sending moves the chunk).
        let rms = if chunk.is_empty() {
            0.0
        } else {
            let sum: f64 = chunk.iter().map(|s| (*s as f64) * (*s as f64)).sum();
            (sum / chunk.len() as f64).sqrt()
        };

        if !chunk.is_empty() {
            let _ = tx.send(chunk);
        }

        if stop.load(Ordering::SeqCst)
            || started.elapsed() >= Duration::from_secs(max_seconds as u64)
        {
            return Ok(());
        }
        if rms > vad_rms_threshold {
            speech_started = true;
            since_voice_ms = 0;
        } else if speech_started {
            // Silence only ends a dictation once there has been speech, which
            // is what VAD_SILENCE_MS documents; waiting that long before the
            // first word would stop a session the user is still thinking in.
            since_voice_ms += 20;
            if silence_expired(since_voice_ms, vad_silence_ms) {
                return Ok(());
            }
        } else {
            // Nothing said at all: the same no-speech cutoff Windows uses, so a
            // forgotten open mic cannot hold the device for max_seconds.
            no_speech_ms += 20;
            if no_speech_ms >= 10_000 {
                return Ok(());
            }
        }
    }
    // `stream` drops here, which stops capture and joins the device thread.
}

/// Whether the silence detector ends a dictation that has heard speech.
///
/// The threshold is the configured value itself, the way the Windows engine
/// reads it, so `VAD_SILENCE_MS=0` stops on the first silent tick instead of
/// switching the detector off.
fn silence_expired(since_voice_ms: u64, vad_silence_ms: u32) -> bool {
    since_voice_ms >= vad_silence_ms as u64
}

/// Resamples mono audio to the provider rate by linear interpolation, the same
/// conversion the Windows engine applies to the device's mix format. The step
/// is fractional so a device rate that is not a multiple of the provider rate
/// still comes out at exactly the provider rate.
fn resample(mono: &[f32], step: f64) -> Vec<i16> {
    let mut out = Vec::with_capacity((mono.len() as f64 / step) as usize + 1);
    let mut pos = 0f64;
    while (pos as usize) < mono.len() {
        let i = pos as usize;
        let frac = (pos - i as f64) as f32;
        let a = mono[i];
        let b = if i + 1 < mono.len() { mono[i + 1] } else { a };
        let v = (a + (b - a) * frac).clamp(-1.0, 1.0);
        out.push((v * i16::MAX as f32) as i16);
        pos += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device rate that is not a multiple of the provider rate must still
    /// come out at the provider rate. 44.1 kHz is the macOS built-in input's
    /// nominal rate: keeping every second sample of it hands the provider
    /// 22 050 samples per second while the request still says 16 000, so a
    /// one second utterance arrives as 1.38 s of sped-up audio.
    #[test]
    fn a_44100hz_second_resamples_to_the_provider_rate() {
        let input: Vec<f32> = (0..44_100).map(|i| i as f32 / 44_100.0).collect();
        let out = resample(&input, 44_100.0 / SAMPLE_RATE as f64);
        assert!(
            (out.len() as i64 - SAMPLE_RATE as i64).abs() <= 2,
            "one second of 44.1 kHz audio produced {} samples",
            out.len()
        );
    }

    /// The resampled signal must be the same signal and not merely the same
    /// length: interpolating a linear ramp reproduces it exactly, so anything
    /// beyond quantisation is the resampler mangling the waveform.
    #[test]
    fn resampling_preserves_the_waveform() {
        let rate = 44_100.0;
        let n = 44_100usize;
        let input: Vec<f32> = (0..n).map(|i| i as f32 / n as f32).collect();
        let step = rate / SAMPLE_RATE as f64;
        let out = resample(&input, step);
        let worst = out
            .iter()
            .enumerate()
            .map(|(k, sample)| {
                let expected = (k as f64 * step) / n as f64;
                (*sample as f64 / i16::MAX as f64 - expected).abs()
            })
            .fold(0.0f64, f64::max);
        assert!(worst < 2.0 / i16::MAX as f64, "worst deviation was {worst}");
    }

    /// A device slower than the provider rate is upsampled rather than
    /// clamped to pass-through, so the provider still receives its own rate.
    #[test]
    fn an_8000hz_device_is_upsampled_to_the_provider_rate() {
        let input: Vec<f32> = (0..8_000).map(|i| i as f32 / 8_000.0).collect();
        let out = resample(&input, 8_000.0 / SAMPLE_RATE as f64);
        assert_eq!(out.len(), 16_000);
    }

    /// The configured value is the threshold itself, the way the Windows
    /// engine reads it: `VAD_SILENCE_MS=0` ends the dictation on the first
    /// silent tick. A guard that treated zero as "detector off" left the
    /// recording running to MAX_SECONDS while the banner still promised that
    /// the configured silence would stop it.
    #[test]
    fn a_zero_silence_threshold_still_ends_the_dictation() {
        assert!(
            silence_expired(20, 0),
            "one silent tick must stop a dictation configured for 0 ms"
        );
        assert!(
            !silence_expired(20, 3_000),
            "the default threshold needs three seconds of quiet"
        );
        assert!(silence_expired(3_000, 3_000));
    }
}
