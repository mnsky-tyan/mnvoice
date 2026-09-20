// Microphone capture via WASAPI. We ask the shared-mode audio engine for
// 16 kHz mono 16-bit PCM directly (the engine resamples); if the engine
// refuses, we take the mix format and convert in software.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::*;
use windows::Win32::Media::Audio::Endpoints::*;
use windows::Win32::System::Com::*;

pub const SAMPLE_RATE: u32 = 16_000;

// Shared-mode capture at a non-mix format needs the engine's converter.
const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;

/// Cheap pre-flight: is the default capture endpoint muted (or missing)?
/// Runs before any hotkey is registered so a blocked mic cannot leave the
/// Esc hotkey hijacked.
pub fn preflight() -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("audio backend unavailable ({e})"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .map_err(|e| format!("no default microphone ({e})"))?;
        if let Ok(vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
            if let Ok(muted) = vol.GetMute() {
                if muted.as_bool() {
                    return Err("microphone is muted in Windows (unmute it in Settings > System > Sound, or the mic-mute key)".into());
                }
            }
        }
        Ok(())
    }
}

/// Records until `stop` is set or `max_seconds` elapses.
/// Returns mono 16 kHz i16 samples.
pub fn capture(stop: &AtomicBool, max_seconds: u32) -> Result<Vec<i16>, String> {
    unsafe {
        // COM may already be initialized on this thread; that is fine.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("audio backend unavailable ({e})"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .map_err(|e| format!("no default microphone ({e})"))?;

        // Fail fast with a clear message when the endpoint is muted;
        // recording digital silence would only confuse.
        if let Ok(vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
            if let Ok(muted) = vol.GetMute() {
                if muted.as_bool() {
                    return Err("microphone is muted in Windows (unmute it in Settings > System > Sound, or the mic-mute key)".into());
                }
            }
        }
        let mut client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("cannot open microphone ({e})"))?;

        let desired = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 1,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * 2,
            nBlockAlign: 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // Preferred: ask the shared-mode engine to convert to 16 kHz mono
        // 16-bit. IsFormatSupported is not a reliable gate (it can refuse a
        // format that Initialize converts to happily), so just try it.
        let (format, native, mix_ptr) = if client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                0,
                0,
                &desired,
                None,
            )
            .is_ok()
        {
            (desired, true, None)
        } else {
            // Fall back to the device mix format on a fresh client and
            // convert in software. Keep the pointer, not a copy: an
            // extensible mix format carries 22 extra bytes that a copy
            // would truncate.
            client = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("cannot open microphone ({e})"))?;
            let mix_ptr = client
                .GetMixFormat()
                .map_err(|e| format!("GetMixFormat ({e})"))?;
            let mix = *mix_ptr;
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 0, 0, mix_ptr, None)
                .map_err(|e| format!("audio client init ({e})"))?;
            (mix, false, Some(mix_ptr))
        };
        let block_align = format.nBlockAlign.max(1) as usize;

        let capture_client: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| format!("capture client ({e})"))?;
        client.Start().map_err(|e| format!("capture start ({e})"))?;

        let deadline = Instant::now() + Duration::from_secs(max_seconds as u64);
        let mut raw: Vec<u8> = Vec::new();
        while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            let packet = capture_client
                .GetNextPacketSize()
                .map_err(|e| format!("capture read ({e})"))?;
            if packet == 0 {
                thread::sleep(Duration::from_millis(4));
                continue;
            }
            let mut frames = packet;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut dwflags = 0u32;
            capture_client
                .GetBuffer(&mut ptr, &mut frames, &mut dwflags, None, None)
                .map_err(|e| format!("capture buffer ({e})"))?;
            if frames > 0 && !ptr.is_null() {
                let bytes = std::slice::from_raw_parts(ptr, frames as usize * block_align);
                raw.extend_from_slice(bytes);
            }
            capture_client
                .ReleaseBuffer(frames)
                .map_err(|e| format!("capture release ({e})"))?;
        }
        let _ = client.Stop();
        if let Some(p) = mix_ptr {
            CoTaskMemFree(Some(p as *const std::ffi::c_void));
        }

        let samples = if native {
            raw.chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect()
        } else {
            convert_mix(&raw, &format)?
        };

        // Digital silence means the mic is disabled or blocked; say so
        // instead of sending a silent clip to the API.
        let peak = samples.iter().map(|&s| s.unsigned_abs()).max().unwrap_or(0);
        if peak < 50 {
            return Err("microphone captured only silence (check the input device and Windows mic privacy settings)".into());
        }

        Ok(samples)
    }
}

/// Software fallback: arbitrary mix format -> mono 16 kHz i16.
fn convert_mix(raw: &[u8], fmt: &WAVEFORMATEX) -> Result<Vec<i16>, String> {
    let channels = fmt.nChannels.max(1) as usize;
    let rate = fmt.nSamplesPerSec.max(1) as usize;
    let sample_bytes = (fmt.wBitsPerSample / 8).max(1) as usize;
    let bits = fmt.wBitsPerSample;
    let tag = fmt.wFormatTag;

    let is_float = match tag {
        1 => false, // PCM
        3 => true,  // IEEE float
        0xFFFE => {
            // WAVE_FORMAT_EXTENSIBLE: the real kind lives in SubFormat.
            let ext_ptr = fmt as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE;
            let sub = unsafe { (*ext_ptr).SubFormat };
            sub.data1 == 3
        }
        t => return Err(format!("unsupported device format tag {t}")),
    };
    if is_float && sample_bytes != 4 {
        return Err(format!("unsupported float depth {bits} bits"));
    }
    if !is_float && sample_bytes != 2 && sample_bytes != 4 {
        return Err(format!("unsupported pcm depth {bits} bits"));
    }

    let frame = channels * sample_bytes;
    if frame == 0 || raw.len() % frame != 0 {
        return Err("unexpected capture buffer size".into());
    }
    let frames = raw.len() / frame;

    // Downmix to mono f32.
    let mut mono: Vec<f32> = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * frame;
        let mut acc = 0f32;
        for c in 0..channels {
            let off = base + c * sample_bytes;
            let v = if is_float {
                f32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]])
            } else if sample_bytes == 2 {
                i16::from_le_bytes([raw[off], raw[off + 1]]) as f32 / 32768.0
            } else {
                i32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]) as f32
                    / 2147483648.0
            };
            acc += v;
        }
        mono.push(acc / channels as f32);
    }

    // Linear resample to 16 kHz.
    let step = rate as f64 / SAMPLE_RATE as f64;
    let mut out = Vec::with_capacity((mono.len() as f64 / step) as usize + 1);
    let mut pos = 0f64;
    while (pos as usize) < mono.len() {
        let i = pos as usize;
        let frac = (pos - i as f64) as f32;
        let a = mono[i];
        let b = if i + 1 < mono.len() { mono[i + 1] } else { a };
        let v = (a + (b - a) * frac).clamp(-1.0, 1.0);
        out.push((v * 32767.0) as i16);
        pos += step;
    }
    Ok(out)
}

/// Wrap samples in a minimal RIFF/WAVE container (44-byte header).
pub fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut v = Vec::with_capacity(44 + data_len);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // channels
    v.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    v.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes()); // block align
    v.extend_from_slice(&16u16.to_le_bytes()); // bits
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}
