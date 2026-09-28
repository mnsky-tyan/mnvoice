// Real-time word-by-word streaming transcription for Deepgram.
// Directly types confirmed tokens into the active cursor in real-time as speech happens.
// Monotonic forward-only word streaming with zero backspaces.
//
// The transport is the platform seam, not WinHTTP directly: this module carries
// the timing-sensitive part of the product, and keeping it free of platform
// calls is what lets the Linux and macOS ports reuse it unchanged. The
// full-duplex guarantee the loop depends on is documented on the trait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::platform::input;
use crate::platform::http::{NativeTransport, Transport, WebSocket};

pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

pub struct StreamResult {
    pub transcript: String,
    pub is_final: bool,
    pub speech_final: bool,
}

/// Dependency-free frame parser. A real JSON pull would drag in a crate for
/// four fields, and the provider's payload shape is stable; the contains()
/// checks are also how frames that are not transcript results at all (metadata,
/// errors, pings) get rejected cheaply before any parsing work happens.
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
/// Types confirmed words into the active cursor as they arrive; returns the full transcript.
pub fn run_stream(
    cfg: &Config,
    stop: &Arc<AtomicBool>,
    cancelled: &Arc<AtomicBool>,
    rx: Receiver<Vec<i16>>,
) -> Result<String, String> {
    let (host, port, secure, base_path) = crate::rest::parse_base_url(&cfg.base_url)
        .unwrap_or_else(|_| ("api.deepgram.com".to_string(), 443, true, String::new()));

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

    let scheme = if secure { "wss" } else { "ws" };
    let url = format!("{scheme}://{host}:{port}{path}");

    // Deepgram wants Token, OpenAI-compatible endpoints want Bearer; whatever
    // the user already typed is passed through so either spelling works.
    let auth_value = if cfg.api_key.starts_with("Token ") || cfg.api_key.starts_with("Bearer ") {
        cfg.api_key.clone()
    } else {
        format!("Token {}", cfg.api_key)
    };
    let auth_header = ("Authorization", auth_value.as_str());

    let socket = NativeTransport.websocket(&url, &[auth_header])?;
    let ws: Arc<dyn WebSocket> = Arc::from(socket);

    let full_transcript = Arc::new(Mutex::new(String::new()));
    let reader_done = Arc::new(AtomicBool::new(false));

    // Capture this before the thread moves in, so the borrow cannot escape.
    let strip_fillers = cfg.strip_fillers;

    let ws_reader = Arc::clone(&ws);
    let full_transcript_clone = full_transcript.clone();
    let stop_clone = stop.clone();
    let cancelled_clone = cancelled.clone();
    let reader_done_clone = reader_done.clone();

    let reader_thread = thread::spawn(move || {
        let mut typed_word_count = 0usize;
        let mut has_typed_any = false;
        let mut latest_uncommitted = String::new();

        while !reader_done_clone.load(Ordering::SeqCst) {
            // Cancel is honoured at the word boundary: stop reading and stop
            // typing immediately rather than waiting for speech to end.
            if cancelled_clone.load(Ordering::SeqCst) {
                break;
            }
            // On Windows the read blocks until a frame or close arrives, which
            // is exactly the pre-seam behaviour; the timeout only matters on
            // backends that can poll, where it keeps cancel responsive.
            let frame = match ws_reader.read(1000) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break,
                Err(_) => break,
            };
            if frame.is_empty() {
                break;
            }
            let msg = String::from_utf8_lossy(&frame);
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
                            let _ = input::type_text(&to_type);
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
                            let _ = input::type_text(&to_type);
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
                let _ = input::type_text(&to_type);
                let mut full = full_transcript_clone.lock().unwrap();
                if !full.is_empty() {
                    full.push(' ');
                }
                full.push_str(&remaining.join(" "));
            }
        }
        // The reader is out, so nothing more can arrive and the main thread's
        // wait can end with it rather than running out its clock.
        reader_done_clone.store(true, Ordering::SeqCst);
    });

    // Drain audio chunks from channel and stream to Deepgram
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(packet_i16) => {
                let slice_u8 = unsafe {
                    std::slice::from_raw_parts(
                        packet_i16.as_ptr() as *const u8,
                        packet_i16.len() * 2,
                    )
                };
                if ws.send_binary(slice_u8).is_err() {
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
        let slice_u8 = unsafe {
            std::slice::from_raw_parts(packet_i16.as_ptr() as *const u8, packet_i16.len() * 2)
        };
        let _ = ws.send_binary(slice_u8);
    }

    // Signal close to Deepgram
    let _ = ws.send_text("{\"type\": \"CloseStream\"}");

    // Wait up to 1500ms for Deepgram to return the final transcription
    let wait_deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < wait_deadline && !reader_done.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(20));
    }

    reader_done.store(true, Ordering::SeqCst);
    ws.close();

    let _ = reader_thread.join();

    let full_text = full_transcript.lock().unwrap().trim().to_string();

    // Add trailing space if configured, but never on a cancelled session
    if cfg.trailing_space && !full_text.is_empty() && !cancelled.load(Ordering::SeqCst) {
        let _ = input::type_text(" ");
    }

    Ok(full_text)
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
