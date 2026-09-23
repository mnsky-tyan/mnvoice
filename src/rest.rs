// Generic WinHTTP-based REST client for OpenAI-compatible speech-to-text endpoints.
// Compatible with any standard audio/transcriptions endpoint (self-hosted Whisper, Groq, OpenAI, etc.).
// Native TLS, system cert store, respects Windows system proxy settings.

use windows::Win32::Networking::WinHttp::*;
use windows::core::{w, PCWSTR};

use crate::config::Config;

const BOUNDARY: &str = "mnvoiceboundary9f2a";
const WINHTTP_ADDREQUEST_HEADER_FLAG: u32 = 0x2000_0000; // add or replace
const WINHTTP_QUERY_STATUS: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wptr(v: &[u16]) -> PCWSTR {
    PCWSTR(v.as_ptr())
}

/// Transcribe a WAV clip using an OpenAI-compatible REST endpoint. Returns plain text.
pub fn transcribe(cfg: &Config, wav: &[u8]) -> Result<String, String> {
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
        WinHttpSetTimeouts(session, 0, 10_000, 30_000, 30_000)
            .map_err(|e| format!("set timeouts ({e})"))?;

        let (host, port, secure, base_path) = parse_base_url(&cfg.base_url)?;
        let host_w = wide(&host);
        let connect = WinHttpConnect(session, wptr(&host_w), port, 0);
        if connect.is_null() {
            let _ = WinHttpCloseHandle(session);
            return Err(format!("cannot connect to {host}"));
        }

        let endpoint_path = if !base_path.is_empty() {
            base_path
        } else if host.contains("groq.com") {
            "/openai/v1/audio/transcriptions".to_string()
        } else {
            "/v1/audio/transcriptions".to_string()
        };

        let headers_str = format!(
            "Authorization: Bearer {}\r\nContent-Type: multipart/form-data; boundary={}\r\n",
            cfg.api_key, BOUNDARY
        );
        let body = multipart_body(cfg, wav);

        let path_w = wide(&endpoint_path);
        let request = WinHttpOpenRequest(
            connect,
            w!("POST"),
            wptr(&path_w),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) },
        );
        if request.is_null() {
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err("cannot create HTTP request".into());
        }

        let headers_w = wide(&headers_str);

        let result = (|| {
            WinHttpAddRequestHeaders(
                request,
                &headers_w[..headers_w.len() - 1],
                WINHTTP_ADDREQUEST_HEADER_FLAG,
            )?;
            WinHttpSendRequest(
                request,
                None,
                Some(body.as_ptr() as *const std::ffi::c_void),
                body.len() as u32,
                body.len() as u32,
                0,
            )?;
            WinHttpReceiveResponse(request, std::ptr::null_mut())?;
            Ok::<(), windows::core::Error>(())
        })();
        if let Err(e) = result {
            let _ = WinHttpCloseHandle(request);
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err(format!("request failed ({e})"));
        }

        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let mut index = 0u32;
        let _ = WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut std::ffi::c_void),
            &mut len,
            &mut index,
        );

        let response = read_all(request);
        let _ = WinHttpCloseHandle(request);
        let _ = WinHttpCloseHandle(connect);
        let _ = WinHttpCloseHandle(session);

        if status != 200 {
            let preview: String = String::from_utf8_lossy(&response).chars().take(200).collect();
            return Err(format!("ASR endpoint returned HTTP {status}: {preview}"));
        }

        let raw_text = String::from_utf8_lossy(&response);
        let parsed = parse_json_transcript(&raw_text).unwrap_or_else(|| raw_text.trim().to_string());
        Ok(parsed.trim().to_string())
    }
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

fn read_all(request: *mut std::ffi::c_void) -> Vec<u8> {
    unsafe {
        let mut out = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let mut read = 0u32;
            let ok = WinHttpReadData(
                request,
                chunk.as_mut_ptr() as *mut std::ffi::c_void,
                chunk.len() as u32,
                &mut read,
            );
            if ok.is_err() || read == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..read as usize]);
        }
        out
    }
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
/// Used on the REST path, where no provider has a native filler_words parameter,
/// and on the streaming path as a safety net for providers that ignore it.
pub fn strip_disfluencies(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| !is_disfluency(w))
        .collect::<Vec<_>>()
        .join(" ")
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
    fn test_strip_disfluencies_removes_fillers() {
        assert_eq!(strip_disfluencies("so um this is uh the plan"), "so this is the plan");
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
