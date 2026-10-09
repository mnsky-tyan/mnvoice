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

/// The f32 scale for 16-bit PCM: `i16::MIN` maps to exactly -1.0, and the
/// missing +32768th step costs half a count of headroom at the top. Every
/// conversion from i16 to f32 in the crate shares this one constant (the
/// Windows engine's `convert_mix` and the Unix cpal callback), so a sample
/// captured on one platform means the same level everywhere - the two engines
/// once divided by different constants here, which is the drift this exists to
/// prevent. The f32-to-i16 direction is not covered: `resample_linear`
/// multiplies by `i16::MAX`, the positive extreme of the target type.
pub const I16_SCALE: f32 = 32768.0;

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

/// Device samples per provider sample, from the device's own capture rate.
///
/// Both engines resample with this ratio, and it passes through unclamped,
/// because the ratio carries the rate correction in both directions: a device
/// slower than the provider rate has to come out *longer* than it went in for
/// the audio to run at the rate the request declares. 8 kHz narrowband is 0.5
/// and doubles the sample count, and flooring it to pass-through would hand
/// the provider 16 kHz-labelled audio running at 8 kHz - a transcript an
/// octave out, with nothing anywhere reporting a problem. A floor hurts from
/// the other side too: 44.1 kHz gives 2.75625 and is very common and the
/// macOS built-in input's nominal rate, so a floor at 1.0, at 2.75625, at
/// 8 kHz's 0.5, or at the 3.0 a reviewer first proposed all silently decimate
/// real capture.
///
/// The one rate that cannot be resampled is 0, because `resample_linear`
/// divides by this to size its output buffer: `len / 0.0` saturates to
/// `usize::MAX`, after which `+ 1` wraps to zero and the fill loop spins at
/// position 0 pushing without bound until the allocation fails - and under
/// `[profile.release] panic = "abort"` that is a process kill rather than a
/// catchable error. A ratio below 1 is not that case, so the guard is the rate
/// itself, not a floor on the ratio. A rate-less device passes its samples
/// through one-for-one, the only honest answer when nothing says how fast it
/// ran.
///
/// The formula itself used to be spelled at both call sites (`audio.rs` and
/// `unix_audio.rs`); sharing it here means the guard cannot be applied to one
/// engine and forgotten on the other.
pub fn resample_step(device_rate: u32) -> f64 {
    if device_rate == 0 {
        return 1.0;
    }
    device_rate as f64 / SAMPLE_RATE as f64
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
            // open mic cannot hold the device for max_seconds. The comparison
            // goes through `silence_expired` like the branch above rather than
            // being written out again, so both windows keep one rule.
            self.no_speech_ms += audio_ms(samples);
            silence_expired(self.no_speech_ms, NO_SPEECH_LIMIT_MS as u32)
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

/// Root-mean-square level of a mono chunk, the value the local VAD compares
/// against its threshold.
///
/// Shared by both engines for the same reason `resample_linear` is: the energy
/// metric the silence detector depends on must not be spelled twice, or a
/// threshold tuned against one copy silently means something else on the other
/// platform. An empty chunk is 0.0 (silence), not NaN.
pub fn rms_of(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt()
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

    /// The energy metric both engines hand the silence detector: silence is
    /// 0.0 (never NaN, which would poison every comparison against the
    /// threshold), a full-scale square wave's RMS is its amplitude, and the
    /// result is the same value the two engines used to compute separately.
    #[test]
    fn the_shared_rms_metric_is_defined_for_every_chunk() {
        assert_eq!(rms_of(&[]), 0.0, "an empty chunk is silence, not NaN");
        assert_eq!(rms_of(&[0i16; 640]), 0.0);
        // A constant full-scale signal: RMS of a square wave is its
        // amplitude, and i16::MAX as f64 rounds to 32767.0.
        assert!((rms_of(&[i16::MAX; 64]) - 32767.0).abs() < 1.0);
        // Half amplitude -> half the RMS.
        assert!((rms_of(&[i16::MAX / 2; 64]) - 16383.5).abs() < 2.0);
        // Symmetry: negating every sample cannot change the level.
        assert_eq!(rms_of(&[1000, -2000, 3000]), rms_of(&[-1000, 2000, -3000]));
    }

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

    /// The amplitude comparison is strict (`rms > threshold`), so a chunk
    /// exactly *at* the threshold is silence. Both branches of `advance`
    /// return `false` for that single chunk, so the boundary only becomes
    /// observable through which window it advances: treated as silence, it
    /// feeds the no-speech window (`NO_SPEECH_LIMIT_MS`, 10 s); treated as
    /// voice, it opens the post-voice window (`vad_silence_ms`, 3 s here).
    /// Feeding the equality chunk and then only `vad_silence_ms` of quiet
    /// therefore separates the two: the session must still be running, and
    /// only the no-speech limit later ends it.
    #[test]
    fn a_chunk_exactly_at_the_threshold_counts_as_silence() {
        let mut silence = SilenceWindows::new();
        let mut chunks = 1u64;
        assert!(
            !silence.advance(640, 0.01, 0.01, 3_000),
            "rms equal to the threshold is not voice"
        );
        // 75 chunks of 40 ms is exactly `vad_silence_ms`. Only a session that
        // counted the equality chunk as voice ends here; a session that
        // counted it as silence is still inside the no-speech window.
        for tick in 0..75 {
            chunks += 1;
            assert!(
                !silence.advance(640, 0.0, 0.01, 3_000),
                "the equality chunk opened the post-voice window at tick {tick}"
            );
        }
        // And the equality chunk really did count as silence, rather than
        // being discarded: the no-speech limit arrives after it, not one
        // chunk later.
        loop {
            chunks += 1;
            if silence.advance(640, 0.0, 0.01, 3_000) {
                break;
            }
            assert!(chunks < 1_000, "the no-speech limit never arrived");
        }
        assert_eq!(
            chunks, 250,
            "10 s of 40 ms chunks, counting the equality chunk"
        );
    }

    /// The no-speech cutoff and the post-voice silence cutoff are the same rule
    /// with different limits, so a session that never hears anything must end
    /// on the shared boundary too, not on a separately written comparison.
    #[test]
    fn both_silence_windows_use_the_same_comparison() {
        for (since, limit, expired) in [
            (0u64, 3_000u32, false),
            (2_999, 3_000, false),
            (3_000, 3_000, true),
            (3_001, 3_000, true),
            (9_999, NO_SPEECH_LIMIT_MS as u32, false),
            (10_000, NO_SPEECH_LIMIT_MS as u32, true),
        ] {
            assert_eq!(
                silence_expired(since, limit),
                expired,
                "{since}ms against a {limit}ms limit"
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
        let out = resample_linear(&input, resample_step(44_100));
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
        let n = 44_100usize;
        let input: Vec<f32> = (0..n).map(|i| i as f32 / n as f32).collect();
        let step = resample_step(44_100);
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
    /// This drives the ratio through `resample_step`, the way both engines do,
    /// so a change there cannot leave the length the provider gets unpinned.
    #[test]
    fn an_8000hz_device_is_upsampled_to_the_provider_rate() {
        let input: Vec<f32> = (0..8_000).map(|i| i as f32 / 8_000.0).collect();
        let out = resample_linear(&input, resample_step(8_000));
        assert_eq!(out.len(), 16_000);
    }

    /// A device that reports no rate at all cannot be resampled: the zero step
    /// saturates `resample_linear`'s output-length division to `usize::MAX`,
    /// and under `panic = "abort"` the resulting unbounded push is a process
    /// kill, not a catchable error. The guard has to make the *result*
    /// bounded, so this drives the real function rather than only the ratio.
    #[test]
    fn a_device_reporting_no_rate_passes_its_samples_through() {
        assert_eq!(resample_step(0), 1.0);
        let input: Vec<f32> = (0..48_000)
            .map(|i| (i as f64 / 48_000.0).sin() as f32)
            .collect();
        let out = resample_linear(&input, resample_step(0));
        assert_eq!(out.len(), input.len());
    }

    /// Every real capture rate must keep its true ratio, in *both* directions.
    /// This test exists so a future "clamp it to be safer" change cannot
    /// quietly break real capture without failing here first.
    ///
    /// 8 kHz is the one below the provider rate that matters: its 0.5 ratio is
    /// what turns an 8 kHz headset into audio the 16 kHz request can describe,
    /// and flooring it to pass-through is that octave bug, not a safeguard.
    /// 44.1 kHz is the one above it that matters, at 2.75625, so no floor
    /// between the two - 0.5, 1.0, 2.75625 or 3.0 - may come back.
    #[test]
    fn no_capture_rate_is_clamped_away_from_its_true_ratio() {
        for (rate, want) in [
            (8_000u32, 0.5f64),
            (16_000, 1.0),
            (22_050, 1.378125),
            (44_100, 2.75625),
            (48_000, 3.0),
            (88_200, 5.5125),
            (96_000, 6.0),
            (192_000, 12.0),
        ] {
            assert_eq!(
                resample_step(rate),
                want,
                "rate {rate} was clamped away from its true ratio {want}"
            );
        }
    }
}
