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

use crate::platform::audio::{
    audio_ms, resample_linear, silence_expired, Audio, SilenceWindows, NO_SPEECH_LIMIT_MS,
    SAMPLE_RATE,
};
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
                push(
                    &data
                        .iter()
                        .map(|s| *s as f32 / i16::MAX as f32)
                        .collect::<Vec<f32>>(),
                )
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

    stream
        .play()
        .map_err(|e| format!("cannot start capture ({e})"))?;

    let started = Instant::now();
    let mut silence = SilenceWindows::new();
    loop {
        thread::sleep(Duration::from_millis(20));

        // Frames arrive interleaved, so the channels are averaged into mono
        // before resampling. Taking every nth sample of a stereo stream
        // instead would alternate left and right, and the provider would be
        // sent an alternating signal rather than the mixed-down voice the
        // Windows engine hands it.
        let mono = take_frames(&buffer, channels);

        let chunk = resample(&mono, resample_step);

        // The silence detector reads the same window that goes downstream,
        // so it runs before the hand-off (sending moves the chunk).
        let chunk_len = chunk.len();
        let rms = if chunk.is_empty() {
            0.0
        } else {
            let sum: f64 = chunk.iter().map(|s| (*s as f64) * (*s as f64)).sum();
            (sum / chunk.len() as f64).sqrt()
        };

        if !chunk.is_empty() && !hand_off(&tx, chunk) {
            return Ok(());
        }

        if stop.load(Ordering::SeqCst)
            || started.elapsed() >= Duration::from_secs(max_seconds as u64)
        {
            return Ok(());
        }
        if silence.advance(chunk_len, rms, vad_rms_threshold, vad_silence_ms) {
            return Ok(());
        }
    }
    // `stream` drops here, which stops capture and joins the device thread.
}

/// The two silence windows that end a dictation.
///
/// Both advance by the audio each tick consumed, which is what keeps them
/// honest when the capture thread is descheduled and a single tick carries a
/// second of device audio: the window still counts a second.
struct SilenceWindows {
    /// Quiet since the last voice, once there has been any.
    since_voice_ms: u64,
    /// Quiet since the session opened.
    no_speech_ms: u64,
    heard_voice: bool,
}

impl SilenceWindows {
    fn new() -> Self {
        Self {
            since_voice_ms: 0,
            no_speech_ms: 0,
            heard_voice: false,
        }
    }

    /// Feeds one tick's audio, reporting whether the session should end.
    fn advance(&mut self, samples: usize, rms: f64, threshold: f64, vad_silence_ms: u32) -> bool {
        if rms > threshold {
            self.heard_voice = true;
            self.since_voice_ms = 0;
            false
        } else if self.heard_voice {
            // Silence only ends a dictation once there has been speech, which
            // is what VAD_SILENCE_MS documents; waiting that long before the
            // first word would stop a session the user is still thinking in.
            self.since_voice_ms += audio_ms(samples);
            silence_expired(self.since_voice_ms, vad_silence_ms)
        } else {
            // Nothing said at all: the same no-speech cutoff Windows uses, so a
            // forgotten open mic cannot hold the device for max_seconds.
            self.no_speech_ms += audio_ms(samples);
            self.no_speech_ms >= NO_SPEECH_LIMIT_MS
        }
    }
}

/// Whether the silence detector ends a dictation that has heard speech.
///
/// The threshold is the configured value itself, the way the Windows engine
/// reads it, so `VAD_SILENCE_MS=0` stops on the first silent tick instead of
/// switching the detector off.
fn silence_expired(since_voice_ms: u64, vad_silence_ms: u32) -> bool {
    since_voice_ms >= vad_silence_ms as u64
}

/// Mixes the interleaved frames in `buf` down to mono and leaves a partial
/// frame behind for the next tick.
///
/// The device's 20 ms tick boundary almost never lands on a frame boundary on
/// a stereo device, so dropping the remainder loses a sample or two about
/// fifty times a second and reads the rest of the next tick one sample out of
/// phase. The Windows engine instead refuses a partial frame outright, which
/// is not an option here: the realtime callback appends whatever the device
/// handed it, so the tail has to be carried.
fn take_frames(buffer: &Arc<Mutex<Vec<f32>>>, channels: usize) -> Vec<f32> {
    let Ok(mut buf) = buffer.lock() else {
        return Vec::new();
    };
    // Only whole frames leave the buffer, so what stays is by construction a
    // partial one.
    let whole = buf.len() / channels * channels;
    let taken: Vec<f32> = buf.drain(..whole).collect();
    taken
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Hands one chunk to the consumer and reports whether it is still reading.
///
/// The streaming loop owns the receiver, so a send that fails means it has
/// already finished: a provider-side disconnect makes it break out of its send
/// phase and drop the receiver. The microphone then has nothing left to record
/// for, so the session ends instead of holding the device open - the same
/// channel contract the Windows engine keeps.
fn hand_off(tx: &Sender<Vec<i16>>, chunk: Vec<i16>) -> bool {
    tx.send(chunk).is_ok()
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
        let out = resample_linear(&input, 44_100.0 / SAMPLE_RATE as f64);
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
        let out = resample_linear(&input, step);
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
        let out = resample_linear(&input, 8_000.0 / SAMPLE_RATE as f64);
        assert_eq!(out.len(), 16_000);
    }

    /// The tick boundary almost never lands on a frame boundary, so a partial
    /// frame has to survive into the next tick. Dropping it instead loses a
    /// sample or two about fifty times a second and reads the rest of the
    /// next tick one sample out of phase.
    #[test]
    fn a_partial_frame_is_carried_into_the_next_tick() {
        let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(vec![1.0, 0.0, 0.5, 0.5, 0.25]));
        let mono = take_frames(&buffer, 2);
        assert_eq!(
            mono,
            vec![0.5, 0.5],
            "two whole frames should have been mixed down"
        );
        assert_eq!(
            buffer.lock().unwrap().as_slice(),
            [0.25],
            "the leftover sample must stay for the next tick"
        );
    }

    /// A streaming loop that stopped reading has already finished, so the
    /// capture has to end with it. Discarding the send result instead leaves
    /// the microphone open: the CLI keeps printing "recording..." until the
    /// VAD fires or MAX_SECONDS expires, which is the channel contract the
    /// Windows engine keeps broken on the Unix side.
    #[test]
    fn a_consumer_that_stopped_reading_ends_the_session() {
        let (tx, rx) = channel::<Vec<i16>>();
        assert!(
            hand_off(&tx, vec![0i16; 320]),
            "a live consumer takes the chunk"
        );
        drop(rx);
        assert!(
            !hand_off(&tx, vec![0i16; 320]),
            "a consumer that stopped reading must end the session"
        );
    }
}
