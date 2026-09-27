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

/// A persistent capture engine, armed once at startup.
///
/// On Windows nothing names this trait - the app holds its concrete
/// `AudioEngine`, and the impl below is the contract the Linux and macOS
/// engines must satisfy. That asymmetry is deliberate: the trait is the port's
/// specification, and the portable CLI is what actually calls it through a
/// trait object.
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