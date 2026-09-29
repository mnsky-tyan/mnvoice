// REST client for OpenAI-compatible speech-to-text endpoints.
// Compatible with any standard audio/transcriptions endpoint (self-hosted Whisper, Groq, OpenAI, etc.).
// Requests go through the platform transport seam: WinHTTP with the system cert store and
// proxy settings on Windows, ureq with rustls (bundled roots) or native-tls elsewhere.

use crate::config::Config;
use crate::platform::http::Transport;

const BOUNDARY: &str = "mnvoiceboundary9f2a";

/// Transcribe a WAV clip using an OpenAI-compatible REST endpoint. Returns plain text.
pub fn transcribe(cfg: &Config, wav: &[u8]) -> Result<String, String> {
    let url = endpoint_url(&cfg.base_url)?;
    let body = multipart_body(cfg, wav);
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");

    let response = crate::platform::http::NativeTransport
        .post(&url, Some(&format!("Bearer {}", cfg.api_key)), &content_type, &body)?;

    if response.status != 200 {
        let preview: String =
            String::from_utf8_lossy(&response.body).chars().take(200).collect();
        return Err(format!(
            "ASR endpoint returned HTTP {}: {preview}",
            response.status
        ));
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
                if after_colon.starts_with('"') {
                    let s = &after_colon[1..];
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
/// "you know" and "i mean" - all common real speech. An over-eager list that
/// eats meaningful words is far worse than a missed filler.
const DISFLUENCIES: &[&str] = &[
    "uh", "uhh", "uh-huh", "uhh-huh", "um", "umm", "umm-hmm", "erm", "errm", "hmm", "hm", "mm",
    "mmm", "mm-hmm", "mhm", "uh-hum",
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
pub fn rest_typing(raw: &str, strip_fillers: bool, trailing_space: bool) -> (String, bool) {
    let text = if strip_fillers {
        strip_disfluencies(raw.trim())
    } else {
        raw.trim().to_string()
    };
    let space = trailing_space && !text.is_empty();
    (text, space)
}

pub fn parse_base_url(url: &str) -> Result<(String, u16, bool, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("bad BASE_URL: {url}"))?;
    let secure = scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("wss");
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{}", p.trim_start_matches('/'))),
        None => (rest, String::new()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("bad port in BASE_URL: {url}"))?,
        ),
        _ => (authority.to_string(), if secure { 443 } else { 80 }),
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
        for w in ["uh", "uhh", "um", "umm", "erm", "errm", "hmm", "hm", "mm", "mhm", "Mm,", "HMM."] {
            assert!(is_disfluency(w), "{w} should be a filler");
        }
        // Ambiguous tokens must NOT match - eating these would corrupt real speech.
        for w in ["like", "er", "very", "so", "the", "you", "mean"] {
            assert!(!is_disfluency(w), "{w} should NOT be a filler");
        }
    }
}
