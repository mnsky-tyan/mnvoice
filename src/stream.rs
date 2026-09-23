// Real-time word-by-word streaming transcription for Deepgram over native WinHTTP WebSockets.
// Directly types confirmed tokens into the active cursor in real-time as speech happens.
// Monotonic forward-only word streaming with zero backspaces.

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

pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

pub struct StreamResult {
    pub transcript: String,
    pub is_final: bool,
    pub speech_final: bool,
}

pub fn parse_stream_json(json: &str) -> Option<StreamResult> {
    if !json.contains("\"Results\"") && !json.contains("\"results\"") && !json.contains("\"text\"") {
        return None;
    }
    let transcript = crate::rest::parse_json_transcript(json).unwrap_or_default();
    let speech_final = json.contains("\"speech_final\":true") || json.contains("\"speech_final\": true");
    let is_final = json.contains("\"is_final\":true") || json.contains("\"is_final\": true");
    Some(StreamResult {
        transcript,
        is_final,
        speech_final,
    })
}

/// Connects to a real-time streaming WebSocket endpoint and streams audio chunks from `rx`.
/// Directly types words into the active window in real-time as they are spoken.
pub fn run_stream(
    cfg: &Config,
    stop: &Arc<AtomicBool>,
    cancelled: &Arc<AtomicBool>,
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

        let (host, port, secure, base_path) = crate::rest::parse_base_url(&cfg.base_url)
            .unwrap_or_else(|_| ("api.deepgram.com".to_string(), 443, true, String::new()));
        let host_w = wide(&host);
        let connect = WinHttpConnect(session, PCWSTR(host_w.as_ptr()), port, 0);
        if connect.is_null() {
            let _ = WinHttpCloseHandle(session);
            return Err(format!("cannot connect to {host}"));
        }

        let prefix = if !base_path.is_empty() {
            base_path
        } else {
            "/v1/listen".to_string()
        };

        // no_delay=true: release words immediately without buffering for more context (Nova-3)
        // vad_events=true: receive SpeechStarted/UtteranceEnd events from Deepgram's own VAD
        // filler_words: strip disfluencies when the provider supports it natively.
        // Providers that ignore it are still covered by the local filter below.
        let filler_words = if cfg.strip_fillers { "false" } else { "true" };
        let mut path = format!(
            "{prefix}?model={}&smart_format=true&encoding=linear16&sample_rate=16000&channels=1&interim_results=true&endpointing=1500&no_delay=true&vad_events=true&filler_words={filler_words}",
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
        for kw in &cfg.keywords {
            let trimmed = kw.trim();
            if !trimmed.is_empty() {
                let enc = url_encode(trimmed);
                if cfg.model.contains("nova-3") {
                    path.push_str("&keyterm=");
                } else {
                    path.push_str("&keywords=");
                }
                path.push_str(&enc);
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
            if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) },
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

        let auth_prefix = if cfg.api_key.starts_with("Token ") || cfg.api_key.starts_with("Bearer ") {
            ""
        } else {
            "Token "
        };
        let headers = format!("Authorization: {auth_prefix}{}\r\n", cfg.api_key);
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

        let full_transcript = Arc::new(Mutex::new(String::new()));
        let reader_done = Arc::new(AtomicBool::new(false));

        // Capture this before the thread moves in, so the borrow cannot escape.
        let strip_fillers = cfg.strip_fillers;

        // Reader thread: streams words token-by-token directly into active cursor
        let ws_reader = ws as usize;
        let full_transcript_clone = full_transcript.clone();
        let stop_clone = stop.clone();
        let cancelled_clone = cancelled.clone();
        let reader_done_clone = reader_done.clone();

        let reader_thread = thread::spawn(move || {
            let ws = ws_reader as *const std::ffi::c_void;
            let mut buf = vec![0u8; 16384];
            let mut typed_word_count = 0usize;
            let mut has_typed_any = false;
            let mut latest_uncommitted = String::new();

            while !reader_done_clone.load(Ordering::SeqCst) {
                // Cancel is honoured at the word boundary: stop reading and stop
                // typing immediately rather than waiting for speech to end.
                if cancelled_clone.load(Ordering::SeqCst) {
                    break;
                }
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
                    let trimmed = res.transcript.trim();
                    if !trimmed.is_empty() {
                        // Filter fillers before this frame is typed, so the word
                        // indices below stay aligned frame to frame.
                        let words: Vec<&str> = if strip_fillers {
                            trimmed
                                .split_whitespace()
                                .filter(|w| !crate::rest::is_disfluency(w))
                                .collect()
                        } else {
                            trimmed.split_whitespace().collect()
                        };

                        // Keep the filtered text for the end-of-stream flush, so
                        // the word indices used there match what was typed.
                        latest_uncommitted = words.join(" ");

                        if res.is_final {
                            // Sentence/clause finalized: type all remaining words to the end
                            if words.len() > typed_word_count {
                                let remaining = &words[typed_word_count..];
                                let mut to_type = remaining.join(" ");
                                if has_typed_any {
                                    to_type = format!(" {to_type}");
                                }
                                let _ = paste::type_text(&to_type);
                                has_typed_any = true;

                                let mut full = full_transcript_clone.lock().unwrap();
                                if !full.is_empty() {
                                    full.push(' ');
                                }
                                full.push_str(&remaining.join(" "));
                            }
                            typed_word_count = 0; // reset for next clause
                            latest_uncommitted.clear();
                        } else {
                            // Interim results: type completed words (all except the trailing partial word)
                            if words.len() > 1 && words.len() - 1 > typed_word_count {
                                let completed = &words[typed_word_count..words.len() - 1];
                                let mut to_type = completed.join(" ");
                                if has_typed_any {
                                    to_type = format!(" {to_type}");
                                }
                                let _ = paste::type_text(&to_type);
                                has_typed_any = true;

                                let mut full = full_transcript_clone.lock().unwrap();
                                if !full.is_empty() {
                                    full.push(' ');
                                }
                                full.push_str(&completed.join(" "));

                                typed_word_count = words.len() - 1;
                            }
                        }

                        if res.speech_final {
                            stop_clone.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }

            // Flush any remaining words from the latest interim transcript upon stop/close.
            // Skipped entirely on cancel so a discarded session leaves nothing behind.
            if !latest_uncommitted.is_empty() && !cancelled_clone.load(Ordering::SeqCst) {
                let words: Vec<&str> = latest_uncommitted.split_whitespace().collect();
                if words.len() > typed_word_count {
                    let remaining = &words[typed_word_count..];
                    let mut to_type = remaining.join(" ");
                    if has_typed_any {
                        to_type = format!(" {to_type}");
                    }
                    let _ = paste::type_text(&to_type);
                    let mut full = full_transcript_clone.lock().unwrap();
                    if !full.is_empty() {
                        full.push(' ');
                    }
                    full.push_str(&remaining.join(" "));
                }
            }
        });

        // Drain audio chunks from channel and stream to Deepgram
        while !stop.load(Ordering::SeqCst) {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(packet_i16) => {
                    let slice_u8 = std::slice::from_raw_parts(
                        packet_i16.as_ptr() as *const u8,
                        packet_i16.len() * 2,
                    );
                    let send_res = WinHttpWebSocketSend(
                        ws,
                        WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE,
                        Some(slice_u8),
                    );
                    if send_res != 0 {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }

        // Drain all remaining audio packets accumulated in rx before closing
        while let Ok(packet_i16) = rx.try_recv() {
            let slice_u8 = std::slice::from_raw_parts(
                packet_i16.as_ptr() as *const u8,
                packet_i16.len() * 2,
            );
            let _ = WinHttpWebSocketSend(
                ws,
                WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE,
                Some(slice_u8),
            );
        }

        // Signal close to Deepgram
        let close_msg = b"{\"type\": \"CloseStream\"}";
        let _ = WinHttpWebSocketSend(ws, WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE, Some(close_msg));

        // Wait up to 1500ms for Deepgram to return the final transcription
        let wait_deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < wait_deadline && !reader_done.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
        }

        reader_done.store(true, Ordering::SeqCst);
        let _ = WinHttpWebSocketClose(ws, WINHTTP_WEB_SOCKET_SUCCESS_CLOSE_STATUS.0 as u16, None, 0);
        let _ = WinHttpCloseHandle(ws);
        let _ = WinHttpCloseHandle(connect);
        let _ = WinHttpCloseHandle(session);

        let _ = reader_thread.join();

        let full_text = full_transcript.lock().unwrap().trim().to_string();

        // Add trailing space if configured, but never on a cancelled session
        if cfg.trailing_space && !full_text.is_empty() && !cancelled.load(Ordering::SeqCst) {
            let _ = paste::type_text(" ");
        }

        Ok(full_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("C++"), "C%2B%2B");
        assert_eq!(url_encode("mnvoice"), "mnvoice");
    }
}
