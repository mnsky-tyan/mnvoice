// Microphone capture via WASAPI. We ask the shared-mode audio engine for
// 16 kHz mono 16-bit PCM directly (the engine resamples); if the engine
// refuses, we take the mix format and convert in software.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::Audio::Endpoints::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

pub const SAMPLE_RATE: u32 = 16_000;
pub const WM_APP_RECORDING_READY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 4;

// Shared-mode capture at a non-mix format needs the engine's converter.
const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;

/// Cheap pre-flight: is the default capture endpoint muted (or missing)?
#[allow(dead_code)]
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

/// Immediately starts WASAPI recording, notifies the UI thread that the mic is live,
/// and streams ~100ms packets into the provided mpsc Sender.
/// Includes local VAD to auto-stop when 2.0s of silence is detected after speaking.
pub fn capture_to_channel(
    stop: &AtomicBool,
    max_seconds: u32,
    tx: Sender<Vec<i16>>,
    hwnd_bits: usize,
) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("audio backend unavailable ({e})"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .map_err(|e| format!("no default microphone ({e})"))?;

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

        // Start hardware capture
        client.Start().map_err(|e| format!("capture start ({e})"))?;

        // Signal UI thread: MICROPHONE IS ACTUALLY LIVE AND RECORDING NOW!
        if hwnd_bits != 0 {
            let hwnd = HWND(hwnd_bits as *mut std::ffi::c_void);
            let _ = PostMessageW(hwnd, WM_APP_RECORDING_READY, WPARAM(0), LPARAM(0));
        }

        let deadline = Instant::now() + Duration::from_secs(max_seconds as u64);
        let mut sample_buf: Vec<i16> = Vec::with_capacity(3200);

        // VAD state: 2.0s silence threshold after speech
        let mut speech_started = false;
        let mut silence_ms = 0u32;
        let mut no_speech_ms = 0u32;

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
                if native {
                    for c in bytes.chunks_exact(2) {
                        sample_buf.push(i16::from_le_bytes([c[0], c[1]]));
                    }
                } else if let Ok(s) = convert_mix(bytes, &format) {
                    sample_buf.extend_from_slice(&s);
                }
            }
            capture_client
                .ReleaseBuffer(frames)
                .map_err(|e| format!("capture release ({e})"))?;

            // Push 40ms chunks (640 samples) for ultra-low latency streaming
            while sample_buf.len() >= 640 {
                let chunk: Vec<i16> = sample_buf.drain(..640).collect();

                // Compute RMS for Voice Activity Detection
                let sum_sq: f64 = chunk.iter().map(|&s| (s as f64) * (s as f64)).sum();
                let rms = (sum_sq / chunk.len() as f64).sqrt();

                if rms > 350.0 {
                    speech_started = true;
                    silence_ms = 0;
                } else if speech_started {
                    silence_ms += 40;
                    if silence_ms >= 2200 {
                        // 2.2s silence after speech -> auto-stop!
                        stop.store(true, Ordering::SeqCst);
                    }
                } else {
                    no_speech_ms += 40;
                    if no_speech_ms >= 10000 {
                        // 10s with no speech at all -> auto-stop!
                        stop.store(true, Ordering::SeqCst);
                    }
                }

                if tx.send(chunk).is_err() {
                    stop.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
        let _ = client.Stop();
        if let Some(p) = mix_ptr {
            CoTaskMemFree(Some(p as *const std::ffi::c_void));
        }

        if !sample_buf.is_empty() {
            let _ = tx.send(sample_buf);
        }

        Ok(())
    }
}

/// Fallback batch capture.
#[allow(dead_code)]
pub fn capture(stop: &AtomicBool, max_seconds: u32) -> Result<Vec<i16>, String> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
    let stop_clone = AtomicBool::new(stop.load(Ordering::SeqCst));
    let deadline = Instant::now() + Duration::from_secs(max_seconds as u64);

    let handle = thread::spawn(move || {
        let mut samples = Vec::new();
        while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
            samples.extend_from_slice(&chunk);
            if stop_clone.load(Ordering::SeqCst) || Instant::now() >= deadline {
                break;
            }
        }
        samples
    });

    let _ = capture_to_channel(stop, max_seconds, tx, 0);
    let samples = handle.join().unwrap_or_default();
    Ok(samples)
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
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    v.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}
