// Real-time word-by-word streaming transcription for Deepgram.
// Directly types confirmed tokens into the active cursor in real-time as speech happens.
// Monotonic forward-only word streaming with zero backspaces.
//
// The transport is the platform seam, not WinHTTP directly: this module carries
// the timing-sensitive part of the product, and keeping it free of platform
// calls is what lets the Linux and macOS ports reuse it unchanged. The
// full-duplex guarantee the loop depends on is documented on the trait.

use crate::locks::lock_poisoned;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::platform::audio::SAMPLE_RATE;
use crate::platform::http::{NativeTransport, Transport, WebSocket};
use crate::platform::input;

/// Little-endian PCM bytes for a chunk of i16 samples, the wire format the
/// streaming endpoint expects (see `platform::audio::wav_bytes` for the same
/// encoding in a WAV container).
fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn url_encode(s: &str) -> String {
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

struct StreamResult {
    transcript: String,
    is_final: bool,
    speech_final: bool,
}

/// Dependency-free frame parser. A real JSON pull would drag in a crate for
/// four fields, and the provider's payload shape is stable; the contains()
/// checks are also how frames that are not transcript results at all (metadata,
/// errors, pings) get rejected cheaply before any parsing work happens.
fn parse_stream_json(json: &str) -> Option<StreamResult> {
    if !json.contains("\"Results\"") && !json.contains("\"results\"") && !json.contains("\"text\"")
    {
        return None;
    }
    let transcript = crate::rest::parse_json_transcript(json).unwrap_or_default();
    let speech_final =
        json.contains("\"speech_final\":true") || json.contains("\"speech_final\": true");
    let is_final = json.contains("\"is_final\":true") || json.contains("\"is_final\": true");
    Some(StreamResult {
        transcript,
        is_final,
        speech_final,
    })
}

/// The provider's listen endpoint for this config: base URL from the config
/// (or the Deepgram default), the model and format parameters, language and
/// keywords. Pure, so the wire contract is testable without a socket.
///
/// An unparseable `BASE_URL` is an error, exactly as it is on the REST path
/// (`rest::endpoint_url`): the config is only defaulted when it is EMPTY, so a
/// present-but-broken value must fail closed rather than silently point the
/// session - and the user's API key - at Deepgram.
fn listen_url(cfg: &Config) -> Result<String, String> {
    let (host, port, secure, base_path) = crate::rest::parse_base_url(&cfg.base_url)?;

    let prefix = if !base_path.is_empty() {
        base_path
    } else {
        "/v1/listen".to_string()
    };

    // no_delay=true: release words immediately without buffering for more context (Nova-3)
    //
    // vad_events=true keeps the connection's parameters intact, which is its
    // whole reason to stay. Deepgram's VAD events are not read here: the reader
    // below has no branch for SpeechStarted or UtteranceEnd, and
    // `parse_stream_json` returns None for those frames, so they are dropped on
    // arrival. The flag is sent anyway because the query is built by appending
    // pairs to one string, and a `#` in MODEL once truncated everything after
    // it - silently disabling this alongside smart_format and interim_results.
    // Sending the pair unconditionally keeps that from happening again.
    //
    // filler_words: strip disfluencies when the provider supports it natively.
    // Providers that ignore it are still covered by the local filter below.
    let filler_words = if cfg.strip_fillers { "false" } else { "true" };
    let mut path = format!(
        "{prefix}?model={}&smart_format=true&encoding=linear16&sample_rate={SAMPLE_RATE}&channels=1&interim_results=true&endpointing=1500&no_delay=true&vad_events=true&filler_words={filler_words}",
        url_encode(&cfg.model)
    );
    if !cfg.language.is_empty() {
        if cfg.language.eq_ignore_ascii_case("auto") {
            path.push_str("&detect_language=true");
        } else {
            // Encoded for the same reason the keyword loop below is: the query
            // is the contract, and a value carrying &, = or # does not just
            // fail - it silently rewrites the request. `MODEL=x#frag` ended the
            // query, discarding smart_format, interim_results, endpointing,
            // no_delay and vad_events, so the app quietly stopped streaming
            // words with nothing anywhere reporting a problem.
            path.push_str("&language=");
            path.push_str(&url_encode(&cfg.language));
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
    // `rest::url_authority` owns the bracketing rule (parse_base_url hands back
    // an unbracketed IPv6 literal because WinHttpConnect wants it that way, and
    // a URL authority must bracket it again). This used to re-derive the
    // decision from `host.contains(':')` at this call site - correct, but a
    // second copy of a rule that can drift from the parser.
    let authority = crate::rest::url_authority(&host);
    Ok(format!("{scheme}://{authority}:{port}{path}"))
}

/// Types the words in `words[from..to]` at the cursor and appends them to the
/// running transcript, space-separated from whatever is already there. This is
/// the one place a decided word becomes typed text; the reader's branches all
/// end here so their only differences can be the range they commit.
fn commit_words(
    words: &[&str],
    from: usize,
    to: usize,
    has_typed_any: &mut bool,
    full: &Mutex<String>,
) {
    // Clamped, so a caller whose counter has run past this frame's words (a
    // shorter final transcript after a longer interim one) is a no-op rather
    // than a slice panic.
    let to = to.min(words.len());
    if from >= to {
        return;
    }
    let joined = words[from..to].join(" ");
    let mut to_type = String::new();
    if *has_typed_any {
        to_type.push(' ');
    }
    to_type.push_str(&joined);
    input::type_text(&to_type);
    *has_typed_any = true;

    let mut full = lock_poisoned(full);
    if !full.is_empty() {
        full.push(' ');
    }
    full.push_str(&joined);
}

/// Connects to a real-time streaming WebSocket endpoint and streams audio chunks from `rx`.
/// Types confirmed words into the active cursor as they arrive; returns the full transcript.
pub fn run_stream(
    cfg: &Config,
    stop: &Arc<AtomicBool>,
    cancelled: &Arc<AtomicBool>,
    rx: Receiver<Vec<i16>>,
) -> Result<String, String> {
    let url = listen_url(cfg)?;
    // Deepgram wants the `Token` scheme; `config::auth_value` owns the rule
    // that a key already carrying a scheme is passed through unchanged.
    let auth_value = crate::config::auth_value(&cfg.api_key, "Token");
    let auth_header = ("Authorization", auth_value.as_str());

    let socket = NativeTransport.websocket(&url, &[auth_header])?;
    let ws: Arc<dyn WebSocket> = Arc::from(socket);

    let full_transcript = Arc::new(Mutex::new(String::new()));
    let reader_done = Arc::new(AtomicBool::new(false));
    // A transport failure mid-dictation is not a clean close, and the seam
    // types `read` as a Result precisely so the two can be told apart. The
    // message is kept here so the caller can report the real cause instead of
    // telling the user they said nothing.
    let read_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // The same failure seen from the sending side. `send_binary` returns a
    // Result too, and discarding it meant the commonest mid-dictation death -
    // the provider drops the socket while audio is still flowing - produced a
    // bare `Ok(full_text)`: the send loop broke, the drain and CloseStream and
    // the 1500 ms wait all failed silently, and the reader got `Ok(None)` (a
    // clean close) rather than `Err`, so nothing recorded the fault anywhere.
    // A half-sentence was typed with no marker, which is the exact outcome the
    // partial-stream contract exists to prevent.
    //
    // Kept separate from `read_error` rather than sharing the slot, because the
    // reader arm's suppression rule (`!reader_done && !stop`) is about
    // self-inflicted teardown and does not apply here: a send failure is never
    // self-inflicted, but it is still expected once the session is ending on
    // purpose, so this side checks `stop`/`cancelled` and nothing else.
    let send_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Capture this before the thread moves in, so the borrow cannot escape.
    let strip_fillers = cfg.strip_fillers;

    let ws_reader = Arc::clone(&ws);
    let full_transcript_clone = full_transcript.clone();
    let stop_clone = stop.clone();
    let cancelled_clone = cancelled.clone();
    let reader_done_clone = reader_done.clone();
    let read_error_clone = read_error.clone();

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
            // A read blocks until the next frame, a close, or an error, so
            // cancel is checked before every call and honoured at the word
            // boundary; a reader parked inside `read` is released by the
            // `close()` this function issues once the session ends.
            let frame = match ws_reader.read() {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break,
                Err(e) => {
                    // Keep it only while the session is still running: the
                    // caller terminates the socket itself on the way out
                    // (`close()` shuts the connection down, which is what
                    // releases this parked read), so an error seen after
                    // `reader_done` was set is self-inflicted and would mark
                    // every ordinary session as lost. Likewise once `stop` is
                    // set the session is ending on purpose, so a provider that
                    // tears down without a close frame is the expected end of
                    // a finished dictation. A fault that arrives while the
                    // caller still expects frames - including one that breaks
                    // the send loop first - is the case the marker is for.
                    //
                    // `cancelled` is deliberately NOT a third condition here,
                    // though the send-side predicate names it. The direction
                    // that actually holds is "`cancelled` implies `stop`
                    // (eventually)", not the contrapositive: both setters
                    // (`watch_recording` and `windows_app::cancel`) store
                    // `cancelled` first and `stop` second, so there is a
                    // narrow window where `cancelled` is true and `stop` is
                    // still false - a read error arriving there is recorded,
                    // not suppressed. A cancelled session's result is
                    // discarded before typing, so the window costs nothing;
                    // that is why the predicate omits `cancelled`.
                    if !reader_done_clone.load(Ordering::SeqCst)
                        && !stop_clone.load(Ordering::SeqCst)
                    {
                        let mut slot = lock_poisoned(&read_error_clone);
                        *slot = Some(e);
                    }
                    break;
                }
            };
            // An empty message is a frame with nothing in it, not a hangup.
            // Both backends report a real close as `Ok(None)` above (WinHTTP
            // via the close buffer type, tungstenite via its own close
            // handling), so reaching here with zero bytes means the provider
            // sent a legal empty data message - which the guard used to treat
            // as the end of the stream, exiting the reader without a fault
            // marker and handing the caller a truncated transcript presented
            // as complete. The break predates the transport seam, when an
            // empty buffer was the only "nothing" signal `read` had.
            if frame.is_empty() {
                continue;
            }
            let msg = String::from_utf8_lossy(&frame);
            if let Some(res) = parse_stream_json(&msg) {
                let trimmed = res.transcript.trim();
                // `speech_final` is a control flag, not data: it says the
                // provider has finished an utterance, and the frame that says
                // so can legitimately carry no words at all. Gating it on the
                // transcript being non-empty - which is where this used to sit,
                // inside the block below - meant an empty final frame never set
                // `stop`, so the session ran to MAX_SECONDS instead of ending
                // when speech did, and the missed flag left `typed_word_count`
                // pointing into the clause that had just ended, so the next
                // clause's interims typed or dropped the wrong words. A flag
                // that ends a session must not depend on what the frame
                // happens to contain.
                if res.speech_final {
                    stop_clone.store(true, Ordering::SeqCst);
                }
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
                        commit_words(
                            &words,
                            typed_word_count,
                            words.len(),
                            &mut has_typed_any,
                            &full_transcript_clone,
                        );
                    } else {
                        // Interim results: type completed words (all except the trailing partial word)
                        if words.len() > 1 && words.len() - 1 > typed_word_count {
                            commit_words(
                                &words,
                                typed_word_count,
                                words.len() - 1,
                                &mut has_typed_any,
                                &full_transcript_clone,
                            );
                            typed_word_count = words.len() - 1;
                        }
                    }
                }
                // The clause boundary itself is outside the data guard, for the
                // same reason `speech_final` is: a final frame that carries no
                // transcript still ends the clause. Left inside, an empty final
                // frame kept `typed_word_count` pointing into the clause that
                // had just closed, so the NEXT clause's interims compared
                // against a stale index - `words.len() - 1 > typed_word_count`
                // failed and its leading words were never typed, and its final
                // frame then committed from the wrong offset. Resetting both
                // halves together is what keeps them describing one clause:
                // `latest_uncommitted` is the current clause's text and
                // `typed_word_count` is how much of it has gone out, so moving
                // one without the other would have the end-of-stream flush
                // re-type words that were already sent.
                if res.is_final {
                    typed_word_count = 0; // reset for next clause
                    latest_uncommitted.clear();
                }
            }
        }

        // Flush any remaining words from the latest interim transcript upon stop/close.
        // Skipped entirely on cancel so a discarded session leaves nothing behind.
        if !latest_uncommitted.is_empty() && !cancelled_clone.load(Ordering::SeqCst) {
            let words: Vec<&str> = latest_uncommitted.split_whitespace().collect();
            commit_words(
                &words,
                typed_word_count,
                words.len(),
                &mut has_typed_any,
                &full_transcript_clone,
            );
        }
        // The reader is out, so nothing more can arrive and the main thread's
        // wait can end with it rather than running out its clock.
        reader_done_clone.store(true, Ordering::SeqCst);
    });

    // Drain audio chunks from channel and stream to Deepgram
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(packet_i16) => {
                if let Err(e) = ws.send_binary(&pcm_bytes(&packet_i16)) {
                    // Only a failure while the session is still live is a
                    // fault. Once `stop` or `cancelled` is set the caller is
                    // tearing the socket down on purpose, and a provider that
                    // has already gone away is the expected end of a finished
                    // dictation, not a lost connection.
                    if !stop.load(Ordering::SeqCst) && !cancelled.load(Ordering::SeqCst) {
                        let mut slot = lock_poisoned(&send_error);
                        if slot.is_none() {
                            *slot = Some(e.to_string());
                        }
                    }
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

    // Drain all remaining audio packets accumulated in rx before closing -
    // unless the session was cancelled: a cancel is a hard discard (the same
    // rule dictate_rest enforces before its upload), so the queued tail must
    // not be shipped to the provider after the user said stop.
    //
    // A send error ends the drain rather than skipping the packet. Once a send
    // fails the socket can carry no more data, so the packets behind it cannot
    // be delivered either - each continued send would stall exactly as long as
    // the one that failed, paid once per packet. The break removes that repeat
    // cost, and on Windows it removes only that: a Windows send to a stalled
    // peer blocks without returning `Err`, so the first drain send parks and
    // this `break` is never reached - the Windows hang needs a bound on the
    // send or a drain that gives up, neither of which this is. The intent is
    // unchanged: ship the queued tail when the peer is healthy, and stop the
    // moment it is not.
    if !cancelled.load(Ordering::SeqCst) {
        loop {
            let Ok(packet_i16) = rx.try_recv() else {
                #[cfg(test)]
                crate::test_hooks::record_drain_backlog_exhausted();
                break;
            };
            if ws.send_binary(&pcm_bytes(&packet_i16)).is_err() {
                #[cfg(test)]
                crate::test_hooks::record_drain_broke_early();
                break;
            }
        }
    }

    // Signal close to Deepgram
    let _ = ws.send_text("{\"type\": \"CloseStream\"}");

    // Wait up to 1500ms for Deepgram to return the final transcription
    //
    // `cancelled` breaks the wait as well as `reader_done`. The reader is the
    // thread that sets `reader_done` (`reader_done_clone.store` at the end of
    // the reader closure), so on a cancel - where the
    // user asked to stop and the socket is being torn down on purpose - waiting
    // on it here would stall the caller for the whole 1500ms. That delay is paid
    // in front of the user: the orb stays on screen and the tray row stays on
    // "Transcribing..." until it elapses, which is the seam `watch_recording`
    // now reaches by raising `cancelled` during capture.
    //
    // What ends the wait is platform-specific and must not be conflated with
    // what breaks it. The Unix transport's `close()` shuts the socket down
    // outright, so it releases a reader parked in `read` and the whole call
    // returns immediately once the wait is skipped; the Windows transport's
    // `close()` is a negotiated close that does NOT release a parked
    // `WinHttpWebSocketReceive` (windows_impl.rs:649-656), so on Windows
    // skipping the wait still leaves the close handshake and the reader join
    // to bound. The `!cancelled` break shortens the wait on every platform;
    // the bound on the whole call is the Unix transport's property alone.
    let wait_deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < wait_deadline
        && !reader_done.load(Ordering::SeqCst)
        && !cancelled.load(Ordering::SeqCst)
    {
        thread::sleep(Duration::from_millis(20));
    }

    reader_done.store(true, Ordering::SeqCst);
    ws.close();

    let _ = reader_thread.join();

    let full_text = lock_poisoned(&full_transcript).trim().to_string();
    let read_error = lock_poisoned(&read_error).clone();
    let send_error = lock_poisoned(&send_error).clone();
    // A fault from either side is a fault. The reader's error is preferred when
    // both fired because it is the one the provider actually reported, and a
    // send that failed first is usually what made the read fail.
    let lost = read_error.or(send_error);

    // The configured trailing space is a side effect of a session that
    // produced words, and it must not depend on which way the session ended:
    // a lost connection after words were typed would otherwise be the one
    // path that commits text without it.
    if crate::rest::trailing_space_due(cfg.trailing_space, &full_text)
        && !cancelled.load(Ordering::SeqCst)
    {
        input::type_text(" ");
    }

    // A read that failed with nothing typed is a real failure and is reported
    // as one. Once words have been typed the user already has their dictation,
    // so it is kept - but the failure is still stated, in the text the caller
    // shows, because a silent partial result reads as a complete one. The user
    // pressed once and got half a sentence with no sign anything went wrong.
    //
    // No log line here: `windows_app` is Windows-only and this module compiles
    // on all three targets, and the marker travels with the returned text,
    // which every caller already reports.
    if let Some(e) = lost {
        if full_text.is_empty() {
            return Err(format!("streaming transcription failed: {e}"));
        }
        return Ok(format!("{full_text} [connection lost: {e}]"));
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

    /// Delegates to the shared constructor so a new `Config` field is added in
    /// one place, not four.
    fn test_cfg() -> Config {
        crate::config::test_config()
    }

    #[test]
    fn the_listen_url_carries_the_constant_sample_rate() {
        // The wire must name SAMPLE_RATE, not a literal: this is the rate the
        // capture engine resamples to, and a mismatch transcribes as garbage
        // with no error anywhere.
        let url = listen_url(&test_cfg()).expect("the default base URL parses");
        assert!(
            url.contains(&format!("sample_rate={SAMPLE_RATE}")),
            "the wire must carry the constant rate: {url}"
        );
        assert!(
            url.starts_with("wss://api.deepgram.com:443/v1/listen?model=nova-3&"),
            "{url}"
        );
        assert!(
            url.contains("&keyterm=Kubernetes"),
            "nova-3 uses keyterm: {url}"
        );
    }

    #[test]
    fn a_model_or_language_cannot_inject_query_parameters() {
        // The query is the contract. `model` and `language` reached the URL
        // raw while keywords were url_encoded, so a value carrying &, = or #
        // did not merely fail - it silently rewrote the request. `MODEL=x#frag`
        // ended the query, discarding smart_format, interim_results,
        // endpointing, no_delay and vad_events, so the app quietly stopped
        // streaming words with nothing anywhere reporting a problem. Both
        // fields now take the same encoding path as the keyword loop.
        let mut cfg = test_cfg();
        cfg.model = "nova-3&encoding=multi#frag".into();
        cfg.language = "en&keyterm=evil".into();
        let url = listen_url(&cfg).expect("the default base URL parses");

        // The injected parameter must not appear as a parameter of its own.
        assert!(
            !url.contains("&encoding=multi"),
            "an encoded model cannot add encoding=multi: {url}"
        );
        assert!(
            !url.contains("&keyterm=evil"),
            "an encoded language cannot add keyterm=evil: {url}"
        );
        // The query this session declares must still be the whole query: the
        // trailing parameters survive the hostile value instead of being cut
        // off by a `#` fragment.
        for required in [
            "smart_format=true",
            "interim_results=true",
            "endpointing=1500",
            "no_delay=true",
            "vad_events=true",
        ] {
            assert!(
                url.contains(required),
                "{required} must survive a hostile model/language: {url}"
            );
        }
        // And the hostile text is present but escaped, so the provider sees one
        // model name rather than a rewritten request.
        assert!(
            url.contains("model=nova-3%26encoding%3Dmulti%23frag"),
            "the model must arrive percent-encoded: {url}"
        );
        assert!(
            url.contains("language=en%26keyterm%3Devil"),
            "the language must arrive percent-encoded: {url}"
        );
    }

    #[test]
    fn an_ipv6_base_url_is_rebracketed_in_the_listen_url() {
        // parse_base_url yields the bare literal, which is what WinHTTP wants,
        // but a URL authority must bracket it: the Unix transport rejects the
        // unbracketed form, so the session never connects.
        let mut cfg = test_cfg();
        cfg.base_url = "https://[::1]:8443".into();
        let url = listen_url(&cfg).expect("a bracketed IPv6 base URL parses");
        // Drive the real Unix consumer of this string - the same
        // `IntoClientRequest` the transport calls at unix_http.rs:210 - rather
        // than only matching a prefix: the fix exists because that parser
        // rejects an unbracketed authority, so the parse succeeding here IS
        // the behavior under test. The parser lives behind the Unix target
        // gate, so this arm runs where it matters (Linux/macOS) and on Windows
        // the prefix check still pins the shape.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use tungstenite::client::IntoClientRequest;
            let request = url
                .as_str()
                .into_client_request()
                .unwrap_or_else(|e| panic!("the Unix transport must accept {url}: {e}"));
            assert_eq!(request.uri().host(), Some("[::1]"));
            assert_eq!(request.uri().port_u16(), Some(8443));
        }
        assert!(
            url.starts_with("wss://[::1]:8443/v1/listen?"),
            "the authority must bracket the IPv6 literal: {url}"
        );
    }

    /// A BASE_URL that is present but unparseable must be an error, not a
    /// silent redirect to Deepgram: the config is only defaulted when it is
    /// empty, so a typo would otherwise send the audio AND the user's API key
    /// to a provider they did not ask for. REST already fails closed here.
    #[test]
    fn an_unparseable_base_url_is_refused_not_redirected_to_deepgram() {
        let mut cfg = test_cfg();
        cfg.base_url = "localhost:8000".into(); // no scheme
        let err = listen_url(&cfg).expect_err("a scheme-less base URL must not parse");
        assert!(
            err.contains("BASE_URL"),
            "the error must name the config key that is wrong: {err}"
        );
        // A well-formed custom endpoint is still honoured verbatim, which is
        // what makes the refusal above a guard rather than a restriction.
        let mut cfg = test_cfg();
        cfg.base_url = "https://stt.corp/deepgram".into();
        let url = listen_url(&cfg).expect("a well-formed custom base URL parses");
        assert!(
            url.starts_with("wss://stt.corp:443/deepgram?"),
            "a configured path is used verbatim: {url}"
        );
    }

    /// The whole point of the connection-lost work: `run_stream` is driven
    /// against a real WebSocket peer on loopback that speaks Deepgram's
    /// framing, so the string the user is shown comes from the production
    /// function and a real socket - not from a stub. `type_text` is a no-op
    /// test injector under `cfg(test)` (see `platform::input`), so this runs
    /// the real streaming loop without touching the machine's keyboard.
    mod live_provider {
        use super::*;
        use crate::platform::audio::SAMPLE_RATE;
        use std::io::{Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};
        use std::sync::atomic::AtomicBool;

        /// A server-to-client WebSocket frame: server frames are never masked.
        pub fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
            let mut out = vec![0x80 | opcode];
            if payload.len() < 126 {
                out.push(payload.len() as u8);
            } else if payload.len() < 65_536 {
                out.push(126);
                out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            } else {
                out.push(127);
                out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
            out.extend_from_slice(payload);
            out
        }

        /// One frame of a fragmented message: the FIN bit is clear on every
        /// piece but the last.
        ///
        /// RFC 6455 lets a sender split one message across frames, and a peer
        /// that does is legal. The consumer of `read` parses each returned
        /// buffer as one complete JSON document, so a backend that surfaced a
        /// fragment as a whole message would drop the words in it.
        pub fn fragment(opcode: u8, payload: &[u8], fin: bool) -> Vec<u8> {
            let mut out = frame(opcode, payload);
            if !fin {
                out[0] &= 0x7f;
            }
            out
        }

        /// Finish the HTTP upgrade by hand, then run `after_handshake` with a
        /// connected peer. Returns the port the client should dial.
        pub fn serve(
            after_handshake: impl FnOnce(&mut TcpStream) + Send + 'static,
        ) -> (u16, std::thread::JoinHandle<()>) {
            let (port, handle, _head) =
                serve_capturing_head(move |_head, stream| after_handshake(stream));
            (port, handle)
        }

        /// The same handshake, but the request head the client actually sent is
        /// handed to `after_handshake` and returned in a shared slot, so a test
        /// can assert what went on the wire rather than what a URL builder
        /// returned.
        pub fn serve_capturing_head(
            after_handshake: impl FnOnce(String, &mut TcpStream) + Send + 'static,
        ) -> (
            u16,
            std::thread::JoinHandle<()>,
            std::sync::Arc<std::sync::Mutex<String>>,
        ) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let head_slot = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let captured = std::sync::Arc::clone(&head_slot);
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                // Read the request head up to the blank line.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(0) => return,
                        Ok(_) => head.push(byte[0]),
                        Err(_) => return,
                    }
                    if head.len() > 64 * 1024 {
                        return;
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let key = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let accept = tungstenite_accept(&key);
                let reply = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                );
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
                if let Ok(mut slot) = captured.lock() {
                    *slot = head.clone();
                }
                after_handshake(head, &mut stream);
            });
            (port, handle, head_slot)
        }

        /// `Sec-WebSocket-Accept`: base64(sha1(key + GUID)).
        ///
        /// The crate computes this, and the same call is what unix_http.rs's
        /// tests use, so the digest has one implementation across the suite.
        /// This module used to spell SHA-1 and base64 by hand in ~95 lines
        /// because the crate was gated to the Unix targets in Cargo.toml, which
        /// meant the Windows test build could not call it.
        fn tungstenite_accept(key: &str) -> String {
            tungstenite::handshake::derive_accept_key(key.as_bytes())
        }

        /// One Deepgram interim/final result frame, the shape `parse_stream_json`
        /// reads: `channel.alternatives[0].transcript` plus the two booleans.
        pub fn result_frame(transcript: &str, is_final: bool, speech_final: bool) -> Vec<u8> {
            // The `Results` envelope is what `parse_stream_json` keys off before
            // it parses anything, so a metadata frame is rejected cheaply; this
            // is the same shape Deepgram sends.
            let json = format!(
                "{{\"type\":\"Results\",\"channel\":{{\"alternatives\":[{{\"transcript\":\"{transcript}\"}}]}},\"is_final\":{is_final},\"speech_final\":{speech_final}}}"
            );
            frame(0x1, json.as_bytes())
        }

        /// Build the config that points the real streaming loop at `port`.
        pub fn cfg_for(port: u16) -> Config {
            let mut cfg = test_cfg();
            cfg.protocol = crate::config::Protocol::Streaming;
            cfg.base_url = format!("ws://127.0.0.1:{port}");
            cfg.keywords.clear();
            cfg
        }

        /// Feed `packets` of silence into the channel and return the real
        /// `run_stream` result. `stop` is set by the reader itself when it sees
        /// `speech_final`, so the audio channel only has to stay open briefly.
        pub fn drive(cfg: &Config, packets: usize) -> Result<String, String> {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
            let n = SAMPLE_RATE as usize / 10; // 100 ms of silence per packet
            std::thread::spawn(move || {
                for _ in 0..packets {
                    if tx.send(vec![0i16; n]).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let stop = Arc::new(AtomicBool::new(false));
            let cancelled = Arc::new(AtomicBool::new(false));
            run_stream(cfg, &stop, &cancelled, rx)
        }

        /// A provider that answers the handshake and then never reads again.
        ///
        /// The same peer `unix_http`'s own `stalled_peer` builds: the kernel
        /// window fills, so a send blocks for the transport's write bound rather
        /// than failing immediately. That is the difference from a provider that
        /// dropped the connection, whose dead socket answers `Err` at once and
        /// costs nothing.
        ///
        /// Alongside the join handle it returns the release channel. The peer
        /// has to keep the socket open while the session is stalled, but an
        /// unconditional `sleep` banks its whole length on every run: tests
        /// share the thread pool, and the live-provider tests either side of
        /// this one assert against real post-stop waits, so a long fixed window
        /// is a flake the fixture causes rather than a property it measures. A
        /// deadline is kept so a session that never returns cannot leave this
        /// peer holding a socket forever, and the test releases it inside that.
        pub fn serve_stalled() -> (
            u16,
            std::thread::JoinHandle<()>,
            std::sync::mpsc::Sender<()>,
        ) {
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (port, server) = serve(move |stream| {
                let mut sink = [0u8; 64];
                // Read once so the handshake lands, then never read again while
                // holding the socket open.
                let _ = stream.read(&mut sink);
                let _ = release_rx.recv_timeout(Duration::from_secs(20));
            });
            (port, server, release_tx)
        }

        /// A session whose audio producer keeps running past `stop`, so the
        /// post-session drain finds a real backlog.
        ///
        /// The backlog only survives while the send loop is blocked inside
        /// `send_binary` on a full window. If the producer has already quit when
        /// the loop stops, the channel is empty and a test about the drain
        /// passes without the drain doing anything - so the producer here runs
        /// for the whole session.
        pub fn drive_with_a_backlog(cfg: &Config, packets: usize) -> Result<String, String> {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
            let n = SAMPLE_RATE as usize / 10;
            std::thread::spawn(move || {
                for _ in 0..packets {
                    if tx.send(vec![0i16; n]).is_err() {
                        return;
                    }
                }
            });
            let stop = Arc::new(AtomicBool::new(false));
            let cancelled = Arc::new(AtomicBool::new(false));
            let stopper = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1500));
                stopper.store(true, Ordering::SeqCst);
            });
            run_stream(cfg, &stop, &cancelled, rx)
        }
    }

    /// The intent's headline behaviour: a stream that dies after words were
    /// typed must return those words with the lost-connection marker, not a
    /// bare success that reads as a complete dictation.
    #[test]
    fn a_fault_after_words_were_typed_is_returned_with_the_connection_lost_marker() {
        use std::io::Write as _;
        let (port, server) = live_provider::serve(|stream| {
            // Say a sentence, finalise it so the words are committed, then drop
            // the TCP connection with no WebSocket close frame: the shape of a
            // provider that dies mid-dictation.
            let _ = stream.write_all(&live_provider::result_frame("hello world", true, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(300));
            // Returning from the closure lets the server thread end, which
            // closes the socket the client is parked on.
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 40);
        let _ = server.join();

        let text = out.expect("words were typed, so the result must be Ok");
        assert!(
            text.starts_with("hello world"),
            "the typed words must survive: {text}"
        );
        assert!(
            text.contains("[connection lost:"),
            "a fault after words must be marked, not silent: {text}"
        );
    }

    /// A stalled peer must not cost the drain one write timeout per queued packet.
    ///
    /// The drain's job is to ship the queued tail so the provider can finalise
    /// it. Once a send fails the socket can carry no more data, so the packets
    /// behind it cannot be delivered either - continuing only re-pays the
    /// transport's write bound for each one. Measured before this stopped: 50
    /// queued packets cost 1.6 s against a peer that stopped reading and 500
    /// cost 300 s.
    ///
    /// The repeat cost is what this removes, and it is the whole of what it
    /// removes on Windows: `WinHttpWebSocketSend` blocks without returning
    /// `Err`, so against a peer that stalls rather than dies the first send
    /// parks and the break is never reached. Bounding that is a separate change.
    ///
    /// This asserts the decision rather than the clock. On Unix a peer that
    /// drops the connection makes every send fail in microseconds, so a timing
    /// assertion here would pass whether or not the drain breaks early - the
    /// hook is what distinguishes the two.
    #[test]
    fn the_drain_stops_at_the_first_failed_send() {
        let (port, server, release) = live_provider::serve_stalled();
        let cfg = live_provider::cfg_for(port);

        // While the peer cannot read, the live loop's send parks and the
        // session cannot reach the drain until that peer goes away, so the
        // release cannot wait for `run_stream` to return - it has to arrive
        // while the session is stalled. Two seconds is past the point where the
        // kernel window is full (the first ~60 sends land in under 40 ms) and
        // past the helper's 1.5 s stop, so the drain still finds a real backlog
        // to work through, and the suite does not bank the fixture's deadline.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2000));
            let _ = release.send(());
        });

        crate::test_hooks::reset_drain_observation();
        // Far more packets than the kernel window can take, still arriving when
        // the session ends, so the drain has a real backlog to work through.
        let _ = live_provider::drive_with_a_backlog(&cfg, 4000);
        let _ = releaser.join();
        server.join().unwrap();

        assert!(
            crate::test_hooks::drain_broke_early(),
            "the drain must stop at the first failed send, not keep paying the write bound"
        );
        assert!(
            !crate::test_hooks::drain_backlog_exhausted(),
            "the drain must not walk the whole backlog after a failed send - if this fails, the break is gone but the record is not"
        );
    }

    /// A cancel must not wait out the provider's final-transcription window.
    ///
    /// The seam waited up to 1500 ms for the provider's last frame, and the
    /// only thing that could end that wait early was the reader thread - which
    /// on a cancel is parked inside `read()`, waiting for a frame that a
    /// cancelled session will never get. So a cancel paid the full 1500 ms
    /// before the socket was closed, and that wait sits between the user's
    /// keypress and the orb coming down.
    ///
    /// The peer here says nothing at all, so the reader is parked for the
    /// whole session and `reader_done` cannot be what releases the wait.
    ///
    /// "Parked in read" holds for both transports, but what releases it does
    /// not: the Unix `close()` shuts the socket down and ends the parked read,
    /// while the Windows `close()` is a negotiated close that leaves
    /// `WinHttpWebSocketReceive` parked (windows_impl.rs:649-656). The bound
    /// asserted below is therefore the Unix transport's; Windows gets a
    /// generous one that still catches a real hang.
    #[test]
    fn a_cancel_does_not_wait_out_the_final_transcription_window() {
        use std::io::Read as _;
        let (port, server) = live_provider::serve(|stream| {
            // Read once so the handshake lands, then stay silent: the reader
            // parks in `read` with no frame ever coming.
            let mut sink = [0u8; 64];
            let _ = std::io::Read::read(stream, &mut sink);
            // Hold the socket open until the client closes it.
            let _ = stream.read(&mut sink);
        });
        let cfg = live_provider::cfg_for(port);

        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        let n = SAMPLE_RATE as usize / 10;
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));

        // The cancel lands while the session is live and the peer is silent.
        let cancel_flag = Arc::clone(&cancelled);
        let stop_flag = Arc::clone(&stop);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            cancel_flag.store(true, Ordering::SeqCst);
            stop_flag.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let result = run_stream(&cfg, &stop, &cancelled, rx);
        let elapsed = started.elapsed();
        let _ = canceller.join();
        let _ = tx.send(vec![0i16; n]);
        let _ = server.join();

        // The result itself is not what is under test - a cancelled session may
        // or may not have a transcript. The elapsed time is.
        let _ = result;
        // The bound on the WHOLE call is the Unix transport's property: its
        // `close()` shuts the socket down outright, which is what releases the
        // reader parked in `read` and lets the join return. On Windows
        // `close()` is a negotiated close that does NOT release a parked
        // `WinHttpWebSocketReceive` (windows_impl.rs:649-656), so the close
        // handshake alone can spend up to WEBSOCKET_CLOSE_TIMEOUT_MS and the
        // join behind it can add more - the Unix bound would be false there.
        // The fixture is still worth running on Windows: the `!cancelled`
        // break skips the 1500 ms wait on every platform.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let bound = Duration::from_millis(1200);
        #[cfg(target_os = "windows")]
        let bound = Duration::from_secs(10);
        assert!(
            elapsed < bound,
            "a cancel must not wait out the 1500ms final-transcription window \
             on {} (bound {bound:?}, took {elapsed:?}) - the orb stays up for \
             exactly that long",
            if cfg!(target_os = "windows") {
                "Windows (10s close/join bound)"
            } else {
                "the Unix transport (1200ms bound)"
            }
        );
    }

    /// The other half of the boundary the cancel fix draws: the wait may be
    /// broken by `cancelled` alone, but an ordinary stop still pays it. The
    /// final frame here lands 500 ms after the user's stop raised `stop` -
    /// inside the window, after the CloseStream - and the session must still
    /// be sitting in the wait to receive it. This is the guard for the fix's
    /// risk surface: breaking the wait on `stop` as well would hand back a
    /// transcript without its final words, and a cancel would no longer be
    /// the only fast path out.
    #[test]
    fn an_ordinary_stop_still_waits_for_the_final_transcription_frame() {
        use std::io::{Read as _, Write as _};
        let (port, server) = live_provider::serve(|stream| {
            // A clause the session commits while the audio is still flowing.
            let _ = stream.write_all(&live_provider::result_frame("hello world", true, false));
            let _ = stream.flush();
            // The final frame arrives after the stop has ended the send
            // loop: 500 ms of audio, then this 1 s delay puts it 500 ms
            // into the post-stop window.
            std::thread::sleep(Duration::from_millis(1000));
            let _ = stream.write_all(&live_provider::result_frame("late words", true, true));
            let _ = stream.flush();
            // The fixture rule: never close with the client still reading -
            // drain until it closes the socket.
            let mut sink = [0u8; 64];
            let _ = stream.read(&mut sink);
        });
        let cfg = live_provider::cfg_for(port);

        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        let n = SAMPLE_RATE as usize / 10;
        std::thread::spawn(move || {
            // 25 packets x 20 ms = 500 ms of audio, the shape of a short
            // ordinary dictation.
            for _ in 0..25 {
                if tx.send(vec![0i16; n]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        // The hotkey's ordinary stop: raised at the end of the audio,
        // with `cancelled` never touched.
        let stop_flag = Arc::clone(&stop);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            stop_flag.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let out = run_stream(&cfg, &stop, &cancelled, rx);
        let elapsed = started.elapsed();
        let _ = stopper.join();
        let _ = server.join();

        let text = out.expect("the ordinary stop must end in a transcript");
        assert!(
            text.contains("hello world") && text.contains("late words"),
            "the post-stop final frame must still be consumed: {text}"
        );
        assert!(
            elapsed >= Duration::from_millis(950),
            "the session must sit in the wait for the late frame, not return \
             early: took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the wait window is bounded: took {elapsed:?}"
        );
    }

    /// The adversarial counterpart: an orderly close frame must NOT be
    /// reported as a lost connection. Round 5's `draining` flag suppressed
    /// this, round 6 narrowed it to `stop`, and round 7 (the change under
    /// validation) scopes it to the graceful stop; this asserts the ordinary
    /// end stays quiet.
    #[test]
    fn an_orderly_close_frame_after_words_is_not_reported_as_a_lost_connection() {
        use std::io::Write as _;
        let (port, server) = live_provider::serve(|stream| {
            let _ = stream.write_all(&live_provider::result_frame("all good", true, true));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(200));
            let _ = stream.write_all(&live_provider::frame(0x8, &[]));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(300));
            // Returning from the closure lets the server thread end, which
            // closes the socket the client is parked on.
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 40);
        let _ = server.join();

        let text = out.expect("an orderly session is Ok");
        assert_eq!(text, "all good", "a clean close must not be marked: {text}");
    }

    /// The other half of the send-side fix. A provider that takes the socket
    /// away while audio is still flowing used to produce a bare `Ok`: the send
    /// loop broke, the drain and CloseStream and the 1500 ms wait all failed
    /// silently, and the reader had already seen an orderly close frame, so
    /// nothing anywhere recorded the fault and half a sentence was typed with
    /// no marker.
    ///
    /// The server here sends an orderly close frame first and only then lets the
    /// connection go, so the reader has already taken the close as a clean end
    /// (`Ok(None)`, which must not be marked) and never reports an error: the
    /// ONLY fault the client can see is on its send side, while audio is still
    /// flowing. That is what makes this the send-side test rather than a second
    /// copy of the read one.
    #[test]
    fn a_send_failure_mid_dictation_is_marked_even_though_the_read_closed_cleanly() {
        use std::io::Write as _;
        let (port, server) = live_provider::serve(|stream| {
            // Words first, committed as final so they are already typed.
            let _ = stream.write_all(&live_provider::result_frame("half a sentence", true, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(300));
            // An orderly close from the provider's side: not a fault in itself.
            let _ = stream.write_all(&live_provider::frame(0x8, &[]));
            let _ = stream.flush();
            // Stay connected long enough for the reader to take the close frame
            // as an orderly end - a close frame is not a fault - and only then
            // let the connection go, which is what the client's send side runs
            // into while audio is still flowing.
            std::thread::sleep(Duration::from_millis(500));
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 100);
        let _ = server.join();

        let text = out.expect("words were typed, so the result must be Ok");
        assert!(
            text.starts_with("half a sentence"),
            "the typed words must survive: {text}"
        );
        assert!(
            text.contains("[connection lost:"),
            "a mid-dictation socket death on the send side must be marked: {text}"
        );
    }

    /// A provider that fragments its messages must still deliver whole
    /// messages to the transcript parser.
    ///
    /// RFC 6455 allows one message to arrive as several frames, and the
    /// consumer of `read` parses every buffer it receives as one complete JSON
    /// document. The Unix backend reassembles through tungstenite; the Windows
    /// backend had to do it itself, because `WinHttpWebSocketReceive` reports a
    /// fragment type and the code returned the fragment as if the message were
    /// whole - so the JSON failed to parse and the words were dropped, on
    /// Windows only. This pins the contract both backends must meet.
    #[test]
    fn a_fragmented_message_is_reassembled_before_it_is_parsed() {
        use std::io::Write as _;
        let (port, server) = live_provider::serve(|stream| {
            // One JSON result frame split across two frames, with the FIN bit
            // clear on the first. The split lands mid-token, which is what a
            // real fragmenting sender does.
            let json = r#"{"type":"Results","channel":{"alternatives":[{"transcript":"fragmented words"}]},"is_final":true,"speech_final":true}"#;
            let (a, b) = json.split_at(json.len() / 2);
            // RFC 6455: the first frame carries the message opcode (text) with
            // FIN clear, and every following frame is a continuation (opcode 0).
            let _ = stream.write_all(&live_provider::fragment(0x1, a.as_bytes(), false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(50));
            let _ = stream.write_all(&live_provider::fragment(0x0, b.as_bytes(), true));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(400));
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 20);
        let _ = server.join();

        let text = out.expect("a fragmented message is not an error");
        assert!(
            text.contains("fragmented words"),
            "a fragmented result must be reassembled and typed, got: {text:?}"
        );
    }

    /// One incoming message cannot be made to grow the process without limit.
    ///
    /// `read` reassembles fragments before handing anything back, so a peer
    /// that keeps sending continuations keeps the buffer growing until it
    /// either completes or is refused. tungstenite refused at its own default
    /// of 64 MiB and the WinHTTP reader had no bound at all, which is the
    /// mistyped-`BASE_URL` risk `MAX_TRANSCRIPT_BYTES` already covers on the
    /// REST path: a transcript frame is a few hundred bytes, so a message this
    /// large is a broken or hostile provider, not a long dictation. Both
    /// transports now refuse at `MAX_WS_MESSAGE_BYTES`, and this drives the
    /// shared seam so neither can regress alone.
    #[test]
    fn an_over_long_message_is_refused_rather_than_buffered() {
        use crate::platform::http::{NativeTransport, MAX_WS_MESSAGE_BYTES};
        use std::io::Write as _;

        // One byte past the ceiling, in frames the peer never marks final
        // until the last one: the reader has to accumulate all of it before it
        // could decide the message was merely large.
        let payload = vec![b'a'; MAX_WS_MESSAGE_BYTES + 1];
        let (port, server) = live_provider::serve(move |stream| {
            let chunk = 256 * 1024;
            let mut off = 0;
            while off < payload.len() {
                let n = (payload.len() - off).min(chunk);
                let last = off + n >= payload.len();
                let opcode = if off == 0 { 0x1 } else { 0x0 };
                if stream
                    .write_all(&live_provider::fragment(
                        opcode,
                        &payload[off..off + n],
                        last,
                    ))
                    .is_err()
                {
                    return;
                }
                let _ = stream.flush();
                off += n;
            }
            // Hold the socket open while the client decides, then let it go.
            std::thread::sleep(Duration::from_millis(500));
        });

        let url = format!("ws://127.0.0.1:{port}/");
        let socket = NativeTransport
            .websocket(&url, &[("Authorization", "Bearer tok")])
            .expect("the loopback upgrade succeeds");
        let read = socket
            .read()
            .expect_err("an over-long message must not be delivered as a transcript");
        // Each transport words its own refusal (WinHTTP says "message
        // exceeded", tungstenite surfaces its frame error), so what both can be
        // held to is the number: the bound that fired is the one this crate set.
        assert!(
            read.contains(&MAX_WS_MESSAGE_BYTES.to_string()),
            "the refusal must name the bound that fired, got: {read}"
        );
        let _ = server.join();
    }

    /// A final frame that carries no words still ends the clause.
    ///
    /// Deepgram sends `is_final` frames with an empty transcript (a clause that
    /// resolved to nothing, or a channel-metadata close-out), and the word
    /// cursor must follow the clause boundary rather than the data. Left inside
    /// the non-empty guard, the cursor kept pointing into the clause that had
    /// just closed: the next clause's interims failed
    /// `words.len() - 1 > typed_word_count` and typed nothing, and its final
    /// frame committed from the wrong offset, so the leading words of every
    /// clause after an empty final were lost.
    ///
    /// The fixture must reach that state, which is not the obvious shape: the
    /// empty final has to follow an interim DIRECTLY. A non-empty final
    /// beforehand would itself run the reset, and the first version of this
    /// test had exactly that, so it passed against the pre-fix code and pinned
    /// nothing (verified: it went green with the reset moved back inside the
    /// guard). Verified both ways now - pre-fix types "one two six", losing
    /// clause two's leading words, fixed types "four five six".
    #[test]
    fn an_empty_final_frame_resets_the_clause_cursor() {
        use std::io::Write as _;
        let (port, server) = live_provider::serve(|stream| {
            // Clause one: an interim, then a final that is EMPTY. That is the
            // legal frame the bug tripped on, and it has to follow the interim
            // DIRECTLY: a non-empty final in between would itself run the
            // reset, which made the first version of this test pass against
            // the pre-fix code and pin nothing. An empty final also means the
            // provider finalised the clause with no transcript, so "three" -
            // the trailing word only a final would commit - is never typed,
            // and the assertions below must not ask for it.
            let _ = stream.write_all(&live_provider::result_frame("one two three", false, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(120));
            let _ = stream.write_all(&live_provider::result_frame("", true, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(120));
            // Clause two: with a stale cursor of 2, "four" is never typed and
            // the final commits only "six". With the cursor reset to 0, the
            // interim types "four" and the final commits the whole clause.
            let _ = stream.write_all(&live_provider::result_frame("four five", false, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(120));
            let _ = stream.write_all(&live_provider::result_frame("four five six", true, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(120));
            let _ = stream.write_all(&live_provider::result_frame("", false, true));
            let _ = stream.flush();
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 60);
        let _ = server.join();
        let text = out.expect("the session must not error");
        // Clause one's interim words: "three" is deliberately absent, see the
        // fixture comment - an empty final commits nothing.
        assert!(
            text.contains("one") && text.contains("two"),
            "clause one's interim words must be typed: {text}"
        );
        assert!(
            text.contains("four") && text.contains("five") && text.contains("six"),
            "clause two must be typed whole after an empty final frame: {text}"
        );
    }

    /// A legal empty data message must not be read as a hangup.
    ///
    /// Both backends report a real close as `Ok(None)`, so an empty buffer can
    /// only be an empty message - which the reader used to treat as the end of
    /// the stream, exiting without a fault marker and returning a truncated
    /// transcript as if it were complete.
    #[test]
    fn an_empty_data_message_does_not_end_the_stream() {
        use std::io::Write as _;
        let (port, server) = live_provider::serve(|stream| {
            let _ = stream.write_all(&live_provider::result_frame("first words", true, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(120));
            // A zero-length text frame: legal, carries nothing, ends nothing.
            let _ = stream.write_all(&live_provider::frame(0x1, b""));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(120));
            // Words that only arrive if the reader survived the empty frame.
            let _ = stream.write_all(&live_provider::result_frame("later words", true, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(120));
            let _ = stream.write_all(&live_provider::result_frame("", false, true));
            let _ = stream.flush();
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 60);
        let _ = server.join();
        let text = out.expect("an empty message is not an error");
        assert!(
            text.contains("later words"),
            "the reader must survive an empty data message: {text}"
        );
    }

    /// The wire-level counterpart of `a_model_or_language_cannot_inject_query_parameters`:
    /// the same hostile values, observed as the request line the real transport
    /// puts on a real socket, rather than as a string the URL builder returned.
    /// A URL that looks right can still be rewritten by a client that
    /// normalizes it, so the bytes are what prove the contract.
    ///
    /// It also pins the two things the intent forbids changing: the per-transport
    /// auth scheme (Deepgram takes `Token`, and a user-supplied prefix passes
    /// through) and the trailing parameters that make a session stream words at
    /// all.
    #[test]
    fn the_request_on_the_wire_survives_a_hostile_model_and_language() {
        use std::io::Write as _;
        let (port, server, head) = live_provider::serve_capturing_head(|_head, stream| {
            std::thread::sleep(Duration::from_millis(200));
            let _ = stream.write_all(&live_provider::result_frame("wire", true, true));
            let _ = stream.flush();
        });
        let mut cfg = live_provider::cfg_for(port);
        cfg.model = "nova-3&encoding=multi#frag".into();
        cfg.language = "en&keyterm=evil".into();
        cfg.api_key = "Token my-own-prefix".into();
        let out = live_provider::drive(&cfg, 10);
        let _ = server.join();
        let head = lock_poisoned(&head).clone();
        let request_line = head.lines().next().unwrap_or_default().to_string();

        assert!(
            out.is_ok(),
            "a hostile model must not break the session: {out:?}"
        );
        // The hostile text arrives as one encoded value, and the parameters the
        // session declares are all still there as parameters of their own.
        assert!(
            request_line.contains("model=nova-3%26encoding%3Dmulti%23frag"),
            "the model must arrive percent-encoded: {request_line}"
        );
        assert!(
            request_line.contains("language=en%26keyterm%3Devil"),
            "the language must arrive percent-encoded: {request_line}"
        );
        for required in [
            "smart_format=true",
            "interim_results=true",
            "endpointing=1500",
            "no_delay=true",
            "vad_events=true",
        ] {
            assert!(
                request_line.contains(required),
                "{required} must survive on the wire: {request_line}"
            );
        }
        assert!(
            !request_line.contains("&encoding=multi") && !request_line.contains("&keyterm=evil"),
            "no injected parameter may reach the provider: {request_line}"
        );
        // The auth scheme is untouched: a user-supplied prefix is passed
        // through rather than being re-wrapped as `Token Token ...`.
        //
        // HTTP field names are case-insensitive, and the two backends spell
        // this one differently: WinHTTP writes the name the caller handed it,
        // while the Unix handshake emits every caller-supplied header
        // lowercased (tungstenite writes `http::HeaderName::as_str()`, which is
        // always lowercase). The value is the contract, so the field name is
        // matched case-insensitively and the value is compared exactly.
        let auth = head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("authorization") {
                Some(value.trim())
            } else {
                None
            }
        });
        assert_eq!(
            auth,
            Some("Token my-own-prefix"),
            "a prefixed key must pass through unchanged: {head}"
        );
    }

    /// The other half of the intent: when nothing was typed, the fault must be
    /// an error the caller reports, not the old "no speech detected".
    #[test]
    fn a_fault_before_any_words_is_an_error_not_a_silent_empty_result() {
        let (port, server) = live_provider::serve(|_stream| {
            std::thread::sleep(Duration::from_millis(200));
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 40);
        let _ = server.join();

        let err = out.expect_err("nothing typed and the peer died is a failure");
        assert!(
            err.contains("streaming transcription failed"),
            "the error must say the stream failed: {err}"
        );
    }

    /// An empty `speech_final` frame must still end the session.
    ///
    /// `speech_final` is the flag that says the provider has finished an
    /// utterance, and the frame carrying it can legitimately have no words in
    /// it. The check used to sit inside the `if !trimmed.is_empty()` block, so
    /// an empty final frame never set `stop`: the session ran to MAX_SECONDS
    /// instead of ending when speech did, and the missed flag left
    /// `typed_word_count` pointing into the clause that had just ended, so the
    /// next clause's interims typed or dropped the wrong words.
    ///
    /// The discriminator is a frame sent AFTER the empty final one. With the
    /// flag honoured the send loop has already broken, so the session's close
    /// path has run and this frame is never read; with the flag missed the
    /// session is still consuming audio when it arrives, so its words are typed.
    /// The producer therefore has to outlive the frame (3 s of packets against
    /// a 1.5 s post-stop wait) - a short producer would end the session through
    /// the channel disconnect instead, and the test would pass either way.
    #[test]
    fn an_empty_speech_final_frame_still_ends_the_session() {
        use std::io::Write as _;
        let (port, server) = live_provider::serve(|stream| {
            // A real clause, finalised, so its words are committed.
            let _ = stream.write_all(&live_provider::result_frame("one two three", true, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(150));
            // The utterance-ending frame, with nothing in it. This is the frame
            // whose flag the bug dropped.
            let _ = stream.write_all(&live_provider::result_frame("", false, true));
            let _ = stream.flush();
            // Long enough that the session's own close path has certainly run
            // before this arrives - 1.5 s of post-stop wait plus slack.
            std::thread::sleep(Duration::from_millis(2200));
            // If the flag was missed this is still being read, and its words
            // reach the user's window.
            let _ = stream.write_all(&live_provider::result_frame("four five six", true, false));
            let _ = stream.flush();
        });
        let cfg = live_provider::cfg_for(port);
        // 150 packets x 20 ms = 3 s of audio, so the channel is still open when
        // the late frame arrives unless `stop` ended the send loop first.
        let out = live_provider::drive(&cfg, 150);
        let _ = server.join();
        let text = out.expect("the session must not error");
        assert!(
            text.contains("one two three"),
            "the committed words must survive: {text}"
        );
        assert!(
            !text.contains("four five six"),
            "a frame sent after speech_final must not be read: {text}"
        );
    }
}
