// Real-time streaming transcription for Deepgram over native WinHTTP WebSockets.
// Directly types confirmed text input into the focused window via Win32 SendInput (KEYEVENTF_UNICODE).
// Auto-stops smoothly when 2.0s of silence is detected.
// Receives pre-buffered audio chunks from the audio thread via an mpsc Receiver to guarantee zero truncation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Networking::WinHttp::*;
use windows::core::{w, PCWSTR};

use crate::config::Config;
use crate::paste;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct StreamResult {
    pub transcript: String,
    pub is_final: bool,
    pub speech_final: bool,
}

pub fn parse_stream_json(json: &str) -> Option<StreamResult> {
    if !json.contains("\"Results\"") {
        return None;
    }
    let transcript = crate::groq::parse_deepgram_transcript(json).unwrap_or_default();
    let speech_final = json.contains("\"speech_final\":true") || json.contains("\"speech_final\": true");
    let is_final = json.contains("\"is_final\":true") || json.contains("\"is_final\": true");
    Some(StreamResult {
        transcript,
        is_final,
        speech_final,
    })
}

/// Connects to Deepgram WebSocket and streams audio chunks from `rx`.
/// Any audio captured before the connection completed is immediately drained from `rx` and sent.
pub fn run_stream(
    cfg: &Config,
    stop: &Arc<AtomicBool>,
    rx: Receiver<Vec<i16>>,
) -> Result<String, String> {
    unsafe {
        let session = WinHttpOpen(
            w!("mnvoice/0.1"),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            return Err("cannot create HTTP session".into());
        }

        let host_w = wide("api.deepgram.com");
        let connect = WinHttpConnect(session, PCWSTR(host_w.as_ptr()), 443, 0);
        if connect.is_null() {
            let _ = WinHttpCloseHandle(session);
            return Err("cannot connect to api.deepgram.com".into());
        }

        // endpointing=1500 allows natural relaxed pauses without cutting off
        let mut path = format!(
            "/v1/listen?model={}&smart_format=true&encoding=linear16&sample_rate=16000&channels=1&interim_results=true&endpointing=1500",
            cfg.model
        );
        if !cfg.language.is_empty() {
            if cfg.language.eq_ignore_ascii_case("auto") {
                path.push_str("&detect_language=true");
            } else {
                path.push_str("&language=");
                path.push_str(&cfg.language);
            }
        }
        let path_w = wide(&path);

        let request = WinHttpOpenRequest(
            connect,
            w!("GET"),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        );
        if request.is_null() {
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err("cannot open request".into());
        }

        let opt_ok = WinHttpSetOption(Some(request), WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET, None);
        if opt_ok.is_err() {
            let _ = WinHttpCloseHandle(request);
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err("upgrade to websocket failed".into());
        }

        let headers = format!("Authorization: Token {}\r\n", cfg.api_key);
        let headers_w = wide(&headers);
        let _ = WinHttpAddRequestHeaders(request, &headers_w[..headers_w.len() - 1], 0x2000_0000);

        WinHttpSendRequest(request, None, None, 0, 0, 0)
            .map_err(|e| format!("SendRequest error ({e})"))?;
        WinHttpReceiveResponse(request, std::ptr::null_mut())
            .map_err(|e| format!("ReceiveResponse error ({e})"))?;

        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let mut idx = 0u32;
        let _ = WinHttpQueryHeaders(
            request,
            19 | 0x2000_0000,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut std::ffi::c_void),
            &mut len,
            &mut idx,
        );
        if status != 101 {
            let _ = WinHttpCloseHandle(request);
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err(format!("WebSocket handshake rejected with HTTP {status}"));
        }

        let ws = WinHttpWebSocketCompleteUpgrade(request, 0);
        let _ = WinHttpCloseHandle(request);
        if ws.is_null() {
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err("CompleteUpgrade failed".into());
        }

        let final_text = Arc::new(Mutex::new(String::new()));
        let typed_count = Arc::new(Mutex::new(0usize));
        let reader_done = Arc::new(AtomicBool::new(false));

        // Reader thread: directly types confirmed tokens into the active window
        let ws_reader = ws as usize;
        let final_text_clone = final_text.clone();
        let typed_clone = typed_count.clone();
        let stop_clone = stop.clone();
        let reader_done_clone = reader_done.clone();

        let reader_thread = thread::spawn(move || {
            let ws = ws_reader as *const std::ffi::c_void;
            let mut buf = vec![0u8; 16384];

            while !reader_done_clone.load(Ordering::SeqCst) {
                let mut bytes_read = 0u32;
                let mut buf_type = WINHTTP_WEB_SOCKET_BUFFER_TYPE::default();
                let res = WinHttpWebSocketReceive(
                    ws,
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    buf.len() as u32,
                    &mut bytes_read,
                    &mut buf_type,
                );
                if res != 0 || bytes_read == 0 {
                    break;
                }

                let msg = String::from_utf8_lossy(&buf[..bytes_read as usize]);
                if let Some(res) = parse_stream_json(&msg) {
                    if !res.transcript.is_empty() {
                        if res.is_final {
                            let mut full = final_text_clone.lock().unwrap();
                            let mut chunk_to_type = res.transcript.clone();

                            if !full.is_empty() {
                                full.push(' ');
                                chunk_to_type = format!(" {chunk_to_type}");
                            } else {
                                *full = chunk_to_type.clone();
                            }

                            // Directly type confirmed text into active window
                            let _ = paste::type_text(&chunk_to_type);
                            *typed_clone.lock().unwrap() += chunk_to_type.len();
                        }

                        if res.speech_final {
                            stop_clone.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
        });

        // Drain pre-buffered audio from channel and continue streaming live
        while let Ok(packet_i16) = rx.recv_timeout(Duration::from_millis(50)) {
            let slice_u8 = std::slice::from_raw_parts(
                packet_i16.as_ptr() as *const u8,
                packet_i16.len() * 2,
            );
            let send_res = WinHttpWebSocketSend(
                ws,
                WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE,
                Some(slice_u8),
            );
            if send_res != 0 || stop.load(Ordering::SeqCst) {
                break;
            }
        }

        // Close stream cleanly
        let close_msg = b"{\"type\": \"CloseStream\"}";
        let _ = WinHttpWebSocketSend(ws, WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE, Some(close_msg));

        // Wait up to 350ms for final response
        let wait_deadline = Instant::now() + Duration::from_millis(350);
        while Instant::now() < wait_deadline && !reader_done.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
        }

        reader_done.store(true, Ordering::SeqCst);
        let _ = WinHttpWebSocketClose(ws, WINHTTP_WEB_SOCKET_SUCCESS_CLOSE_STATUS.0 as u16, None, 0);
        let _ = WinHttpCloseHandle(ws);
        let _ = WinHttpCloseHandle(connect);
        let _ = WinHttpCloseHandle(session);

        let _ = reader_thread.join();

        let full_text = final_text.lock().unwrap().trim().to_string();
        let typed = *typed_count.lock().unwrap();

        // If any text was finalized after CloseStream, type it
        if full_text.len() > typed {
            let remaining = &full_text[typed..];
            let _ = paste::type_text(remaining);
        }

        // Add trailing space if configured
        if cfg.trailing_space && !full_text.is_empty() {
            let _ = paste::type_text(" ");
        }

        Ok(full_text)
    }
}
