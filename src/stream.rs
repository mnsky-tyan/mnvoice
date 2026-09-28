// Real-time word-by-word streaming transcription for Deepgram.
// Directly types confirmed tokens into the active cursor in real-time as speech happens.
// Monotonic forward-only word streaming with zero backspaces.
//
// The transport is the platform seam, not WinHTTP directly: this module carries
// the timing-sensitive part of the product, and keeping it free of platform
// calls is what lets the Linux and macOS ports reuse it unchanged. The
// full-duplex guarantee the loop depends on is documented on the trait.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// How long the provider must stay quiet after its last frame before the
/// streaming loop stops waiting for a final result.
const FINAL_QUIET: Duration = Duration::from_millis(1500);

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
    // When the provider last sent a frame, in milliseconds since the stream
    // opened.
    let started = Instant::now();
    let last_frame_ms = Arc::new(AtomicU64::new(0));

    // Capture this before the thread moves in, so the borrow cannot escape.
    let strip_fillers = cfg.strip_fillers;

    let ws_reader = Arc::clone(&ws);
    let full_transcript_clone = full_transcript.clone();
    let stop_clone = stop.clone();
    let cancelled_clone = cancelled.clone();
    let reader_done_clone = reader_done.clone();
    let last_frame_clone = Arc::clone(&last_frame_ms);

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
            last_frame_clone.store(started.elapsed().as_millis() as u64, Ordering::SeqCst);

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

    let closed_at = Instant::now();
    wait_for_final_result(
        started,
        closed_at,
        &last_frame_ms,
        &reader_done,
        cfg.max_seconds,
    );

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

/// How long the streaming loop waits for the provider's final result once
/// CloseStream has been sent.
///
/// The window never runs shorter than it did before the port: the deadline is
/// the later of CloseStream plus the quiet period and the provider's own last
/// frame plus that same period. On Windows the audio had been flowing in real
/// time, so interim results were already being typed and the provider's last
/// frame landed about when CloseStream was sent, which makes the two the same
/// instant. The Unix CLI hands the provider a whole clip in one burst, so its
/// transcription of that clip is still running after CloseStream and each frame
/// it sends pushes the deadline out again - a window measured from the send
/// alone would cut the final clause off. The wait also ends the moment the
/// reader thread goes idle, and `cap_seconds` bounds a peer that never stops
/// sending - floored at the quiet period itself, so a small `MAX_SECONDS`
/// cannot shorten the window below the one it replaces.
fn wait_for_final_result(
    started: Instant,
    closed_at: Instant,
    last_frame_ms: &AtomicU64,
    reader_done: &AtomicBool,
    cap_seconds: u32,
) {
    let final_deadline = closed_at + Duration::from_secs(cap_seconds as u64).max(FINAL_QUIET);
    while !reader_done.load(Ordering::SeqCst) {
        let quiet_after_last_frame =
            started + Duration::from_millis(last_frame_ms.load(Ordering::SeqCst)) + FINAL_QUIET;
        let deadline = (closed_at + FINAL_QUIET).max(quiet_after_last_frame);
        let now = Instant::now();
        if now >= deadline || now >= final_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
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

    /// A provider that had already gone quiet before CloseStream still gets the
    /// full window the port inherited, because the deadline is the later of the
    /// send plus the quiet period and the provider's last frame plus the same
    /// period. Anchoring to the last frame alone made the wait zero here, and
    /// the provider's final `is_final` frame - the one carrying the last
    /// clause - was never read, so the transcript came back truncated.
    #[test]
    fn the_final_wait_is_never_shorter_than_the_window_it_replaces() {
        let started = Instant::now() - Duration::from_secs(10);
        let closed_at = Instant::now();
        let last_frame = AtomicU64::new(8_000); // two seconds before the close
        let reader_done = AtomicBool::new(false);

        let began = Instant::now();
        wait_for_final_result(started, closed_at, &last_frame, &reader_done, 120);
        let waited = began.elapsed();

        assert!(
            waited >= Duration::from_millis(1_400),
            "the window ran short at {waited:?}"
        );
        assert!(waited < Duration::from_millis(2_500), "waited {waited:?}");
    }

    /// A clip handed over in one burst is still being transcribed after
    /// CloseStream, so the final frame lands late. The window has to outlast
    /// that frame by the quiet period instead of expiring at a fixed offset
    /// from the send, which is what cut the last clause off.
    #[test]
    fn the_final_wait_outlasts_a_final_frame_that_arrives_late() {
        let started = Instant::now();
        let closed_at = Instant::now();
        let last_frame = Arc::new(AtomicU64::new(0));
        let reader_done = Arc::new(AtomicBool::new(false));
        let late = Arc::clone(&last_frame);

        let provider = thread::spawn(move || {
            thread::sleep(Duration::from_millis(400));
            late.store(400, Ordering::SeqCst);
        });

        let began = Instant::now();
        wait_for_final_result(started, closed_at, &last_frame, &reader_done, 120);
        let waited = began.elapsed();
        provider.join().unwrap();

        assert!(
            waited >= Duration::from_millis(1_800),
            "the last clause was cut off after only {waited:?}"
        );
        assert!(waited < Duration::from_millis(4_000), "waited {waited:?}");
    }

    /// A reader that finished on its own - the provider closed the connection -
    /// has nothing left to wait for.
    #[test]
    fn the_final_wait_ends_when_the_reader_has_finished() {
        let started = Instant::now();
        let closed_at = Instant::now();
        let last_frame = AtomicU64::new(0);
        let reader_done = AtomicBool::new(true);

        let began = Instant::now();
        wait_for_final_result(started, closed_at, &last_frame, &reader_done, 120);

        assert!(began.elapsed() < Duration::from_millis(100));
    }

    /// The cap is what stops a peer that keeps sending from holding the session
    /// open: without it the quiet window would keep moving and never expire.
    /// The cap never goes below the window it replaces either, so a
    /// `MAX_SECONDS` smaller than the quiet period cannot cut the final result
    /// off the way an unfloored cap did.
    #[test]
    fn the_final_wait_is_capped_so_a_chatty_peer_cannot_hold_it_open() {
        let started = Instant::now();
        let closed_at = Instant::now();
        let last_frame = Arc::new(AtomicU64::new(0));
        let reader_done = Arc::new(AtomicBool::new(false));
        let chatty = Arc::clone(&last_frame);

        let provider = thread::spawn(move || {
            // Stamps the real elapsed time on every tick, so the quiet window is
            // always a full period away and never elapses by itself.
            for _ in 0..40 {
                thread::sleep(Duration::from_millis(50));
                chatty.store(started.elapsed().as_millis() as u64, Ordering::SeqCst);
            }
        });

        let began = Instant::now();
        wait_for_final_result(started, closed_at, &last_frame, &reader_done, 1);
        let waited = began.elapsed();
        provider.join().unwrap();

        assert!(
            waited >= Duration::from_millis(1_400),
            "a cap below the quiet window shortened it: {waited:?}"
        );
        assert!(
            waited < Duration::from_millis(1_900),
            "a peer that never stops sending was waited on past the cap: {waited:?}"
        );
    }
}
