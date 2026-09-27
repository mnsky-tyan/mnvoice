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
//     buffer. A 20 ms ticker does the rest: decimate to the provider rate,
//     convert to i16 mono, run the silence detector, feed the transcriber.
//     A slow transcriber consumer can never make the callback overrun.

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
    // Decimation factor from the device rate to the provider rate; a device
    // below 16 kHz would give zero, so clamp to pass-through.
    let step = (in_rate / SAMPLE_RATE).max(1) as usize;

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
    let mut since_voice_ms = 0u64;
    loop {
        thread::sleep(Duration::from_millis(20));

        let raw: Vec<f32> = buffer
            .lock()
            .map(|mut buf| std::mem::take(&mut *buf))
            .unwrap_or_default();

        // Keep every `step`-th sample: linear decimation to 16 kHz, mono
        // (interleaved channels collapse onto the primary voice, which is
        // where dictation microphones put it).
        let chunk: Vec<i16> = raw
            .iter()
            .enumerate()
            .filter(|(i, _)| i % step == 0)
            .map(|(_, s)| (*s * i16::MAX as f32) as i16)
            .collect();

        if !chunk.is_empty() {
            let _ = tx.send(chunk);
        }

        if stop.load(Ordering::SeqCst)
            || started.elapsed() >= Duration::from_secs(max_seconds as u64)
        {
            return Ok(());
        }

        let rms = if chunk.is_empty() {
            0.0
        } else {
            let sum: f64 = chunk.iter().map(|s| (*s as f64) * (*s as f64)).sum();
            (sum / chunk.len() as f64).sqrt()
        };
        if rms < vad_rms_threshold {
            since_voice_ms += 20;
            if vad_silence_ms > 0 && since_voice_ms >= vad_silence_ms as u64 {
                return Ok(());
            }
        } else {
            since_voice_ms = 0;
        }
    }
    // `stream` drops here, which stops capture and joins the device thread.
}
