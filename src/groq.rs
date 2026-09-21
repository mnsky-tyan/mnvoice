// WinHTTP-based transcription for Deepgram (Nova-3/Whisper) and Groq (Whisper).
// Native TLS, system cert store, respects Windows system proxy settings.

use windows::Win32::Networking::WinHttp::*;
use windows::core::{w, PCWSTR};

use crate::config::{Config, Provider};

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

/// Transcribe a WAV clip using the configured provider. Returns plain text.
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

        let (host, port, secure) = parse_base_url(&cfg.base_url)?;
        let host_w = wide(&host);
        let connect = WinHttpConnect(session, wptr(&host_w), port, 0);
        if connect.is_null() {
            let _ = WinHttpCloseHandle(session);
            return Err(format!("cannot connect to {host}"));
        }

        let (endpoint_str, headers_str, body) = match cfg.provider {
            Provider::Deepgram => {
                let mut path = format!("/v1/listen?model={}&smart_format=true", cfg.model);
                if !cfg.language.is_empty() {
                    if cfg.language.eq_ignore_ascii_case("auto") {
                        path.push_str("&detect_language=true");
                    } else {
                        path.push_str("&language=");
                        path.push_str(&cfg.language);
                    }
                }
                let hdrs = format!(
                    "Authorization: Token {}\r\nContent-Type: audio/wav\r\n",
                    cfg.api_key
                );
                (path, hdrs, wav.to_vec())
            }
            Provider::Groq => {
                let path = "/openai/v1/audio/transcriptions".to_string();
                let hdrs = format!(
                    "Authorization: Bearer {}\r\nContent-Type: multipart/form-data; boundary={}\r\n",
                    cfg.api_key, BOUNDARY
                );
                let b = groq_multipart_body(cfg, wav);
                (path, hdrs, b)
            }
        };

        let endpoint_w = wide(&endpoint_str);
        let request = WinHttpOpenRequest(
            connect,
            w!("POST"),
            wptr(&endpoint_w),
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
            // dwHeadersLength slice must not include the trailing NUL.
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

        let provider_name = match cfg.provider {
            Provider::Deepgram => "Deepgram",
            Provider::Groq => "Groq",
        };

        if status != 200 {
            let preview: String = String::from_utf8_lossy(&response).chars().take(200).collect();
            return Err(format!("{provider_name} returned HTTP {status}: {preview}"));
        }

        let raw_text = String::from_utf8_lossy(&response);
        match cfg.provider {
            Provider::Deepgram => {
                let parsed = parse_deepgram_transcript(&raw_text)
                    .unwrap_or_default();
                Ok(parsed.trim().to_string())
            }
            Provider::Groq => Ok(raw_text.trim().to_string()),
        }
    }
}

pub fn parse_deepgram_transcript(json: &str) -> Option<String> {
    let key = "\"transcript\"";
    let key_pos = json.find(key)?;
    let after_key = &json[key_pos + key.len()..];
    let colon_pos = after_key.find(':')?;
    let after_colon = after_key[colon_pos + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
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

fn parse_base_url(url: &str) -> Result<(String, u16, bool), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("bad BASE_URL: {url}"))?;
    let secure = scheme.eq_ignore_ascii_case("https");
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("bad port in BASE_URL: {url}"))?,
        ),
        _ => (authority.to_string(), if secure { 443 } else { 80 }),
    };
    Ok((host, port, secure))
}

fn groq_multipart_body(cfg: &Config, wav: &[u8]) -> Vec<u8> {
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
    fn test_parse_deepgram_basic() {
        let json = r#"{"metadata":{},"results":{"channels":[{"alternatives":[{"transcript":"hello world","confidence":0.99}]}]}}"#;
        assert_eq!(parse_deepgram_transcript(json), Some("hello world".to_string()));
    }

    #[test]
    fn test_parse_deepgram_escapes() {
        let json = r#"{"results":{"channels":[{"alternatives":[{"transcript":"he said \"hello\"\nand left"}]}]}}"#;
        assert_eq!(parse_deepgram_transcript(json), Some("he said \"hello\"\nand left".to_string()));
    }

    #[test]
    fn test_parse_deepgram_empty() {
        let json = r#"{"results":{"channels":[{"alternatives":[{"transcript":""}]}]}}"#;
        assert_eq!(parse_deepgram_transcript(json), Some("".to_string()));
    }
}
