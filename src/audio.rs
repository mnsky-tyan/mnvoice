// Microphone capture via WASAPI.
// Uses a persistent standby audio thread to pay the ~500ms WASAPI kernel initialization
// cost once at application startup. When Alt+Space is pressed, client.Start() executes in ~4ms,
// capturing the first audio frame within ~15ms so no speech is ever truncated.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows::core::GUID;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// How the mix format encodes one sample, resolved from the format tag alone
/// (for `WAVE_FORMAT_EXTENSIBLE`, from its sub-format GUID). The bit width
/// never decides this: a 32-bit stream is plain integer PCM unless the tag
/// says float, and treating width as the tiebreaker silently returned
/// `f32::from_le_bytes` for integer samples.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SampleKind {
    Int,
    Float,
}

fn sample_kind(format: &WAVEFORMATEX, mix_ptr: *const WAVEFORMATEX) -> Result<SampleKind, String> {
    match format.wFormatTag {
        WAVE_FORMAT_PCM => Ok(SampleKind::Int),
        WAVE_FORMAT_IEEE_FLOAT => Ok(SampleKind::Float),
        WAVE_FORMAT_EXTENSIBLE => {
            // The sub-format GUID lives after the base WAVEFORMATEX; GetMixFormat
            // allocates the full WAVEFORMATEXTENSIBLE, so the pointer is wide
            // enough to read it.
            let ext = unsafe { &*(mix_ptr as *const WAVEFORMATEXTENSIBLE) };
            match ext.SubFormat {
                KSDATAFORMAT_SUBTYPE_PCM => Ok(SampleKind::Int),
                KSDATAFORMAT_SUBTYPE_IEEE_FLOAT => Ok(SampleKind::Float),
                other => Err(format!("unsupported mix sub-format: {other:?}")),
            }
        }
        other => Err(format!("unsupported mix format tag: {other}")),
    }
}

pub use crate::platform::audio::SAMPLE_RATE;
use crate::platform::audio::{resample_linear, SilenceWindows, I16_SCALE};

struct CaptureRequest {
    stop: Arc<AtomicBool>,
    max_seconds: u32,
    vad_silence_ms: u32,
    vad_rms_threshold: f64,
    tx: Sender<Vec<i16>>,
    done_tx: Sender<Result<(), String>>,
}

#[derive(Clone)]
pub struct AudioEngine {
    request_tx: Sender<CaptureRequest>,
}

impl AudioEngine {
    pub fn start() -> Self {
        let (request_tx, request_rx) = channel::<CaptureRequest>();
        thread::spawn(move || {
            audio_worker_loop(request_rx);
        });
        Self { request_tx }
    }

    pub fn capture_to_channel(
        &self,
        stop: Arc<AtomicBool>,
        max_seconds: u32,
        vad_silence_ms: u32,
        vad_rms_threshold: f64,
        tx: Sender<Vec<i16>>,
    ) -> Result<Receiver<Result<(), String>>, String> {
        let (done_tx, done_rx) = channel();
        self.request_tx
            .send(CaptureRequest {
                stop,
                max_seconds,
                vad_silence_ms,
                vad_rms_threshold,
                tx,
                done_tx,
            })
            .map_err(|_| "audio worker thread unavailable".to_string())?;
        Ok(done_rx)
    }
}

struct EngineState {
    client: IAudioClient,
    capture_client: IAudioCaptureClient,
    format: WAVEFORMATEX,
    kind: SampleKind,
    mix_ptr: *mut WAVEFORMATEX,
}

impl Drop for EngineState {
    fn drop(&mut self) {
        unsafe {
            if !self.mix_ptr.is_null() {
                CoTaskMemFree(Some(self.mix_ptr as *const std::ffi::c_void));
            }
        }
    }
}

unsafe fn init_wasapi() -> Result<EngineState, String> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .map_err(|e| format!("audio backend unavailable ({e})"))?;
    let device = enumerator
        .GetDefaultAudioEndpoint(eCapture, eConsole)
        .map_err(|e| format!("no default microphone ({e})"))?;

    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("cannot open microphone ({e})"))?;

    let mix_ptr = client
        .GetMixFormat()
        .map_err(|e| format!("GetMixFormat ({e})"))?;
    let format = *mix_ptr;
    let kind = match sample_kind(&format, mix_ptr) {
        Ok(kind) => kind,
        Err(e) => {
            unsafe { CoTaskMemFree(Some(mix_ptr as *const std::ffi::c_void)) };
            return Err(e);
        }
    };

    client
        .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 0, 0, mix_ptr, None)
        .map_err(|e| format!("audio client init ({e})"))?;

    let capture_client: IAudioCaptureClient = client
        .GetService()
        .map_err(|e| format!("capture client ({e})"))?;

    Ok(EngineState {
        client,
        capture_client,
        format,
        kind,
        mix_ptr,
    })
}

fn audio_worker_loop(request_rx: Receiver<CaptureRequest>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        // Pre-initialize WASAPI in standby state! (Paid once at startup)
        let mut state: Option<EngineState> = init_wasapi().ok();

        while let Ok(req) = request_rx.recv() {
            if state.is_none() {
                state = init_wasapi().ok();
            }

            let res = if let Some(engine) = &mut state {
                let r = run_session(engine, &req);
                if r.is_err() {
                    // Reset state on error so next session can re-init
                    state = None;
                }
                r
            } else {
                Err("failed to initialize microphone".into())
            };

            let _ = req.done_tx.send(res);
        }
    }
}

unsafe fn run_session(engine: &mut EngineState, req: &CaptureRequest) -> Result<(), String> {
    // Ultra-fast start: client was already initialized in standby! Takes ~4ms.
    engine.client.Start().map_err(|e| format!("capture start ({e})"))?;

    let deadline = Instant::now() + Duration::from_secs(req.max_seconds as u64);
    let mut sample_buf: Vec<i16> = Vec::with_capacity(3200);

    let mut silence = SilenceWindows::new();
    let block_align = engine.format.nBlockAlign.max(1) as usize;
    let vad_silence_ms = req.vad_silence_ms;
    let vad_rms_threshold = req.vad_rms_threshold;

    while !req.stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        let packet = engine
            .capture_client
            .GetNextPacketSize()
            .map_err(|e| format!("capture read ({e})"))?;
        if packet == 0 {
            thread::sleep(Duration::from_millis(2));
            continue;
        }

        let mut frames = packet;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut dwflags = 0u32;
        engine
            .capture_client
            .GetBuffer(&mut ptr, &mut frames, &mut dwflags, None, None)
            .map_err(|e| format!("capture buffer ({e})"))?;

        if frames > 0 && !ptr.is_null() {
            let bytes = std::slice::from_raw_parts(ptr, frames as usize * block_align);
            // Convert while the raw buffer is still borrowed, then always
            // release it below before acting on the result.
            let converted = convert_mix(bytes, &engine.format, engine.kind);
            engine
                .capture_client
                .ReleaseBuffer(frames)
                .map_err(|e| format!("capture release ({e})"))?;
            match converted {
                Ok(s) => sample_buf.extend_from_slice(&s),
                // A capture format the mixer cannot translate (the device
                // switched or a stream reset landed mid-dictation) would
                // otherwise capture nothing, silently, until the session ends
                // with "No speech detected". Stop and report instead.
                Err(e) => {
                    let _ = engine.client.Stop();
                    let _ = engine.client.Reset();
                    return Err(format!(
                        "audio device delivered an unreadable format, session aborted: {e}"
                    ));
                }
            }
        } else {
            engine
                .capture_client
                .ReleaseBuffer(frames)
                .map_err(|e| format!("capture release ({e})"))?;
        }

        // Push 40ms chunks (640 samples) for ultra-low latency streaming
        while sample_buf.len() >= 640 {
            let chunk: Vec<i16> = sample_buf.drain(..640).collect();

            // Compute RMS for Voice Activity Detection. The metric is shared
            // with the Unix engines (platform::audio::rms_of) so a threshold
            // means the same thing on every platform.
            let rms = crate::platform::audio::rms_of(&chunk);

            // Advance the voice-activity windows by the audio the chunk
            // carries (see platform::audio::audio_ms), not by loop ticks.
            if silence.advance(chunk.len(), rms, vad_rms_threshold, vad_silence_ms) {
                // silence after speech, or 10s with no speech at all
                req.stop.store(true, Ordering::SeqCst);
            }

            if req.tx.send(chunk).is_err() {
                req.stop.store(true, Ordering::SeqCst);
                break;
            }
        }
    }

    let _ = engine.client.Stop();
    let _ = engine.client.Reset();

    if !sample_buf.is_empty() {
        let _ = req.tx.send(sample_buf);
    }

    Ok(())
}

fn convert_mix(raw: &[u8], format: &WAVEFORMATEX, kind: SampleKind) -> Result<Vec<i16>, String> {
    let channels = format.nChannels as usize;
    let rate = format.nSamplesPerSec as usize;
    let bits = format.wBitsPerSample as usize;
    let is_float = kind == SampleKind::Float;
    let sample_bytes = bits / 8;
    if sample_bytes == 0 || channels == 0 {
        return Err("invalid mix format".into());
    }

    // The read width below MUST be derived from sample_bytes, not assumed:
    // the catch-all used to read four bytes for every non-16-bit format, so a
    // 24-bit or 8-bit mix (or 16-bit float) read past the buffer and panicked
    // on the last sample - and with the release profile's panic = "abort"
    // that is the whole process dying, not one capture thread. Anything this
    // function cannot represent exactly is an Err, never a best-effort read.
    let supported = if is_float {
        sample_bytes == 4
    } else {
        sample_bytes == 2 || sample_bytes == 4
    };
    if !supported {
        return Err(format!(
            "unsupported mix format: {bits}-bit {} (expected 16-bit PCM, 32-bit PCM, or float32)",
            if is_float { "float" } else { "PCM" }
        ));
    }

    let frame = if format.nBlockAlign > 0 {
        format.nBlockAlign as usize
    } else {
        channels * sample_bytes
    };
    if frame < channels * sample_bytes || !raw.len().is_multiple_of(frame) {
        return Err("unexpected capture buffer size".into());
    }
    let frames = raw.len() / frame;

    let mut mono: Vec<f32> = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * frame;
        let mut acc = 0f32;
        for c in 0..channels {
            let off = base + c * sample_bytes;
            let v = if is_float {
                f32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]])
            } else if sample_bytes == 2 {
                i16::from_le_bytes([raw[off], raw[off + 1]]) as f32 / I16_SCALE
            } else {
                i32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]) as f32
                    / 2147483648.0
            };
            acc += v;
        }
        mono.push(acc / channels as f32);
    }

    let step = rate as f64 / SAMPLE_RATE as f64;
    Ok(resample_linear(&mono, step))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mix format this converter cannot represent must be an Err, never a
    /// best-effort read: the old catch-all read four bytes for every
    /// non-16-bit format, so a 24-bit or 8-bit mix panicked on the last
    /// sample - and with the release profile's panic = "abort" that is the
    /// whole process dying. WAVEFORMATEX is field-for-field what WASAPI
    /// hands over; only the fields convert_mix reads are populated.
    #[test]
    fn an_unsupported_mix_width_is_an_err_not_an_overread() {
        let mut f = WAVEFORMATEX {
            nChannels: 2,
            nSamplesPerSec: 48000,
            ..Default::default()
        };

        // 24-bit PCM: sample_bytes == 3, not float. The old code read four
        // bytes per three-byte sample and panicked at the buffer end.
        f.wBitsPerSample = 24;
        f.wFormatTag = WAVE_FORMAT_PCM;
        let raw = vec![0u8; 3 * 2 * 10]; // 10 stereo 24-bit frames
        assert!(
            convert_mix(&raw, &f, SampleKind::Int).is_err(),
            "24-bit must be refused"
        );

        // 8-bit PCM: same catch-all, same over-read.
        f.wBitsPerSample = 8;
        let raw = vec![0u8; 2 * 10]; // 10 stereo 8-bit frames
        assert!(
            convert_mix(&raw, &f, SampleKind::Int).is_err(),
            "8-bit must be refused"
        );

        // 16-bit float: is_float but sample_bytes == 2 - the old code read
        // four bytes per two-byte sample.
        f.wBitsPerSample = 16;
        f.wFormatTag = WAVE_FORMAT_IEEE_FLOAT;
        let raw = vec![0u8; 2 * 2 * 10];
        assert!(
            convert_mix(&raw, &f, SampleKind::Float).is_err(),
            "16-bit float must be refused"
        );

        // The supported formats still convert: 16-bit PCM and float32.
        f.wFormatTag = WAVE_FORMAT_PCM;
        f.wBitsPerSample = 16;
        let raw = vec![0u8; 2 * 2 * 10];
        assert!(
            convert_mix(&raw, &f, SampleKind::Int).is_ok(),
            "16-bit PCM must convert"
        );
        f.wFormatTag = WAVE_FORMAT_IEEE_FLOAT;
        f.wBitsPerSample = 32;
        let raw = vec![0u8; 4 * 2 * 10];
        assert!(
            convert_mix(&raw, &f, SampleKind::Float).is_ok(),
            "float32 must convert"
        );
    }

    /// The sample kind comes from the format tag, never from the bit width: a
    /// 32-bit PCM mix decodes as integer, exactly as its guard advertises. The
    /// old `bits == 32` heuristic forced every 32-bit stream onto the float
    /// decoder, so integer PCM 0x3f800000 (about +0.496 full scale) came back
    /// as 1.0 with no error.
    #[test]
    fn a_32bit_pcm_mix_decodes_as_integer_not_float() {
        let f = WAVEFORMATEX {
            nChannels: 1,
            nSamplesPerSec: SAMPLE_RATE,
            wBitsPerSample: 32,
            wFormatTag: WAVE_FORMAT_PCM,
            ..Default::default()
        };

        let raw = [0x00u8, 0x00, 0x80, 0x3f];
        let out = convert_mix(&raw, &f, SampleKind::Int).expect("32-bit PCM must convert");
        assert_eq!(out.len(), 1);
        let expected = 0x3f80_0000i32 as f32 / 2147483648.0 * i16::MAX as f32;
        assert!(
            (out[0] as f32 - expected).abs() < 1.0,
            "32-bit PCM sample decoded as {} (expected about {expected})",
            out[0]
        );
    }

    #[test]
    fn block_align_sets_frame_stride_when_provided() {
        let f = WAVEFORMATEX {
            nChannels: 2,
            nSamplesPerSec: SAMPLE_RATE,
            wBitsPerSample: 32,
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nBlockAlign: 8, // 2 channels * 4 bytes
            ..Default::default()
        };
        let raw = vec![0u8; 8 * 10]; // 10 frames
        assert!(convert_mix(&raw, &f, SampleKind::Float).is_ok());
    }

    /// `sample_kind` reads the sub-format GUID for WAVE_FORMAT_EXTENSIBLE, so a
    /// float mix delivered in that shape is not mistaken for integer PCM, and
    /// an unknown sub-format is an Err rather than a best-effort read.
    #[test]
    fn extensible_mixes_resolve_their_sub_format() {
        let mut ext = WAVEFORMATEXTENSIBLE::default();
        ext.Format.nChannels = 2;
        ext.Format.nSamplesPerSec = 48000;
        ext.Format.wBitsPerSample = 32;
        ext.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE;

        ext.SubFormat = KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        assert_eq!(
            sample_kind(&ext.Format, &ext as *const _ as *const WAVEFORMATEX).unwrap(),
            SampleKind::Float
        );

        ext.SubFormat = KSDATAFORMAT_SUBTYPE_PCM;
        assert_eq!(
            sample_kind(&ext.Format, &ext as *const _ as *const WAVEFORMATEX).unwrap(),
            SampleKind::Int
        );

        ext.SubFormat = GUID::from_u128(0xdead_beef_dead_beef_dead_beef_dead_beef);
        assert!(
            sample_kind(&ext.Format, &ext as *const _ as *const WAVEFORMATEX).is_err(),
            "unknown sub-format must be refused"
        );
    }
}
