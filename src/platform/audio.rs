// The audio capture contract.
//
// Shaped by the property that makes mnvoice worth using: capture starts in
// single-digit milliseconds because the platform engine is initialized once at
// startup and held in standby, not opened per session. So the trait is a
// persistent engine handed a channel to stream into - not a `record() ->
// samples` call, which is the shape that would quietly reintroduce a
// half-second device open on every hotkey press.
//
// Chunks are mono 16 kHz i16 LE (see [`SAMPLE_RATE`]); the worker owns device
// format conversion, so callers never see a WAVEFORMATEX.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

/// Every backend delivers this rate; providers are configured to expect it.
pub const SAMPLE_RATE: u32 = 16_000;

/// A session that has heard no speech at all ends itself after this long, so
/// an accidental hotkey press does not hold the microphone hostage until
/// `max_seconds`. Both engines enforce it.
pub const NO_SPEECH_LIMIT_MS: u64 = 10_000;

/// How much wall-clock audio a buffer of `samples` covers at the shared rate.
///
/// Both engines advance their voice-activity windows by the audio they
/// consumed, not by loop ticks - a tick that ran long still counts the time it
/// covered, so a stalled capture thread ends the session on the configured
/// silence instead of leaving the microphone open.
pub fn audio_ms(samples: usize) -> u64 {
    samples as u64 * 1000 / SAMPLE_RATE as u64
}

/// The two silence windows that end a dictation.
///
/// Shared by both engines so a VAD change cannot land on one platform only.
/// Both windows advance by the audio each chunk carries (see [`audio_ms`]),
/// which is what keeps them honest when the capture loop is descheduled and a
/// single chunk covers more than the usual 40 ms: the window still counts that
/// time.
pub struct SilenceWindows {
    /// Quiet since the last voice, once there has been any.
    since_voice_ms: u64,
    /// Quiet since the session opened.
    no_speech_ms: u64,
    heard_voice: bool,
}

impl SilenceWindows {
    pub fn new() -> Self {
        Self {
            since_voice_ms: 0,
            no_speech_ms: 0,
            heard_voice: false,
        }
    }

    /// Feeds one chunk's audio, reporting whether the session should end.
    pub fn advance(
        &mut self,
        samples: usize,
        rms: f64,
        threshold: f64,
        vad_silence_ms: u32,
    ) -> bool {
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
            // Nothing said at all: the shared no-speech cutoff, so a forgotten
            // open mic cannot hold the device for max_seconds.
            self.no_speech_ms += audio_ms(samples);
            self.no_speech_ms >= NO_SPEECH_LIMIT_MS
        }
    }
}

/// Whether the silence detector ends a dictation that has heard speech.
///
/// The threshold is the configured value itself, so `VAD_SILENCE_MS=0` stops
/// on the first silent tick instead of switching the detector off.
pub fn silence_expired(since_voice_ms: u64, vad_silence_ms: u32) -> bool {
    since_voice_ms >= vad_silence_ms as u64
}

/// Resamples mono f32 samples to [`SAMPLE_RATE`] with linear interpolation.
///
/// Shared by both engines: the Windows engine resamples inside
/// `convert_mix`, the Unix engines inside their capture loops, and the two
/// copies had already started spelling the same arithmetic differently.
pub fn resample_linear(mono: &[f32], step: f64) -> Vec<i16> {
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

/// A persistent capture engine, armed once at startup.
///
/// On Windows nothing names this trait - the app holds its concrete
/// `AudioEngine` and calls its inherent method. That asymmetry is deliberate:
/// the trait is the port's specification for the Unix engines, and the portable
/// CLI is what actually calls it through a trait object.
#[allow(dead_code)]
pub trait Audio: Send {
    /// Begin a capture session.
    ///
    /// Chunks of PCM are streamed to `tx` as they are captured; the session
    /// ends when `stop` is set, when `max_seconds` elapses, or when the
    /// backend's voice-activity detector decides the user stopped talking
    /// (`vad_silence_ms` of quiet below `vad_rms_threshold`). The returned
    /// receiver yields exactly one `Ok(())` or one `Err` describing why the
    /// session ended.
    fn capture_to_channel(
        &self,
        stop: Arc<AtomicBool>,
        max_seconds: u32,
        vad_silence_ms: u32,
        vad_rms_threshold: f64,
        tx: Sender<Vec<i16>>,
    ) -> Result<Receiver<Result<(), String>>, String>;
}

/// Wrap mono i16 samples in a minimal PCM WAV container.
///
/// Shared rather than per-platform because the REST fallback uploads exactly
/// this container on every platform, and a malformed header is the kind of bug
/// you want fixed once.
pub fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let byte_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + byte_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + byte_len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(byte_len as u32).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The windows advance by the audio each chunk carries, not by a literal
    /// tick length: a chunk that ran long still counts the time it covered,
    /// so a stalled loop ends on the configured silence instead of holding
    /// the microphone until max_seconds.
    #[test]
    fn the_silence_window_counts_the_audio_the_chunk_carries() {
        let mut silence = SilenceWindows::new();
        // Voice first, so the since-voice window is the one in play.
        assert!(
            !silence.advance(1_280, 1.0, 0.01, 3_000),
            "voice is not silence"
        );
        // A chunk twice the usual size counts twice the time: 80 ms each, so
        // the three-second threshold takes 38 of them, not the 75 a fixed
        // 40 ms tick would need.
        let mut ticks = 0;
        loop {
            ticks += 1;
            if silence.advance(1_280, 0.0, 0.01, 3_000) {
                break;
            }
            assert!(
                ticks < 100,
                "three seconds of silence never ended the session"
            );
        }
        assert_eq!(ticks, 38, "3000 ms of 80 ms chunks");
    }

    /// A tick that consumed a second of device audio has to count a second
    /// of silence - the Unix engine's worst case. Counting the tick instead
    /// reaches the configured silence only after fifty such ticks, and in
    /// the meantime nothing ends the dictation but MAX_SECONDS.
    #[test]
    fn a_stalled_tick_still_counts_the_audio_it_covered() {
        let mut silence = SilenceWindows::new();
        // Voice first, so the since-voice window is the one in play.
        assert!(
            !silence.advance(16_000, 1.0, 0.01, 3_000),
            "voice is not silence"
        );
        let mut ticks = 0;
        loop {
            ticks += 1;
            if silence.advance(16_000, 0.0, 0.01, 3_000) {
                break;
            }
            assert!(
                ticks < 100,
                "three seconds of silence never ended the session"
            );
        }
        assert_eq!(
            ticks, 3,
            "one second of audio per tick is one second of silence"
        );
    }

    /// The ordinary chunk still ends on the configured silence, so the
    /// audio-time accounting did not change the normal case: 40 ms chunks
    /// (Windows) and 20 ms ticks (Unix) both work out.
    #[test]
    fn ordinary_chunks_still_end_on_the_configured_silence() {
        for (samples, expected) in [(640, 75), (320, 150)] {
            let mut silence = SilenceWindows::new();
            assert!(!silence.advance(samples, 1.0, 0.01, 3_000));
            let mut ticks = 0;
            loop {
                ticks += 1;
                if silence.advance(samples, 0.0, 0.01, 3_000) {
                    break;
                }
                assert!(ticks < 1_000);
            }
            assert_eq!(
                ticks,
                expected,
                "3000 ms of {} ms silence",
                audio_ms(samples)
            );
        }
    }

    /// Nothing said at all ends the session after the shared no-speech limit,
    /// counted in audio time the same way for both engines' chunk sizes.
    #[test]
    fn ten_seconds_of_no_speech_ends_the_session() {
        for (samples, expected) in [(640, 250), (320, 500)] {
            let mut silence = SilenceWindows::new();
            let mut ticks = 0;
            loop {
                ticks += 1;
                if silence.advance(samples, 0.0, 0.01, 3_000) {
                    break;
                }
                assert!(ticks < 2_000);
            }
            assert_eq!(
                ticks,
                expected,
                "10 000 ms of {} ms silence",
                audio_ms(samples)
            );
        }
    }

    /// A voice chunk restarts the since-voice window, so a pause that never
    /// reached the threshold is not carried into the next one.
    #[test]
    fn voice_resets_the_silence_window() {
        let mut silence = SilenceWindows::new();
        assert!(
            !silence.advance(640, 1.0, 0.01, 3_000),
            "voice is not silence"
        );
        // Two quiet chunks that never reach the threshold.
        assert!(!silence.advance(640, 0.0, 0.01, 3_000));
        assert!(!silence.advance(640, 0.0, 0.01, 3_000));
        // Voice again: the window restarts, so 75 fresh quiet chunks end the
        // dictation, not the 73 the carried-over 80 ms would leave.
        assert!(!silence.advance(640, 1.0, 0.01, 3_000));
        let mut quiet = 0;
        while !silence.advance(640, 0.0, 0.01, 3_000) {
            quiet += 1;
            assert!(quiet < 1_000);
        }
        assert_eq!(quiet + 1, 75, "the window restarted at the last voice");
    }

    /// The configured value is the threshold itself: `VAD_SILENCE_MS=0` ends
    /// the dictation on the first silent tick. A guard that treated zero as
    /// "detector off" left the recording running to MAX_SECONDS while the
    /// banner still promised that the configured silence would stop it.
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
}
