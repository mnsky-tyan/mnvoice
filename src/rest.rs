// REST client for OpenAI-compatible speech-to-text endpoints.
// Compatible with any standard audio/transcriptions endpoint (self-hosted Whisper, Groq, OpenAI, etc.).
// Requests go through the platform transport seam: WinHTTP with the system cert store and
// proxy settings on Windows, ureq with rustls (bundled roots) or native-tls elsewhere.

use crate::config::{auth_value, Config};
use crate::platform::http::Transport;

const BOUNDARY: &str = "mnvoiceboundary9f2a";

/// Transcribe a WAV clip using an OpenAI-compatible REST endpoint. Returns plain text.
pub fn transcribe(cfg: &Config, wav: &[u8]) -> Result<String, String> {
    let url = endpoint_url(&cfg.base_url)?;
    let body = multipart_body(cfg, wav);
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");

    let response = crate::platform::http::NativeTransport.post(
        &url,
        Some(&auth_value(&cfg.api_key, "Bearer")),
        &content_type,
        &body,
    )?;

    if response.status != 200 {
        return Err(response.error_for_status("ASR endpoint"));
    }

    let raw_text = String::from_utf8_lossy(&response.body);
    let parsed =
        parse_json_transcript(&raw_text).unwrap_or_else(|| raw_text.trim().to_string());
    Ok(parsed.trim().to_string())
}

/// The URL a transcription POST goes to.
///
/// A `BASE_URL` that already carries a path is the whole endpoint: that is how
/// a custom endpoint or a reverse proxy is configured, so the default path is
/// appended only when the base URL has none. Appending it anyway would ask the
/// provider for `/deepgram/deepgram`. The base URL is used exactly as written
/// rather than trimmed, because a trailing slash in it is part of the
/// configured endpoint - the Windows release this port must not change posts
/// to that path verbatim, and trimming it would send the request somewhere
/// else (an empty path, for a base URL whose path is just `/`).
fn endpoint_url(base_url: &str) -> Result<String, String> {
    let (host, _port, _secure, base_path) = parse_base_url(base_url)?;
    if !base_path.is_empty() {
        return Ok(base_url.to_string());
    }
    let endpoint_path = if host.contains("groq.com") {
        "/openai/v1/audio/transcriptions"
    } else {
        "/v1/audio/transcriptions"
    };
    Ok(format!("{base_url}{endpoint_path}"))
}

pub fn parse_json_transcript(json: &str) -> Option<String> {
    for key in ["\"text\"", "\"transcript\""] {
        if let Some(key_pos) = json.find(key) {
            let after_key = &json[key_pos + key.len()..];
            if let Some(colon_pos) = after_key.find(':') {
                let after_colon = after_key[colon_pos + 1..].trim_start();
                if let Some(s) = after_colon.strip_prefix('"') {
                    let mut out = String::new();
                    let mut chars = s.chars();
                    while let Some(c) = chars.next() {
                        match c {
                            '"' => return Some(out),
                            '\\' => {
                                match chars.next()? {
                                    '"' => out.push('"'),
                                    '\\' => out.push('\\'),
                                    '/' => out.push('/'),
                                    'b' => out.push('\x08'),
                                    'f' => out.push('\x0c'),
                                    'n' => out.push('\n'),
                                    'r' => out.push('\r'),
                                    't' => out.push('\t'),
                                    'u' => {
                                        let mut hex = String::with_capacity(4);
                                        for _ in 0..4 {
                                            hex.push(chars.next()?);
                                        }
                                        if let Ok(code) = u16::from_str_radix(&hex, 16) {
                                            if let Some(ch) = char::from_u32(code as u32) {
                                                out.push(ch);
                                            }
                                        }
                                    }
                                    other => out.push(other),
                                }
                            }
                            other => out.push(other),
                        }
                    }
                }
            }
        }
    }
    None
}

/// Disfluency tokens. These are vocal stumbles that carry no meaning in
/// dictation, so dropping them cannot change what was said.
///
/// Deliberately excluded: "er" (ER / emergency room), "like" ("I'd like"),
/// "you know" and "i mean" - all common real speech. Also excluded: the
/// spoken affirmatives ("uh-huh", "mm-hmm", "mhm"), which answer questions -
/// dictating "uh-huh, do that" must not type "do that". An over-eager list
/// that eats meaningful words is far worse than a missed filler.
const DISFLUENCIES: &[&str] = &[
    "uh", "uhh", "um", "umm", "erm", "errm", "hmm", "hm", "mm", "mmm",
];

/// True if the token is a disfluency, ignoring surrounding punctuation and case.
pub fn is_disfluency(word: &str) -> bool {
    let norm = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
    if norm.is_empty() {
        return false;
    }
    DISFLUENCIES.iter().any(|d| norm.eq_ignore_ascii_case(d))
}

/// Remove disfluency tokens from a transcript and normalise whitespace.
/// Used on the REST path, where no provider has a native filler_words
/// parameter, so the local stoplist is the only filter. The streaming path
/// filters word-by-word as commits arrive (see stream.rs), which is why this
/// does not appear there.
pub fn strip_disfluencies(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| !is_disfluency(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the REST path types for a transcript, and whether a trailing space
/// follows it.
///
/// FILLER_WORDS=1 keeps the transcript verbatim - the streaming path asks the
/// provider for that instead, because REST has no such parameter anywhere -
/// and TRAILING_SPACE only ever appends to text that was actually typed. Both
/// control surfaces state the rule the same way; it lives here because the
/// Windows tray and the Unix terminal CLI would otherwise each keep their own
/// copy and drift.
/// True when a trailing space should be appended: trailing space is enabled
/// and the typed text is non-empty. Single-sourced so REST and streaming
/// agree by construction.
pub fn trailing_space_due(trailing_space: bool, text: &str) -> bool {
    trailing_space && !text.is_empty()
}

pub fn rest_typing(raw: &str, strip_fillers: bool, trailing_space: bool) -> (String, bool) {
    let text = if strip_fillers {
        strip_disfluencies(raw.trim())
    } else {
        raw.trim().to_string()
    };
    let space = trailing_space_due(trailing_space, &text);
    (text, space)
}

/// Transcribes captured `samples` and types the cleaned text.
///
/// Returns the transcript; an empty string means the provider heard nothing.
/// The tray app and the CLI each drain their capture channel their own way
/// (the tray blocks until the capture closes it, the CLI drains after its
/// capture thread reports done) - the glue after the drain is the part that
/// must not drift.
///
/// `cancelled` is honoured here for the same reason `run_stream` honours it:
/// a cancel must discard the session, so a cancelled recording is neither
/// uploaded to the provider nor typed. Without this the REST path shipped the
/// buffer and typed the words while the caller reported "nothing typed".
pub fn dictate_rest(
    cfg: &Config,
    samples: &[i16],
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<String, String> {
    if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(String::new());
    }
    let wav = crate::platform::audio::wav_bytes(samples);
    let raw = transcribe(cfg, &wav)?;
    // No provider here exposes a native filler_words parameter, so
    // disfluencies are removed locally before anything is typed.
    let (text, trailing) = rest_typing(&raw, cfg.strip_fillers, cfg.trailing_space);
    // Re-check after the round trip: the user may have cancelled while the
    // provider was working, and typing then would be the same lie as before.
    if !text.is_empty() && !cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        crate::platform::input::type_text(&text);
        if trailing {
            crate::platform::input::type_text(" ");
        }
    }
    Ok(text)
}

pub fn parse_base_url(url: &str) -> Result<(String, u16, bool, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("bad BASE_URL: {url}"))?;
    // An allow-list, not a deny-list: anything that is not one of these four
    // is refused rather than treated as cleartext. Treating "not https" as
    // "plain http" silently downgrades a mistyped scheme - ftp:// or gopher://
    // would parse fine and then post the transcript unencrypted to a host the
    // user wrote believing it was secure.
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" | "wss" => true,
        "http" | "ws" => false,
        _ => return Err(format!("unsupported scheme {scheme}:// in BASE_URL (use http, https, ws or wss): {url}")),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{}", p.trim_start_matches('/'))),
        None => (rest, String::new()),
    };
    // Userinfo is stripped before the host/port split: `rsplit_once(':')` on
    // "user:pass@host:8080" would otherwise hand back "user:pass@host" as the
    // host, which no resolver can answer. The authority parser in the http
    // crate drops userinfo the same way, so both ends now normalise alike.
    let authority = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => host,
        None => authority,
    };
    let (host, port) = if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| format!("unclosed IPv6 bracket in BASE_URL: {url}"))?;
        let ip = &authority[1..end];
        let after = &authority[end + 1..];
        let port = if let Some(port_str) = after.strip_prefix(':') {
            port_str
                .parse::<u16>()
                .map_err(|_| format!("bad port in BASE_URL: {url}"))?
        } else if secure {
            443
        } else {
            80
        };
        (ip.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| format!("bad port in BASE_URL: {url}"))?,
            ),
            _ => (authority.to_string(), if secure { 443 } else { 80 }),
        }
    };
    Ok((host, port, secure, path))
}

fn multipart_body(cfg: &Config, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);
    let field = |body: &mut Vec<u8>, name: &str, value: &str| {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };
    field(&mut body, "model", &cfg.model);
    field(&mut body, "language", &cfg.language);
    field(&mut body, "response_format", "text");
    if !cfg.keywords.is_empty() {
        field(&mut body, "prompt", &cfg.keywords.join(", "));
    }
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cancelled REST session must not upload and must not type. The guard
    /// runs before any network or injection, so a cancelled call is
    /// observable here without a provider: it returns an empty transcript
    /// without ever reaching `transcribe`.
    #[test]
    fn a_cancelled_rest_session_neither_uploads_nor_types() {
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        // A real config whose base URL is unreachable: if the guard ever
        // stopped short-circuiting, this call would attempt the network and
        // fail here instead of returning Ok, which is exactly the regression
        // this test exists to catch.
        let cfg = crate::config::test_config();
        let out = dictate_rest(&cfg, &[0i16; 640], &cancelled);
        assert_eq!(out, Ok(String::new()));
    }

    #[test]
    fn test_parse_json_transcript_basic() {
        let json = r#"{"text":"hello world"}"#;
        assert_eq!(parse_json_transcript(json), Some("hello world".to_string()));
    }

    #[test]
    fn test_parse_json_transcript_nested() {
        let json = r#"{"results":{"channels":[{"alternatives":[{"transcript":"deepgram format"}]}]}}"#;
        assert_eq!(parse_json_transcript(json), Some("deepgram format".to_string()));
    }

    #[test]
    fn test_parse_json_transcript_escapes() {
        let json = r#"{"text":"line 1\nline 2 \"quoted\""}"#;
        assert_eq!(parse_json_transcript(json), Some("line 1\nline 2 \"quoted\"".to_string()));
    }

    #[test]
    fn test_parse_base_url() {
        let (host, port, secure, path) = parse_base_url("https://api.groq.com/openai/v1/audio/transcriptions").unwrap();
        assert_eq!(host, "api.groq.com");
        assert_eq!(port, 443);
        assert!(secure);
        assert_eq!(path, "/openai/v1/audio/transcriptions");

        let (host, port, secure, path) = parse_base_url("http://localhost:8000").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 8000);
        assert!(!secure);
        assert_eq!(path, "");

        // IPv6 literal with port and path
        let (host, port, secure, path) = parse_base_url("https://[::1]:8443/custom").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 8443);
        assert!(secure);
        assert_eq!(path, "/custom");

        // IPv6 literal without explicit port
        let (host, port, secure, path) = parse_base_url("http://[fe80::1]/").unwrap();
        assert_eq!(host, "fe80::1");
        assert_eq!(port, 80);
        assert!(!secure);
        assert_eq!(path, "/");
    }

    /// A scheme that is neither http(s) nor ws(s) used to parse as cleartext,
    /// so a mistyped or hostile `BASE_URL` silently downgraded the transcript
    /// to an unencrypted request instead of being refused.
    #[test]
    fn an_unsupported_scheme_is_refused_rather_than_downgraded_to_cleartext() {
        for url in [
            "ftp://api.deepgram.com",
            "gopher://example.com/x",
            "file:///etc/passwd",
        ] {
            let err = parse_base_url(url).unwrap_err();
            assert!(
                err.contains("unsupported scheme"),
                "{url} was accepted or mis-reported: {err}"
            );
        }
    }

    /// The four schemes the app actually speaks must all still parse, and the
    /// secure pair must still mark themselves secure.
    #[test]
    fn the_four_supported_schemes_all_parse() {
        for (url, want_secure) in [
            ("http://h:1/p", false),
            ("https://h:1/p", true),
            ("ws://h:1/p", false),
            ("wss://h:1/p", true),
        ] {
            let (_, _, secure, path) = parse_base_url(url).unwrap();
            assert_eq!(secure, want_secure, "{url}");
            assert_eq!(path, "/p", "{url}");
        }
    }

    /// `rsplit_once(':')` used to hand "user:pass@host" back as the host, so
    /// credentials in the URL left the connector with an unresolvable name.
    /// The userinfo is dropped before the host/port split, which also matches
    /// what the http crate's authority parser does.
    #[test]
    fn credentials_in_the_url_are_stripped_before_the_host_is_split() {
        let (host, port, _, _) = parse_base_url("https://user:pass@api.example.com:8443/p").unwrap();
        assert_eq!(host, "api.example.com");
        assert_eq!(port, 8443);

        let (host, port, _, _) = parse_base_url("http://token@api.example.com/p").unwrap();
        assert_eq!(host, "api.example.com");
        assert_eq!(port, 80);

        // Credentials in front of an IPv6 literal must not defeat the
        // bracket handling either.
        let (host, port, _, _) = parse_base_url("http://user@[::1]:8080/p").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 8080);
    }

    #[test]
    fn test_endpoint_url_uses_a_configured_path_verbatim() {
        // A custom endpoint or reverse proxy is configured as a BASE_URL that
        // already carries the path, so the default path must not be appended
        // to it a second time.
        assert_eq!(
            endpoint_url("https://stt.corp/deepgram").unwrap(),
            "https://stt.corp/deepgram"
        );
        assert_eq!(
            endpoint_url("https://stt.corp:8443/listen").unwrap(),
            "https://stt.corp:8443/listen"
        );
        // A trailing slash is part of the configured endpoint, so it is kept.
        // Trimming it changed where the request went: the Windows release this
        // port must not change posts to that path verbatim.
        assert_eq!(
            endpoint_url("https://stt.corp/deepgram/").unwrap(),
            "https://stt.corp/deepgram/"
        );
        // The sharpest case: a base URL whose path is just "/". Trimming it
        // left an empty path, which is not a request the transport can make.
        assert_eq!(endpoint_url("https://host/").unwrap(), "https://host/");
    }

    #[test]
    fn test_endpoint_url_appends_the_default_path_only_when_absent() {
        assert_eq!(
            endpoint_url("https://api.deepgram.com").unwrap(),
            "https://api.deepgram.com/v1/audio/transcriptions"
        );
        assert_eq!(
            endpoint_url("https://api.groq.com").unwrap(),
            "https://api.groq.com/openai/v1/audio/transcriptions"
        );
        assert_eq!(
            endpoint_url("http://localhost:8000").unwrap(),
            "http://localhost:8000/v1/audio/transcriptions"
        );
    }

    #[test]
    fn test_strip_disfluencies_removes_fillers() {
        assert_eq!(strip_disfluencies("so um this is uh the plan"), "so this is the plan");
    }

    /// FILLER_WORDS=1 keeps the transcript verbatim. The streaming path asks
    /// the provider for that, but REST has no such parameter anywhere, so the
    /// local stoplist is the only lever - and the setting has to reach it, or a
    /// user who asked for verbatim gets filtered text with no way to tell.
    #[test]
    fn filler_words_off_keeps_the_rest_transcript_verbatim() {
        let (text, _) = rest_typing("so um this is uh the plan", false, true);
        assert_eq!(text, "so um this is uh the plan");
    }

    /// FILLER_WORDS=0 (the default) strips, on REST exactly as it strips
    /// everywhere else.
    #[test]
    fn filler_words_on_strips_the_rest_transcript() {
        let (text, _) = rest_typing("so um this is uh the plan", true, true);
        assert_eq!(text, "so this is the plan");
    }

    /// TRAILING_SPACE appends after each dictation. The streaming loop and both
    /// control surfaces do it; without it two consecutive dictations run
    /// together in the focused window as "hello worldagain".
    #[test]
    fn trailing_space_appends_only_after_a_real_transcript() {
        assert!(trailing_space_due(true, "hello"));
        assert!(!trailing_space_due(true, ""));
        assert!(!trailing_space_due(false, "hello"));
        assert!(!trailing_space_due(false, ""));

        let (_, space) = rest_typing("hello world", true, true);
        assert!(space);
        let (_, space) = rest_typing("hello world", true, false);
        assert!(!space);
    }

    /// An empty transcript must not be typed at all, and must not leave a lone
    /// space behind either.
    #[test]
    fn an_empty_rest_transcript_types_nothing() {
        let (text, space) = rest_typing("   ", true, true);
        assert!(text.is_empty());
        assert!(!space);
    }

    #[test]
    fn test_strip_disfluencies_punctuation_and_case() {
        assert_eq!(strip_disfluencies("well, Um... hmm the results, erm, look good"),
                   "well, the results, look good");
    }

    #[test]
    fn test_strip_disfluencies_keeps_meaningful_repeats() {
        // Consecutive duplicates are legitimate speech, not fillers.
        assert_eq!(strip_disfluencies("it was very very good"), "it was very very good");
        assert_eq!(strip_disfluencies("no no no wait"), "no no no wait");
    }

    #[test]
    fn test_strip_disfluencies_keeps_ambiguous_words() {
        // These are real speech, so they must survive untouched.
        assert_eq!(strip_disfluencies("I would like a er scan"), "I would like a er scan");
        assert_eq!(strip_disfluencies("no, like, seriously"), "no, like, seriously");
    }

    #[test]
    fn test_is_disfluency() {
        // Every entry in the list, plus case and punctuation variants, must match.
        for w in ["uh", "uhh", "um", "umm", "erm", "errm", "hmm", "hm", "mm", "Mm,", "HMM."] {
            assert!(is_disfluency(w), "{w} should be a filler");
        }
        // Ambiguous tokens must NOT match - eating these would corrupt real speech.
        for w in ["like", "er", "very", "so", "the", "you", "mean"] {
            assert!(!is_disfluency(w), "{w} should NOT be a filler");
        }
        // Spoken affirmatives answer questions, so they are real speech and
        // must survive the filter.
        for w in ["uh-huh", "mm-hmm", "mhm", "uh-hum"] {
            assert!(!is_disfluency(w), "{w} is an affirmative, not a filler");
        }
    }

    /// The two response ceilings must be different ceilings, not one shared
    /// number: a transcript is a few kilobytes and a release executable is
    /// several megabytes, and the intent requires the asset path to keep
    /// headroom the transcript path does not. This drives the real Windows
    /// transport (`transcribe` -> `WinHttpTransport::post` -> the capped
    /// `read_body`) against a loopback server that answers with more bytes than
    /// `MAX_TRANSCRIPT_BYTES`, and asserts the body really was clipped there.
    ///
    /// It is deliberately not a test of truncation *silently* happening - that
    /// behaviour was reviewed and declined - but of the two bounds being
    /// distinct, which is what the split was for.
    #[test]
    fn a_transcript_response_is_bounded_by_the_transcript_ceiling() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        // One byte past the transcript ceiling, and nowhere near the asset one.
        let body_len = crate::platform::http::MAX_TRANSCRIPT_BYTES as usize + 1;
        assert!(
            body_len < crate::platform::http::MAX_ASSET_BYTES as usize,
            "the fixture only makes sense while the ceilings differ"
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(20)));
            // A blocked write must not wedge the server thread: the client
            // stops reading at its ceiling, so once the socket buffers fill the
            // server can only make progress by giving up on the rest of the
            // body. Without this bound the server can park in `write` forever
            // while the client waits for bytes that will never come.
            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(20)));
            // Consume the whole request, head and body, before answering. If the
            // server answers while WinHTTP is still sending its multipart body,
            // the client's own write can fail and the read sees a reset instead
            // of the response - which would make this test measure the wrong
            // thing entirely.
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => head.push(byte[0]),
                }
            }
            let head = String::from_utf8_lossy(&head).to_string();
            let want: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);
            let mut got = 0usize;
            let mut sink = [0u8; 16 * 1024];
            while got < want {
                match stream.read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got += n,
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            // A JSON body whose first characters are a valid transcript, then
            // padding: an uncapped read would return all of it.
            let prefix = b"{\"text\":\"ok\"}";
            if stream.write_all(prefix).is_err() {
                return;
            }
            // The padding is written in bounded chunks rather than one large
            // `write_all`, so a full socket buffer surfaces as one failed
            // chunk - which ends the loop - instead of an unbounded write. The
            // write is expected to fail once the client has read its ceiling
            // and stopped: "connection reset by peer" or a timed-out write here
            // is the fixture working, not a fault. It is ignored on purpose.
            let chunk = [b' '; 16 * 1024];
            let mut left = body_len - prefix.len();
            while left > 0 {
                let n = left.min(chunk.len());
                if stream.write_all(&chunk[..n]).is_err() {
                    break;
                }
                left -= n;
            }
            let _ = stream.flush();
            // Crucially, do NOT drop the connection here. The response body is
            // one byte longer than the client will read, so closing first
            // leaves an unread byte queued and the kernel answers the client's
            // next read with RST - which ureq reports as a read error instead
            // of the capped body this test is measuring. Holding the socket
            // open until the client closes keeps the byte in flight so the
            // read ends cleanly at the ceiling on every platform.
            let mut drain = [0u8; 16 * 1024];
            while let Ok(n) = stream.read(&mut drain) {
                if n == 0 {
                    break;
                }
            }
        });

        let mut cfg = crate::config::test_config();
        cfg.protocol = crate::config::Protocol::Rest;
        cfg.model = "whisper-1".into();
        cfg.base_url = format!("http://127.0.0.1:{port}");
        cfg.keywords.clear();
        cfg.strip_fillers = false;

        // Drive the same seam `transcribe` drives, so the body length the
        // transport actually buffered is observable: an uncapped read would
        // return `body_len`, a capped one exactly the ceiling.
        let url = endpoint_url(&cfg.base_url).unwrap();
        let response = crate::platform::http::NativeTransport
            .post(&url, Some("Bearer tok"), "multipart/form-data; boundary=x", b"x")
            .expect("the request itself succeeds");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.body.len() as u64,
            crate::platform::http::MAX_TRANSCRIPT_BYTES,
            "the transcript path must stop at its own ceiling"
        );
        let _ = server.join();
    }
}
