// REST client for OpenAI-compatible speech-to-text endpoints.
// Compatible with any standard audio/transcriptions endpoint (self-hosted Whisper, Groq, OpenAI, etc.).
// Requests go through the platform transport seam: WinHTTP with the system cert store and
// proxy settings on Windows, ureq with rustls (bundled roots) or native-tls elsewhere.

use crate::config::{auth_value, Config};
use crate::platform::http::{preview, Transport};

const BOUNDARY: &str = "mnvoiceboundary9f2a";

/// Transcribe a WAV clip using an OpenAI-compatible REST endpoint. Returns plain text.
///
/// Reached only through `dictate_rest` below, so this is module-private: the
/// crate is a binary and the streaming path has its own `run_stream`.
fn transcribe(cfg: &Config, wav: &[u8]) -> Result<String, String> {
    let url = endpoint_url(&cfg.base_url)?;
    let body = multipart_body(cfg, wav)?;
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
    fallback_transcript(&raw_text)
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

/// Turns a 200 body into the transcript, or refuses it.
///
/// The order of the checks is the guard. The BOM comes off first, an empty body
/// is refused, the JSON key scan runs only on a body that opens as a JSON
/// object, the refusal shapes are checked, and only then is the whole body
/// taken as the transcript.
///
/// The fallback is load-bearing and cannot be dropped: a provider that honours
/// `response_format=text` returns the transcript as a bare body, with no JSON
/// envelope at all, and that body has to be taken verbatim. But a 200 is not
/// proof of a transcript - a proxy's HTML error page, or a JSON object whose
/// `error` member is the whole payload, is also a 200 body, and typing either
/// into the user's focused window is exactly the failure this function exists
/// to prevent. So the fallback is taken only when the body does not look like
/// a refusal.
///
/// The shapes checked are the ones a refusal actually arrives in, and they are
/// checked in the body rather than by a content-type header, because a proxy
/// that returns an error page has no obligation to label it one.
fn fallback_transcript(raw: &str) -> Result<String, String> {
    // A byte-order mark is not part of any transcript: str::trim does not
    // strip U+FEFF (the same property config.rs documents), and a BOM left
    // in place evades every clause below - a BOM'd error page or JSON
    // error object would be typed into the user's window as the result.
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("the ASR endpoint returned an empty body".to_string());
    }
    // The JSON key scan is a heuristic, so it runs only on a body that
    // opens as a JSON object. Over an arbitrary text body the scan let a
    // dictated sentence containing "text": "..." be replaced by the one
    // quoted word, and an HTML page's type="text" attribute satisfy the
    // parser before the refusal guard ever ran.
    if trimmed.starts_with('{') {
        if let Some(transcript) = parse_json_transcript(trimmed) {
            return Ok(transcript);
        }
    }
    if looks_like_a_refusal(trimmed) {
        return Err(format!(
            "the ASR endpoint returned a refusal, not a transcript: {}",
            preview(trimmed)
        ));
    }
    Ok(trimmed.to_string())
}

/// Whether a 200 body is a provider's error payload rather than a transcript.
///
/// Kept deliberately narrow. A false positive would reject a real transcript,
/// which loses a dictation the user already spoke; a false negative types an
/// error page, which is what the guard is for. So only the shapes that can only
/// be an error are listed - a JSON object with an `error` member, an HTML
/// document, and an XML one - and anything ambiguous is left to the fallback.
///
/// The JSON clauses are read only when the body opens as a JSON object, for
/// that same reason: a provider honouring `response_format=text` returns the
/// words the user dictated, and a dictated bug report or commit body happily
/// contains `"error"`. Quoted-key shapes found anywhere in a bare transcript
/// would throw away a dictation that was actually heard.
///
/// The `"message"` value may be separated from its key and colon by ASCII
/// whitespace, because providers pretty-print their JSON (`{"message": "..."}`
/// is as common as the compact spelling).
/// ASCII-case-insensitive "does `haystack` begin with `needle`", comparing
/// bytes without lowercasing (or copying) either side. Both sides are folded,
/// so neither argument has to be pre-lowercased for the comparison to hold.
fn starts_with_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len()
        && haystack
            .bytes()
            .zip(needle.bytes())
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
}

fn looks_like_a_refusal(body: &str) -> bool {
    let start = body.trim_start();
    // Only the marker prefixes are examined, never a copy of the whole body.
    // This runs on every REST response, and the body is bounded at 8 MiB
    // (`MAX_TRANSCRIPT_BYTES`), so the old `start.to_ascii_lowercase()`
    // allocated and lowercased up to 8 MiB to answer a question about its
    // first 14 bytes. Behaviour is identical: the same three markers, matched
    // ASCII-case-insensitively, against a string that no longer pays for a
    // full copy. Byte-slicing is avoided on purpose - clamping to a fixed
    // length would panic on a multibyte character at that boundary.
    if starts_with_ignore_case(start, "<!doctype html")
        || starts_with_ignore_case(start, "<html")
        || starts_with_ignore_case(start, "<?xml")
    {
        return true;
    }
    if !start.starts_with('{') {
        return false;
    }
    if start.contains("\"error\"") {
        return true;
    }
    const MESSAGE_KEY: &str = "\"message\"";
    let Some(key_pos) = start.find(MESSAGE_KEY) else {
        return false;
    };
    start[key_pos + MESSAGE_KEY.len()..]
        .trim_start_matches(|c: char| c.is_ascii_whitespace())
        .strip_prefix(':')
        .is_some_and(|after_colon| {
            after_colon
                .trim_start_matches(|c: char| c.is_ascii_whitespace())
                .starts_with('"')
        })
}

/// The transcript a provider's JSON body carries, or None when there is none.
///
/// Shared by both paths: the REST fallback and the streaming frame parser
/// (`stream::parse_stream_json`) go through here, which is why the unescape
/// rules and the "is this a transcript-shaped object" test live once.
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
fn strip_disfluencies(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| !is_disfluency(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// True when a trailing space should be appended: trailing space is enabled
/// and the typed text is non-empty. Single-sourced so REST and streaming
/// agree by construction.
pub fn trailing_space_due(trailing_space: bool, text: &str) -> bool {
    trailing_space && !text.is_empty()
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
///
/// (The paragraph above used to sit on `trailing_space_due` - two functions'
/// docs collided onto the wrong item, and this one had none.)
/// What the REST path types, and whether a trailing space follows it.
///
/// Module-private: `dictate_rest` is its only production caller, and the API
/// deliberately cannot be called twice by the same caller (the second call
/// would re-type), which is what the `bool` half of the return exists to serve.
/// The shared questions it composes - `strip_disfluencies` locally, and
/// `trailing_space_due` for the streaming path too - are the parts with more
/// than one user.
fn rest_typing(raw: &str, strip_fillers: bool, trailing_space: bool) -> (String, bool) {
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
    // `trailing` is consumed for its side effect above and dropped from the
    // return, so callers must not re-derive it from the transcript - and cannot
    // mistake the transcript for something that says whether it was typed. A
    // caller wanting to know has to call `rest_typing` again, which is the only
    // honest answer.
    Ok(text)
}

/// Split a base URL into `(host, port, secure, path)`.
///
/// The host is returned WITHOUT IPv6 brackets on purpose - see
/// [`url_authority`] for the rule that puts them back for a URL. A scheme is
/// allow-listed rather than assumed, so a mistyped one is an error instead of
/// an unencrypted upload.
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

/// The URL-authority spelling of a host that `parse_base_url` returns.
///
/// `parse_base_url` deliberately hands back an IPv6 literal WITHOUT brackets,
/// because `WinHttpConnect` wants the bare address - that dependency lives at
/// `platform::windows_impl`'s connect call. Anything building a URL must put
/// the brackets back, because a URI authority requires them. The rule used to
/// be re-derived from the shape of the string inside `stream::listen_url` (a
/// `host.contains(':')` test), which is the same knowledge written down twice:
/// correct today, and free to drift tomorrow. This is the one place it is
/// written.
///
/// Borrows in the ordinary case (a DNS host or a bare IPv4 needs no brackets);
/// allocates only for the IPv6 literal.
pub(crate) fn url_authority(host: &str) -> std::borrow::Cow<'_, str> {
    if host.contains(':') {
        std::borrow::Cow::Owned(format!("[{host}]"))
    } else {
        std::borrow::Cow::Borrowed(host)
    }
}

/// Refuse a value that would break out of its multipart part.
///
/// CR or LF ends the part's headers early and makes the rest of the value parse
/// as new headers; the boundary itself ends the part and lets the caller supply
/// the closing delimiter. Both are refused rather than escaped - see
/// `multipart_body` for why.
fn reject_part_breaker(label: &str, value: &str) -> Result<(), String> {
    if value.contains('\r') || value.contains('\n') {
        return Err(format!(
            "{label} must not contain a line break (it would break the upload's formatting)"
        ));
    }
    if value.contains(BOUNDARY) {
        return Err(format!(
            "{label} must not contain the multipart boundary {BOUNDARY}"
        ));
    }
    Ok(())
}

fn multipart_body(cfg: &Config, wav: &[u8]) -> Result<Vec<u8>, String> {
    // Three user-supplied values are written into the body verbatim, and the
    // boundary is a fixed constant with no uniqueness check. A value containing
    // CR or LF terminates its part early and the following bytes are parsed as
    // new part headers; a value containing the boundary itself ends the part
    // and lets the closing `--BOUNDARY--` be the caller's. Either way the
    // `file` part can be truncated, and the POST then carries a Content-Length
    // for a body whose final boundary is not the one this side sent.
    //
    // Refused rather than escaped: a model name, a language code or a keyterm
    // list has no legitimate reason to contain a newline or this boundary, so
    // "MODEL must not contain a newline" is a more useful message than a
    // percent-encoded model name the provider will reject anyway. The streaming
    // path percent-encodes the same two fields (stream.rs listen_url), which is
    // what made the asymmetry worth closing: one MODEL value was inert on the
    // websocket query and structurally significant here.
    for (label, value) in [
        ("MODEL", cfg.model.as_str()),
        ("LANGUAGE", cfg.language.as_str()),
    ] {
        reject_part_breaker(label, value)?;
    }
    for kw in &cfg.keywords {
        reject_part_breaker("KEYWORDS", kw)?;
    }

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
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback has to survive: a provider honouring `response_format=text`
    /// returns a bare transcript with no JSON envelope, and that body is the
    /// answer. Without this the guard could grow a case that eats it.
    #[test]
    fn a_bare_text_transcript_is_taken_verbatim() {
        assert_eq!(
            fallback_transcript("  hello there  "),
            Ok("hello there".to_string())
        );
    }

    /// The refusal guard must not panic or misclassify on a body whose leading
    /// bytes are multibyte. The old shape copied the entire body to lowercase
    /// it; the replacement compares in place, and a naive byte-slice clamp to
    /// the marker length would panic mid-character. A transcript that happens
    /// to start with a non-ASCII word is the real-world case.
    #[test]
    fn a_multibyte_transcript_is_not_a_refusal_and_does_not_panic() {
        let out = fallback_transcript("\u{00e9}coute: a French transcript");
        assert_eq!(out.as_deref(), Ok("\u{00e9}coute: a French transcript"));
        // Case folding still fires on the markers however they are cased.
        for marker in [
            "<!doctype html",
            "<!DOCType HTML",
            "<HTML lang=en>",
            "<?xml version=\"1.0\">",
        ] {
            assert!(looks_like_a_refusal(marker), "{marker:?} is a refusal page");
        }
        // ...and does not fire on a body merely beginning with `<h`, the
        // shorter marker's own prefix.
        assert!(!looks_like_a_refusal("<h1>not a marker</h1>"));
    }

    /// The case-insensitive prefix comparison folds BOTH sides, so a caller
    /// passing a mixed-case needle is not silently mis-handled.
    #[test]
    fn the_prefix_check_folds_both_sides() {
        assert!(starts_with_ignore_case("<!DOCTYPE html>", "<!doctype html"));
        assert!(starts_with_ignore_case("<!doctype html>", "<!DOCTYPE HTML"));
        assert!(!starts_with_ignore_case("<htm", "<html"));
        assert!(starts_with_ignore_case("", ""));
        // Only ASCII letters fold: an 't'/'T' difference matches, because the
        // comparison is bytewise-with-ASCII-fold, while a real accented
        // difference (the precomposed E vs e, two different UTF-8 sequences)
        // does not. The refusal markers are pure ASCII, so this is exactly the
        // behaviour the caller needs and nothing more.
        assert!(starts_with_ignore_case("\u{00e9}tale", "\u{00e9}Tale"));
        assert!(!starts_with_ignore_case("\u{00c9}tale", "\u{00e9}tale"));
    }

    /// The adversarial half: a 200 whose body is not a transcript must be
    /// refused rather than typed into whatever window the user was in.
    ///
    /// Both shapes arrive from real providers. The JSON one is a provider that
    /// answers 200 with an error object; the HTML one is a proxy or a captive
    /// portal's page.
    #[test]
    fn a_refusal_body_is_reported_rather_than_typed() {
        let json = r#"{"error":{"message":"insufficient quota","type":"quota"}}"#;
        let out = fallback_transcript(json);
        assert!(out.is_err(), "a JSON error must not be typed: {out:?}");

        let html = "<!DOCTYPE html><html><body>502 Bad Gateway</body></html>";
        let out = fallback_transcript(html);
        assert!(
            out.is_err(),
            "an HTML error page must not be typed: {out:?}"
        );

        let out = fallback_transcript("   ");
        assert!(out.is_err(), "an empty body must not be typed: {out:?}");
    }

    /// The guard must not reject the envelope it is meant to pass through: a
    /// JSON transcript still comes out of the parser, not the fallback.
    #[test]
    fn a_json_transcript_still_comes_out_of_the_parser() {
        let out = fallback_transcript(r#"{"text":"half a sentence"}"#);
        assert_eq!(out, Ok("half a sentence".to_string()));
    }

    /// The false positive this guard cannot afford: a bare transcript that
    /// mentions an error is the user's own words, and refusing it loses a
    /// dictation they already spoke. So the quoted-key shapes are only read in
    /// a body that opens as a JSON object.
    #[test]
    fn a_transcript_that_names_an_error_is_still_typed() {
        for body in [
            "the error was in the parser",
            "the log prints \"error\" and \"message\":\"failed to start\"",
        ] {
            assert_eq!(
                fallback_transcript(body),
                Ok(body.to_string()),
                "a bare transcript that mentions an error must not be refused"
            );
        }
    }

    /// The JSON key scan is a heuristic over the whole body, so it runs only
    /// when the body opens as a JSON object. A provider honouring
    /// `response_format=text` returns the dictated words bare, and a sentence
    /// containing `"text": "..."` was collapsed to the one quoted word by the
    /// scan before the refusal guard ever ran - a whole dictation replaced by
    /// one word, silently.
    #[test]
    fn a_bare_transcript_containing_a_json_key_is_taken_whole() {
        let body = r#"set the field "text": "name" in the schema"#;
        assert_eq!(
            fallback_transcript(body),
            Ok(body.to_string()),
            "a dictated sentence must not be replaced by a quoted word inside it"
        );
    }

    /// The same scan also mined an HTML page: `type="text"` satisfies it, and
    /// the first quoted string after any later colon came back as a transcript.
    /// The page is a refusal, not a dictation.
    #[test]
    fn an_html_page_that_satisfies_the_json_key_scan_is_refused() {
        let page = r#"<!DOCTYPE html><html><body><input type="text" name="q"><p>error: "503 Service Unavailable"</p></body></html>"#;
        let out = fallback_transcript(page);
        assert!(out.is_err(), "an HTML page must not be typed: {out:?}");
    }

    /// A byte-order mark is not part of any transcript, and it also defeats
    /// every anchored check: `str::trim` does not strip U+FEFF, so a BOM'd
    /// error page or JSON error object passed the refusal guard and was typed
    /// into the user's window with the mark in front.
    #[test]
    fn a_bom_does_not_smuggle_a_refusal_past_the_guard() {
        let json = "\u{feff}{\"error\":{\"message\":\"insufficient quota\"}}";
        let out = fallback_transcript(json);
        assert!(
            out.is_err(),
            "a BOM'd JSON error must not be typed: {out:?}"
        );

        let html = "\u{feff}<!DOCTYPE html><html><body>502 Bad Gateway</body></html>";
        let out = fallback_transcript(html);
        assert!(out.is_err(), "a BOM'd HTML page must not be typed: {out:?}");

        // A body that is nothing but a BOM is an empty body, not a transcript.
        let out = fallback_transcript("\u{feff}");
        assert!(out.is_err(), "a lone BOM must not be typed: {out:?}");
    }

    /// The BOM comes off a real transcript too, rather than being typed as an
    /// invisible character in front of the user's words.
    #[test]
    fn a_bom_is_stripped_from_a_real_transcript() {
        assert_eq!(
            fallback_transcript("\u{feff}hello world"),
            Ok("hello world".to_string())
        );
        assert_eq!(
            fallback_transcript("\u{feff}{\"text\":\"hello\"}"),
            Ok("hello".to_string())
        );
    }

    /// The `"message"` key is the other error shape a provider returns, and
    /// providers pretty-print: the exact byte run `"message":"` missed
    /// `{"message": "..."}` and typed the error sentence into the focused
    /// window. The quoted-value requirement is what keeps the clause narrow -
    /// a bare `"message"` mention or a null value is not a refusal.
    #[test]
    fn a_message_key_with_a_quoted_value_is_a_refusal() {
        for body in [
            r#"{"message": "insufficient quota"}"#,
            r#"{"message":"insufficient quota"}"#,
            r#"{"message" : "insufficient quota"}"#,
            "{\"message\":\t\"insufficient quota\"}",
        ] {
            let out = fallback_transcript(body);
            let err = out.expect_err("a message error must not be typed");
            assert!(
                err.starts_with("the ASR endpoint returned a refusal"),
                "{err}"
            );
        }

        let body = r#"{"message": null}"#;
        assert_eq!(
            fallback_transcript(body),
            Ok(body.to_string()),
            "a null message is not a refusal"
        );

        assert_eq!(
            fallback_transcript(r#"{"text": "hello"}"#),
            Ok("hello".to_string()),
            "a JSON transcript still parses"
        );
    }

    /// A refusal names the body it rejected without carrying it: the message
    /// goes to the log and the CLI's stdout, and the body is bounded only by
    /// the transport's 8 MiB.
    #[test]
    fn a_refusal_reports_a_preview_not_the_whole_body() {
        let mut json = String::from(r#"{"error":{"message":"quota"}}"#);
        json.push_str(&"x".repeat(200 * 1024));
        let err = fallback_transcript(&json).expect_err("a JSON error object is a refusal");
        assert!(
            err.len() < 300,
            "the message must stay bounded, got {} bytes: {}",
            err.len(),
            &err[..err.len().min(80)]
        );
        assert!(
            !err.contains(&"x".repeat(300)),
            "the tail of the body must not be carried"
        );
        assert!(
            err.starts_with("the ASR endpoint returned a refusal"),
            "{err}"
        );
    }

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
        let mut cfg = crate::config::test_config();
        cfg.base_url = "https://127.0.0.1:1".to_string();
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

    /// A model, language or keyterm containing a line break or the multipart
    /// boundary is refused rather than written into the body, where it would end
    /// its part early and let the following bytes parse as headers.
    #[test]
    fn a_value_that_would_break_out_of_its_part_is_refused() {
        // Three user-supplied values go into the body verbatim and the boundary
        // is a fixed constant, so a value with a line break in it ends its part
        // early and the bytes after it parse as new part headers - which can
        // truncate the `file` part while the Content-Length still claims the
        // whole body. Refused rather than escaped.
        let base = || {
            let mut c = crate::config::test_config();
            c.model = "whisper-large-v3-turbo".into();
            c.language = "en".into();
            c
        };

        // A line break in any of the three is refused.
        for (label, mutate) in [("model", 0usize), ("language", 1), ("keywords", 2)] {
            let mut cfg = base();
            match mutate {
                0 => cfg.model = "whisper\r\n--mnvoiceboundary9f2a--".into(),
                1 => cfg.language = "en\nContent-Disposition: form-data".into(),
                _ => cfg.keywords = vec!["kubernetes".into(), "dock\r\ner".into()],
            }
            let err =
                multipart_body(&cfg, b"RIFFxxxx").expect_err(&format!("{label} must be refused"));
            assert!(
                err.to_ascii_uppercase()
                    .contains(&label.to_ascii_uppercase()),
                "the message must name the field: {err}"
            );
            assert!(
                err.contains("line break") || err.contains("boundary"),
                "the message must say what was wrong: {err}"
            );
        }

        // The boundary itself is refused even with no line break.
        let mut cfg = base();
        cfg.model = format!("x{}y", BOUNDARY);
        let err = multipart_body(&cfg, b"RIFFxxxx").expect_err("the boundary must be refused");
        assert!(err.contains("boundary"), "{err}");

        // Ordinary values still build, and the body is unchanged for them.
        let cfg = base();
        let body = multipart_body(&cfg, b"RIFFxxxx").expect("ordinary values build");
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(&format!("--{BOUNDARY}\r\n")), "{text}");
        assert!(
            text.contains("name=\"model\"\r\n\r\nwhisper-large-v3-turbo\r\n"),
            "{text}"
        );
        assert!(text.ends_with(&format!("--{BOUNDARY}--\r\n")), "{text}");
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
