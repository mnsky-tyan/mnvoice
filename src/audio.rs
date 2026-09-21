// Microphone capture via WASAPI.
// Uses a persistent standby audio thread to pay the ~500ms WASAPI kernel initialization
// cost once at application startup. When Alt+Space is pressed, client.Start() executes in ~4ms,
// capturing the first audio frame within ~15ms so no speech is ever truncated.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

pub const SAMPLE_RATE: u32 = 16_000;
pub const WM_APP_RECORDING_READY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 4;

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

    let mut speech_started = false;
    let mut silence_ms = 0u32;
    let mut no_speech_ms = 0u32;
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
            if let Ok(s) = convert_mix(bytes, &engine.format) {
                sample_buf.extend_from_slice(&s);
            }
        }
        engine
            .capture_client
            .ReleaseBuffer(frames)
            .map_err(|e| format!("capture release ({e})"))?;

        // Push 40ms chunks (640 samples) for ultra-low latency streaming
        while sample_buf.len() >= 640 {
            let chunk: Vec<i16> = sample_buf.drain(..640).collect();

            // Compute RMS for Voice Activity Detection
            let sum_sq: f64 = chunk.iter().map(|&s| (s as f64) * (s as f64)).sum();
            let rms = (sum_sq / chunk.len() as f64).sqrt();

            if rms > vad_rms_threshold {
                speech_started = true;
                silence_ms = 0;
            } else if speech_started {
                silence_ms += 40;
                if silence_ms >= vad_silence_ms {
                    // silence after speech -> auto-stop
                    req.stop.store(true, Ordering::SeqCst);
                }
            } else {
                no_speech_ms += 40;
                if no_speech_ms >= 10000 {
                    // 10s with no speech at all -> auto-stop
                    req.stop.store(true, Ordering::SeqCst);
                }
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

fn convert_mix(raw: &[u8], format: &WAVEFORMATEX) -> Result<Vec<i16>, String> {
    let channels = format.nChannels as usize;
    let rate = format.nSamplesPerSec as usize;
    let bits = format.wBitsPerSample as usize;
    let is_float = format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16 || bits == 32;
    let sample_bytes = bits / 8;
    if sample_bytes == 0 || channels == 0 {
        return Err("invalid mix format".into());
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
