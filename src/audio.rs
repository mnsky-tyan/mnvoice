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

use crate::platform::audio::{resample_linear, resample_step, SilenceWindows, I16_SCALE};

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

/// The live capture session.
///
/// `format` is a copy, not a pointer: `GetMixFormat` returns a
/// `CoTaskMemAlloc`'d block that is released as soon as it has been copied and
/// read, so nothing here has to free it and `Drop` has nothing to do. The two
/// clients below release their own handles.
struct EngineState {
    client: IAudioClient,
    capture_client: IAudioCaptureClient,
    format: WAVEFORMATEX,
    kind: SampleKind,
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
    // The format is copied out for `EngineState`, but `Initialize` still gets
    // the original allocation - not this copy. WASAPI reads the format's `cbSize`
    // bytes, and an extensible mix format's `WAVEFORMATEXTENSIBLE` is 40 bytes
    // where `WAVEFORMATEX` is 18: handing `Initialize` a `WAVEFORMATEX` on the
    // stack makes it read past the object. Passing the allocation is what the
    // caller is entitled to, and `Initialize` copies what it needs, so the
    // allocation is released straight afterwards - every exit path frees it
    // exactly once, and no early return can leak it.
    let format = *mix_ptr;
    let kind = match sample_kind(&format, mix_ptr) {
        Ok(kind) => kind,
        Err(e) => {
            unsafe { CoTaskMemFree(Some(mix_ptr as *const std::ffi::c_void)) };
            return Err(e);
        }
    };

    if let Err(e) = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 0, 0, mix_ptr, None) {
        unsafe { CoTaskMemFree(Some(mix_ptr as *const std::ffi::c_void)) };
        return Err(format!("audio client init ({e})"));
    }
    unsafe { CoTaskMemFree(Some(mix_ptr as *const std::ffi::c_void)) };

    let capture_client: IAudioCaptureClient = client
        .GetService()
        .map_err(|e| format!("capture client ({e})"))?;

    Ok(EngineState {
        client,
        capture_client,
        format,
        kind,
    })
}

fn audio_worker_loop(request_rx: Receiver<CaptureRequest>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        // Pre-initialize WASAPI in standby state! (Paid once at startup)
        //
        // The reason for the failure is kept, not discarded into a bool. A
        // user whose Windows Audio service is stopped and a user with no
        // microphone both land here, and the four messages init_wasapi builds
        // are the only thing that tells them apart; collapsing them to None
        // made both report "failed to initialize microphone".
        let mut state = init_wasapi();

        while let Ok(req) = request_rx.recv() {
            if state.is_err() {
                state = init_wasapi();
            }

            let res = match state.as_mut() {
                Ok(engine) => {
                    let r = run_session(engine, &req);
                    if r.is_err() {
                        // Reset state on error so next session can re-init
                        state = Err("previous session ended in error".into());
                    }
                    r
                }
                // The reason init_wasapi gave, not a generic one: it already
                // names which of the four ways the backend refused.
                Err(why) => Err(why.clone()),
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
    // The frame width comes from `frame_bytes`, the same definition the decoder
    // below uses, so the buffer sizing and the decode cannot disagree.
    let block_align = frame_bytes(&engine.format);
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
            // No tail to carry here, unlike the Unix engine. The length is
            // `frames * block_align` and `block_align` is computed by the same
            // `frame_bytes` the decoder uses, so a whole number of frames is
            // what `convert_mix` is handed; `GetBuffer` reports a frame count,
            // so a partial frame cannot arrive on this path. A carry was added
            // and then removed: by construction it could never become non-empty,
            // so it only added a heap copy and a full memcpy to the 2 ms capture
            // loop for a branch that never engaged. The length guard's reach
            // is the declared stride itself: a device that declares an
            // nBlockAlign smaller than its own fields imply is refused there,
            // which is a claim about one WAVEFORMATEX rather than about the
            // stream. A format that changes mid-session is not something this
            // guard can catch - both operands derive from the startup copy, so
            // a change would have to be noticed by re-reading the packet's
            // format instead, which is a separate change. That is also why the
            // guard has no cost on this path: sizing the slice and decoding it
            // both go through the same `frame_bytes`, so the multiple of the
            // stride holds by construction.
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
                // The format `convert_mix` checks is the startup copy, fixed
                // for the session, so an unreadable format means this device
                // was never readable - not that it changed under us. The abort
                // is the point: without it an unreadable format captures
                // nothing at all, silently, until the session ends reporting
                // "No speech detected".
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

/// The width of one interleaved frame in bytes, computed the one way.
///
/// `nBlockAlign` is what the format declares, and a zero value is a broken
/// declaration: the fallback is the width the format's own fields imply
/// (`channels * sample_bytes`), so a caller that sizes its buffer from this
/// function and a consumer that decodes with it agree by construction. They
/// used to spell the rule twice, and disagreed exactly at `nBlockAlign == 0`,
/// where one said 1 byte and the other `channels * sample_bytes`.
///
/// Both widths are read from `format` rather than passed in, so no caller can
/// hand the helper a pair that contradicts the format it is sizing or decoding.
///
/// A zero `wBitsPerSample` would make the fallback width zero, so the byte
/// width is at least 1: with a zero stride the sizing site hands `convert_mix`
/// an empty slice, and decoding it would divide by the same zero stride. The
/// clamp keeps the stride non-zero so the session ends in `convert_mix`'s
/// "invalid mix format" refusal instead - still a loud abort, which is the
/// point. The channel count is not clamped, and a format declaring zero
/// channels does yield a zero stride: that slice is read empty and
/// `convert_mix` refuses `channels == 0` before any decode, so it is loud
/// rather than silent. The clamp does not make an unreadable format readable.
/// It only decides which refusal catches one: with a zero byte width the stride
/// stays non-zero and `convert_mix` refuses "invalid mix format"; the
/// zero-channel format is caught by its `channels == 0` refusal before any
/// decode. Both end the session loudly, and neither reads out of bounds first.
fn frame_bytes(format: &WAVEFORMATEX) -> usize {
    let declared = format.nBlockAlign as usize;
    if declared > 0 {
        return declared;
    }
    let channels = format.nChannels as usize;
    let sample_bytes = (format.wBitsPerSample as usize / 8).max(1);
    channels * sample_bytes
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

    let frame = frame_bytes(format);
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

    let step = resample_step(rate as u32);
    Ok(resample_linear(&mono, step))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::audio::SAMPLE_RATE;

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

    /// `nBlockAlign` must actually be consulted, not just present.
    ///
    /// The previous version of this test could not fail on the change it
    /// named: it set `nBlockAlign: 8` on a 2-channel 32-bit format, and 8 is
    /// exactly what the `channels * sample_bytes` fallback computes, so the
    /// two branches agreed and the assertion held either way.
    ///
    /// A block align SMALLER than one frame is the case that separates them,
    /// and it needs no floating-point comparison to be decisive: honoured, it
    /// trips the "unexpected capture buffer size" guard; ignored, the fallback
    /// stride reads the same bytes without complaint.
    #[test]
    fn block_align_sets_frame_stride_when_provided() {
        let convert = |n_block_align: u16, bytes: usize| {
            let f = WAVEFORMATEX {
                nChannels: 2,
                nSamplesPerSec: SAMPLE_RATE,
                wBitsPerSample: 32,
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nBlockAlign: n_block_align,
                ..Default::default()
            };
            let raw = vec![0u8; bytes];
            convert_mix(&raw, &f, SampleKind::Float)
        };

        // One frame is 2 channels * 4 bytes = 8. A 4-byte block align claims
        // half a frame, which cannot be read, and saying so is the whole point
        // of consulting the field.
        assert!(
            convert(4, 40).is_err(),
            "a block align below one frame must be rejected, not silently \
             replaced by the channels * sample_bytes fallback"
        );

        // The fallback still works when the field is absent...
        assert!(convert(0, 80).is_ok(), "nBlockAlign 0 must use the fallback");
        // ...and so does a field that agrees with it.
        assert!(convert(8, 80).is_ok(), "a matching block align must convert");

        // A stride wider than one frame is padding the device inserted, and is
        // accepted: only a stride too small to hold a frame is an error.
        assert!(
            convert(12, 120).is_ok(),
            "a padded frame stride must still convert"
        );
    }

    /// What `frame_bytes` returns, and that a buffer sized by it decodes whole.
    ///
    /// The sizing site lives in `run_session`, which is unsafe and device-bound,
    /// so nothing here can observe whether that loop still calls this helper -
    /// what this pins is the helper's own values and the decode that consumes
    /// them. They used to be spelled twice (`nBlockAlign.max(1)` against
    /// `nBlockAlign > 0 ? nBlockAlign : channels * sample_bytes`) and differed
    /// exactly where a broken declaration lands, where one side said 1 byte per
    /// frame and the other 8 for this test's stereo float32 format: a packet
    /// sized the old way either failed the length check outright or decoded one
    /// frame for every eight the device had captured.
    #[test]
    fn a_buffer_sized_by_frame_bytes_converts_for_a_zero_block_align_format() {
        let fmt = |n_block_align: u16| WAVEFORMATEX {
            nChannels: 2,
            nSamplesPerSec: SAMPLE_RATE,
            wBitsPerSample: 32,
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nBlockAlign: n_block_align,
            ..Default::default()
        };

        // A zero block align is a broken declaration: the width the format's
        // own fields imply is what this function must return, never 1 byte.
        assert_eq!(
            frame_bytes(&fmt(0)),
            8,
            "a zero nBlockAlign must fall back to channels * sample_bytes"
        );
        // A declared width is honoured.
        assert_eq!(frame_bytes(&fmt(8)), 8, "a declared stride wins");
        assert_eq!(frame_bytes(&fmt(12)), 12, "padding is kept");

        // A buffer built the way the capture loop builds one - `frames *
        // frame_bytes` - must be accepted for a zero-block-align format rather
        // than reported as a bad length.
        let f = fmt(0);
        let stride = frame_bytes(&f);
        let raw = vec![0u8; 2 * stride];
        let converted = convert_mix(&raw, &f, SampleKind::Float);
        assert!(
            converted.is_ok(),
            "a buffer sized by frame_bytes must convert, got {converted:?}"
        );
        assert_eq!(converted.unwrap().len(), 2, "both frames decoded");
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
