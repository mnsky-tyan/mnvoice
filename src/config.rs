// Generic configuration for mnvoice.
// Reads from mnvoice.env next to the executable, or environment variables.
// Supports real-time WebSocket streaming or standard REST speech-to-text endpoints.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Streaming,
    Rest,
}

#[derive(Clone)]
pub struct Config {
    pub protocol: Protocol,
    pub api_key: String,
    pub model: String,
    pub language: String,
    pub base_url: String,
    pub max_seconds: u32,
    pub trailing_space: bool,
    pub keywords: Vec<String>,
    // Orb appearance. Read by the Windows orb renderer; on Linux and macOS
    // the orb is not ported yet, so the fields ride along unparsed-but-stored
    // to keep config files and their round-trip identical everywhere.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub orb_color: (f32, f32, f32),
    #[cfg_attr(not(windows), allow(dead_code))]
    pub orb_fluid_level: f32,
    // Windows-only settings; they ride along for the same round-trip reason.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub hotkey: (u32, u32),
    #[cfg_attr(not(windows), allow(dead_code))]
    pub hotkey_str: String,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub cancel_key: (u32, u32),
    #[cfg_attr(not(windows), allow(dead_code))]
    pub cancel_key_str: String,
    pub vad_silence_ms: u32,
    pub vad_rms_threshold: f64,
    /// Drop disfluencies (uh, um, erm). Streaming uses the provider's native
    /// parameter when one exists; REST filters locally. FILLER_WORDS=0 strips.
    pub strip_fillers: bool,
    // AUTO_UPDATE deliberately has no field here: `auto_update_enabled()` is the
    // single authority, because it must stay readable when load() has failed (a
    // broken API key aborts the parse) and that is exactly when a user is most
    // likely to be stuck on an outdated build. A field copy would be a second
    // value that can disagree with the function it was copied from.
}

pub fn load() -> Result<Config, String> {
    let mut raw = RawFields::new();

    // Check for keywords.txt beside the executable
    //
    // Three filenames, one fixed order, and they ADD to each other rather than
    // overriding: all of them push into the same `raw.keywords` and the
    // de-duplication further down is case-insensitive. So `vocabulary.txt` does
    // not become the keyword list, it extends whatever `keywords.txt` already
    // said, and `words.txt` extends both. That is deliberate - it lets a user
    // keep a private keyterm file beside a shared one - but it is not obvious
    // from the loop, and a user who writes a `vocabulary.txt` expecting it to be
    // their list also sends the words from the other two.
    if let Ok(exe) = std::env::current_exe() {
        for filename in ["keywords.txt", "vocabulary.txt", "words.txt"] {
            let path = exe.with_file_name(filename);
            if let Ok(text) = std::fs::read_to_string(&path) {
                parse_keywords_text(&text, &mut raw.keywords);
            }
        }
    }

    // Lowest priority: mnvoice.env beside the executable.
    if let Ok(exe) = std::env::current_exe() {
        let path = exe.with_file_name("mnvoice.env");
        if let Ok(text) = std::fs::read_to_string(&path) {
            apply_env_text(&text, &mut raw);
        }
    }

    // Highest priority: real environment variables. Both sources read the same
    // table, so a name one accepts the other cannot silently ignore.
    for (names, field) in FIELDS {
        for name in *names {
            if let Ok(v) = std::env::var(name) {
                raw.set(*field, "", &v, false);
                break;
            }
        }
    }

    // Determine protocol: streaming vs rest
    derive(raw)
}

/// The derivation both sources feed: protocol defaults, the key requirement,
/// hotkey pairs and the display strings that must name them.
fn derive(raw: RawFields) -> Result<Config, String> {
    let protocol = match raw.protocol_str.to_lowercase().as_str() {
        "rest" | "http" | "batch" | "groq" | "openai" => Protocol::Rest,
        "streaming" | "stream" | "websocket" | "ws" | "deepgram" => Protocol::Streaming,
        _ => {
            // The host is compared at a domain boundary, not by substring:
            // `raw.base_url.contains("groq.com")` was true for a self-hosted
            // endpoint that merely had the name in its path or a spoofed label
            // in front of it (`https://groq.com.example.net/`,
            // `http://tunnel.internal/mirror/groq.com/v1`), and inference is
            // exactly where that matters - a user pointing at their own Whisper
            // box was silently switched to a provider's API path. The model
            // name stays a substring test on purpose: model identifiers are
            // free-form ("whisper-large-v3-turbo", "groq/whisper-lg3"), so
            // there is no boundary to compare against.
            let host = crate::rest::host_of(&raw.base_url).unwrap_or_default();
            if crate::rest::host_is_domain(&host, "groq.com")
                || crate::rest::host_is_domain(&host, "openai.com")
                || raw.model.contains("whisper")
            {
                Protocol::Rest
            } else {
                Protocol::Streaming
            }
        }
    };

    if raw.api_key.trim().is_empty() {
        return Err("No API key configured. Set API_KEY in mnvoice.env next to the mnvoice binary.".into());
    }
    // Stored trimmed, like the model, base URL and language below. `auth_value`
    // interpolates this string into the Authorization header verbatim, so an
    // environment value like `API_KEY='tok '` used to go out as `Token tok `
    // and the provider rejected it with no local hint about the space. The
    // empty check above already trimmed; keeping the trimmed value is what
    // makes the check and the wire agree.
    let api_key = raw.api_key.trim().to_string();

    let mut model = raw.model.trim().to_string();
    if model.is_empty() {
        model = match protocol {
            Protocol::Streaming => "nova-3".into(),
            Protocol::Rest => "whisper-large-v3-turbo".into(),
        };
    }
    // Same rule the API key uses above. The file path is already trimmed by
    // `parse`, but an environment variable is not, and `load` hands it straight
    // to `raw.set` - so `MODEL=" "` used to produce a Config whose model was
    // one space and no error, which `listen_url` then spliced verbatim into the
    // query. "Empty" now means one thing for every field.
    let mut base_url = raw.base_url.trim().to_string();
    if base_url.is_empty() {
        base_url = match protocol {
            Protocol::Streaming => "https://api.deepgram.com".into(),
            Protocol::Rest => "https://api.groq.com".into(),
        };
    }
    let mut language = raw.language.trim().to_string();
    if language.is_empty() {
        language = "en".into();
    }

    let orb_color = parse_color(&raw.orb_color_str);
    let orb_fluid_level = parse_fluid_level(&raw.orb_fluid_str);

    // The displayed spelling must name the pair that was actually registered:
    // an unparseable HOTKEY falls back to the default below, so echoing
    // "Foobar" back into the tray tip and the log would advertise a key that
    // does nothing. "none" disables the toggle hotkey outright - the same
    // spelling, and the same zero-pair sentinel, that CANCEL_KEY=none uses
    // below, so one word cannot mean "default" for one key and "off" for the
    // other. Without a toggle hotkey the tray menu still starts and stops a
    // dictation.
    //
    // Both keys go through `hotkey_with_display`, which owns that contract
    // once: it used to be spelled inline here and again inside
    // `cancel_key_with_display`, and the two copies had already drifted on what
    // "none" displays (this one normalized to lowercase, that one kept the
    // user's case). One helper is what keeps the pair and the advertised
    // spelling in step for whichever key it is.
    let hotkey_input = raw.hotkey_str.trim().to_string();
    let (hotkey, hotkey_actual_str) = hotkey_with_display(
        &hotkey_input,
        (DEFAULT_HOTKEY_MOD, DEFAULT_HOTKEY_VK),
        DEFAULT_HOTKEY_STR,
    );

    let cancel_input = raw.cancel_key_str.trim().to_string();
    // "none" disables the cancel key outright: a zero virtual key is what the
    // Windows registration reads as "register nothing". The pair and the
    // spelling shown are decided together so the tray/log cannot name a key
    // different from the one registered.
    let (cancel_key, cancel_actual_str) = cancel_key_with_display(&cancel_input);

    // FILLER_WORDS=0 (default) strips disfluencies; =1 keeps them verbatim.
    // Empty or unset strips, so a typo can never silently re-enable fillers.
    let strip_fillers = !matches!(
        raw.filler_words_str.trim().to_lowercase().as_str(),
        "1" | "true" | "on" | "yes" | "keep"
    );

    // The three numeric fields, parsed here rather than in `set` so one bad
    // spelling is a config error naming its own key instead of a silent
    // default. (A BLANK numeric - `MAX_SECONDS=` with nothing after the `=` -
    // is that loud case, while the string fields treat blank as unset and fall
    // back to their defaults. The asymmetry is deliberate: a number with no
    // value is a truncated line, and a comment-out is spelled by deleting the
    // whole line, so a bare `KEY=` for a number is worth naming.) Ranges: a zero MAX_SECONDS is refused because its only effect
    // is a deadline that already passed - the session returns Ok(()) with no
    // audio and no transcript, which no reading of "120" could intend. The two
    // VAD bounds keep zero deliberately: `silence_expired` documents 0 as
    // "stop on the first silent tick", and an RMS threshold of 0 counts any
    // NONZERO level as speech - so it is a usable "gate on anything but pure
    // digital silence" setting, not a switch that disables the detector: the
    // comparison is `rms > threshold`, so a chunk whose RMS is exactly 0.0 is
    // still silence (measured: `a_chunk_exactly_at_the_threshold_counts_as_
    // silence`), and the 10 s no-speech limit still ends a silent session.
    // Both are coherent requests, not typos.
    let max_seconds = parse_positive_num("MAX_SECONDS", &raw.max_seconds)?;
    let vad_silence_ms = parse_u32_num("VAD_SILENCE_MS", &raw.vad_silence_ms)?;
    let vad_rms_threshold = parse_rms_num("VAD_RMS_THRESHOLD", &raw.vad_rms_threshold)?;

    Ok(Config {
        protocol,
        api_key,
        model,
        language,
        base_url,
        max_seconds,
        trailing_space: raw.trailing_space,
        keywords: raw.keywords,
        orb_color,
        orb_fluid_level,
        hotkey,
        hotkey_str: hotkey_actual_str,
        cancel_key,
        cancel_key_str: cancel_actual_str,
        vad_silence_ms,
        vad_rms_threshold,
        strip_fillers,
    })
}

/// The Authorization header value for a config, honouring either scheme.
///
/// `default_scheme` is what this transport uses when the key carries no scheme
/// of its own: Deepgram wants `Token`, OpenAI-compatible endpoints want
/// `Bearer`. Those defaults differ per transport and are NOT interchangeable -
/// they are what the providers actually accept - so each caller passes its own
/// and neither is silently changed.
///
/// What is shared is the pass-through: both transports are configured by one
/// `API_KEY`, so a key the user already spelled `Token ...` must not be
/// double-prefixed. A hardcoded `Bearer {}` on the REST path turned exactly
/// that key into `Bearer Token xyz` and it was rejected there, while the same
/// key worked for streaming.
pub fn auth_value(api_key: &str, default_scheme: &str) -> String {
    if api_key.starts_with("Token ") || api_key.starts_with("Bearer ") {
        api_key.to_string()
    } else {
        format!("{default_scheme} {api_key}")
    }
}

/// Whether `AUTO_UPDATE` asks for automatic installs, read without depending on
/// the rest of the config being valid. `load()` fails outright on a missing API
/// key, so this is what keeps a typo'd config from also silencing updates.
// Read only by the Windows updater (`#[cfg(windows)] mod update`); the
// Unix builds store and parse the value but have no auto-install path yet,
// so the functions ride along unused. Same house pattern as the
// Windows-only Config fields above.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn auto_update_enabled() -> bool {
    let mut raw = auto_update_from_file();
    // The real environment wins over the file beside the exe, matching load().
    if let Ok(v) = std::env::var("AUTO_UPDATE") {
        raw = Some(v);
    }
    // `matches!` over an Option, the same idiom `parse_auto_update` and
    // `derive`'s strip_fillers use for the same "does this value arm the
    // feature" question - the file's convention everywhere else. The
    // `map(..).unwrap_or(false)` spelling made one reader stop and check
    // whether the two forms differed.
    raw.as_deref().is_some_and(parse_auto_update)
}

/// The `AUTO_UPDATE` value from `mnvoice.env` beside the executable, if it names
/// one. Last entry wins so a later correction overrides an earlier one.
// Read only by the Windows updater (`#[cfg(windows)] mod update`); the
// Unix builds store and parse the value but have no auto-install path yet,
// so the functions ride along unused. Same house pattern as the
// Windows-only Config fields above.
#[cfg_attr(not(windows), allow(dead_code))]
fn auto_update_from_file() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let text = std::fs::read_to_string(exe.with_file_name("mnvoice.env")).ok()?;
    parse_auto_update_text(&text)
}

/// The `AUTO_UPDATE` value out of an env file body, or `None` if it never names
/// one. `Some("")` means the key was present but empty, which is "off".
///
/// Matched case-insensitively, unlike the `FIELDS` lookup that reads every other
/// key, which is case-sensitive. The asymmetry is deliberate - `AUTO_UPDATE` is
/// read on a path where the rest of the config has already been rejected, so a
/// config that fails to parse still gets its update decision honoured - but it
/// does mean `auto_update=1` in a file arms automatic installs while a
/// lowercased `api_key=` in the same file is silently ignored. Pinned by a test
/// rather than being an accident.
// Read only by the Windows updater (`#[cfg(windows)] mod update`); the
// Unix builds store and parse the value but have no auto-install path yet,
// so the functions ride along unused. Same house pattern as the
// Windows-only Config fields above.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_auto_update_text(text: &str) -> Option<String> {
    let mut found = None;
    parse(text, |k, v| {
        if k.eq_ignore_ascii_case("AUTO_UPDATE") {
            found = Some(v.to_string());
        }
    });
    found
}

/// Whether an `AUTO_UPDATE` value asks for automatic installs. Opt-in, so an
/// empty string, a typo and every other spelling all mean "off".
// Read only by the Windows updater (`#[cfg(windows)] mod update`); the
// Unix builds store and parse the value but have no auto-install path yet,
// so the functions ride along unused. Same house pattern as the
// Windows-only Config fields above.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_auto_update(s: &str) -> bool {
    matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

/// One numeric config value, or an error naming the key and the bad text.
fn parse_num(key: &str, v: &str) -> Result<f64, String> {
    v.trim().parse::<f64>().map_err(|_| {
        format!("{key} is not a number: {v:?}")
    })
}

/// One env-file value, with the quote and comment rules applied in the order
/// the rules require: a wholly-quoted value is taken verbatim (quoting is the
/// request that its content not be reinterpreted), and an unquoted value loses
/// a whitespace-separated trailing comment before a single matched quote pair
/// is stripped.
///
/// Named because the ordering is the part that was once a bug (the comment
/// scan reaching inside a quoted value cut `KEYWORDS="k8s #cluster"` at the
/// ` #`), and a fourth rule cannot be added to an inline nested `match` with-
/// out another level that hides which rule runs when.
fn parse_env_value(v: &str) -> &str {
    let v = v.trim();
    match strip_quote_pair(v) {
        Some(inner) => inner,
        None => {
            let v = match v.find(" #").or_else(|| v.find("\t#")) {
                Some(i) => &v[..i],
                None => v,
            };
            let v = v.trim();
            strip_quote_pair(v).unwrap_or(v)
        }
    }
}

/// A non-negative integer field (`VAD_SILENCE_MS`); `f64` first so "3000.5"
/// and "3000x" both fail loudly rather than half-parsing.
fn parse_u32_num(key: &str, v: &str) -> Result<u32, String> {
    let f = parse_num(key, v)?;
    if f < 0.0 || f != f.trunc() || f > u32::MAX as f64 {
        return Err(format!("{key} must be a whole number >= 0: {v:?}"));
    }
    Ok(f as u32)
}

/// `VAD_RMS_THRESHOLD`, held to the `0..=32767` scale both README and
/// `Config::vad_rms_threshold` document.
///
/// The bare number parse used to accept anything `f64` parses, and the three
/// spellings that survive it each break dictation silently, with no config
/// error anywhere:
///
/// - `NaN` and `inf` (and `1e400`, which parses to `inf`) make every comparison
///   `rms > threshold` false, so every chunk reads as silence and every
///   dictation ends at the 10 s no-speech limit reporting "No speech detected".
/// - a negative value is the inverse: digital silence satisfies
///   `0.0 > -400.0`, so the post-voice window never opens and only
///   `MAX_SECONDS` ends the session.
///
/// All three are measured above, not inferred: `"-400".parse::<f64>()` and
/// `"NaN".parse::<f64>()` both return `Ok`. Refusing them at load is the only
/// place the mistake can be named, because the symptom (never speech, or never
/// silence) reads as a broken microphone rather than a typo three screens away.
///
/// The upper bound is the documented one rather than a new invention. A
/// threshold above the largest RMS an i16 buffer can carry is behaviourally
/// identical to `inf` - nothing ever counts as speech - so accepting it would
/// keep the silent failure the check exists to remove.
fn parse_rms_num(key: &str, v: &str) -> Result<f64, String> {
    let f = parse_num(key, v)?;
    if !f.is_finite() || f < 0.0 || f > 32767.0 {
        return Err(format!("{key} must be a number from 0 to 32767: {v:?}"));
    }
    Ok(f)
}

/// `MAX_SECONDS`, which additionally may not be 0 (see `derive`).
fn parse_positive_num(key: &str, v: &str) -> Result<u32, String> {
    let n = parse_u32_num(key, v)?;
    if n == 0 {
        return Err(format!("{key} must be at least 1 second (0 would end every session before it captured anything): {v:?}"));
    }
    Ok(n)
}

/// Run the file-shaped pass over `text` into `raw`: parse each line, map the
/// key through `FIELDS`, and hand the pair to `RawFields::set` as a
/// file-sourced value (the alias-priority guard applies).
///
/// Extracted because the test helper `load_from` had to reproduce this whole
/// block to feed an in-memory config file: the copy is the kind that drifts -
/// if `load` grew a fourth source or the lookup changed shape, the tests would
/// quietly keep exercising the old one. The one difference from `load`'s own
/// use remains deliberate and is `load_from`'s business: it never reads the
/// keyword files, because tests supply their words in the env text itself.
fn apply_env_text(text: &str, raw: &mut RawFields) {
    parse(text, |k, v| {
        if let Some((_, field)) = FIELDS.iter().find(|(names, _)| names.contains(&k)) {
            raw.set(*field, k, v, true);
        }
    });
}

/// The line prelude both file formats share: a UTF-8 BOM has to come off
/// before the first line is parsed, not after - `lines()` does not treat it as
/// whitespace, so `trim()` leaves it attached and the first key becomes
/// `\u{feff}API_KEY` instead of `API_KEY`, or the first keyword becomes
/// `\u{feff}kubernetes` and goes on the wire as `%EF%BB%BFkubernetes` boosting
/// nothing. That key then misses the field it would have filled and is reported
/// as missing, so a file edited in Windows Notepad looks corrupted when it is
/// not. The checksum-file parser in `update.rs` strips it for the same reason: a
/// file format has to be total with respect to what an editor on this platform
/// actually writes. Lines are then trimmed, and blank or `#`-comment lines are
/// dropped.
///
/// `load` feeds three keyword files and the env file through the same two
/// shapes, so the BOM rule lives in one function instead of two copy-pasted
/// preludes that a third caller would have to re-derive.
fn config_lines(text: &str) -> impl Iterator<Item = &str> {
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// One matched quote pair, if `v` is wholly wrapped in one. Returns the content
/// between the quotes; an unmatched or absent pair returns None so the caller
/// treats the text as data (the rule `only_one_matched_quote_pair_is_stripped`
/// pins: never every leading and trailing quote character, just the one pair).
fn strip_quote_pair(v: &str) -> Option<&str> {
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
}

pub fn parse_keywords_text(text: &str, keywords: &mut Vec<String>) {
    // A lowercase index beside the vector, so de-duplication does not have to
    // re-scan every word already collected for every new one. The dedup is
    // case-insensitive by contract, so the key is the lowercase form and the
    // vector keeps the spelling the user wrote. Without this a pasted vendor
    // list is quadratic in its own length: 16k distinct words took ~17 s to
    // parse, and `load` feeds three files into the same vector before the
    // session can start.
    let mut seen: std::collections::HashSet<String> = keywords
        .iter()
        .map(|k| k.to_lowercase())
        .collect();
    for line in config_lines(text) {
        for part in line.split(',') {
            let word = part.trim();
            if word.is_empty() {
                continue;
            }
            if seen.insert(word.to_lowercase()) {
                keywords.push(word.to_string());
            }
        }
    }
}

fn parse<F: FnMut(&str, &str)>(text: &str, mut f: F) {
    for line in config_lines(text) {
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            // Quote first, comment second, but only for values that are NOT
            // wholly quoted. The previous order (comment scan before quote
            // strip) let the comment rule reach inside a quoted value:
            // `KEYWORDS="k8s #cluster"` was cut at the ` #` and then failed the
            // quote match, so the user got a lone leading quote plus a missing
            // word. Quoting a value is exactly the request that its content not
            // be reinterpreted, so a wrapped value is now taken verbatim.
            // An unquoted value keeps every previous rule: the comment must be
            // whitespace-separated (`MODEL=nova-3  # my model` drops the
            // comment; a `#` in a URL fragment stays), and quotes are stripped
            // after the comment is removed so `KEY="a" # c` still yields `a`.
            let v = parse_env_value(v);
            // An empty key names no field; `f` filters on the FIELDS list so
            // this is inert today, but rejecting it here means a malformed line
            // is a parse decision rather than an accident of a later filter.
            if !k.is_empty() {
                f(k, v);
            }
        }
    }
}

/// One configuration field: the first name is canonical, the rest are aliases
/// that mean the same thing. Both sources - mnvoice.env and the process
/// environment - drive from this one table, so the two can never disagree about
/// which names a field accepts (that drift once made `KEYBIND=F9` work in the
/// file while silently doing nothing as an environment variable).
const FIELDS: &[(&[&str], Field)] = &[
    (&["PROTOCOL", "MODE", "PROVIDER"], Field::Protocol),
    (
        &["API_KEY", "DEEPGRAM_API_KEY", "GROQ_API_KEY", "OPENAI_API_KEY"],
        Field::ApiKey,
    ),
    (
        &["MODEL", "DEEPGRAM_MODEL", "GROQ_MODEL", "OPENAI_MODEL"],
        Field::Model,
    ),
    (&["LANGUAGE", "DEEPGRAM_LANGUAGE", "GROQ_LANGUAGE"], Field::Language),
    (
        &["BASE_URL", "DEEPGRAM_BASE_URL", "GROQ_BASE_URL", "ENDPOINT"],
        Field::BaseUrl,
    ),
    (&["MAX_SECONDS"], Field::MaxSeconds),
    (&["TRAILING_SPACE"], Field::TrailingSpace),
    (
        &["KEYWORDS", "KEYTERMS", "CUSTOM_WORDS", "VOCABULARY"],
        Field::Keywords,
    ),
    (&["ORB_COLOR", "ORB_HEX", "COLOR"], Field::OrbColor),
    (&["ORB_FLUID_LEVEL", "ORB_FLUID_AMOUNT", "FLUID_LEVEL"], Field::OrbFluid),
    (&["HOTKEY", "TRIGGER_HOTKEY", "KEYBIND"], Field::Hotkey),
    (&["CANCEL_KEY", "CANCEL_HOTKEY"], Field::CancelKey),
    (&["VAD_SILENCE_MS", "SILENCE_MS"], Field::VadSilenceMs),
    (&["VAD_RMS_THRESHOLD", "RMS_THRESHOLD"], Field::VadRmsThreshold),
    (&["FILLER_WORDS"], Field::FillerWords),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Protocol,
    ApiKey,
    Model,
    Language,
    BaseUrl,
    MaxSeconds,
    TrailingSpace,
    Keywords,
    OrbColor,
    OrbFluid,
    Hotkey,
    CancelKey,
    VadSilenceMs,
    VadRmsThreshold,
    FillerWords,
}

impl Field {
    /// The key this field is documented under - the first name in its
    /// `FIELDS` entry.
    fn canonical(self) -> &'static str {
        FIELDS
            .iter()
            .find(|(_, f)| *f == self)
            .map_or("", |(names, _)| names[0])
    }
}

/// The not-yet-interpreted field values gathered from the sources, in the
/// order they arrive. `load` turns this into a `Config` once both passes ran.
struct RawFields {
    protocol_str: String,
    api_key: String,
    model: String,
    language: String,
    base_url: String,
    // The numeric fields are kept as the text the user wrote, not parsed on
    // arrival. `set` cannot return an error - the file pass and the env pass
    // both feed it, and an error there would mean abandoning the ordering the
    // module depends on - so the parse and its range checks happen once, in
    // `derive`, which is already the function that reports a bad config. They
    // used to parse in the arm with `if let Ok(n)`, which made a typo silently
    // keep the default: `MAX_SECONDS=12o` read as 120 and nothing ever said so.
    max_seconds: String,
    trailing_space: bool,
    keywords: Vec<String>,
    orb_color_str: String,
    orb_fluid_str: String,
    hotkey_str: String,
    cancel_key_str: String,
    vad_silence_ms: String,
    vad_rms_threshold: String,
    filler_words_str: String,
}

impl RawFields {
    fn new() -> Self {
        Self {
            protocol_str: String::new(),
            api_key: String::new(),
            model: String::new(),
            language: String::new(),
            base_url: String::new(),
            max_seconds: DEFAULT_MAX_SECONDS.to_string(),
            trailing_space: true,
            keywords: Vec::new(),
            orb_color_str: String::new(),
            orb_fluid_str: String::new(),
            hotkey_str: String::new(),
            cancel_key_str: String::new(),
            vad_silence_ms: DEFAULT_VAD_SILENCE_MS.to_string(),
            vad_rms_threshold: DEFAULT_VAD_RMS_THRESHOLD.to_string(),
            filler_words_str: String::new(),
        }
    }

    /// One key/value pair from either source: a line from mnvoice.env
    /// (`from_file`) or a process environment variable.
    ///
    /// The file pass accumulates in file order: for the alias-priority fields
    /// (`API_KEY`, `MODEL`, `LANGUAGE`, `BASE_URL`) the canonical key wins
    /// over its aliases whatever order the lines appear in, and an alias fills
    /// only a field nothing has set yet (a Deepgram-specific default
    /// overridable by a plain `API_KEY=`); every other field takes the last
    /// line that names it, so an ALIAS CAN OVERWRITE A CANONICAL KEY for those
    /// (`ORB_COLOR=#111111` followed by `COLOR=#222222` ends as `#222222`, and
    /// the same holds for `HOTKEY`/`KEYBIND` and the other unguarded fields).
    /// That is deliberate - a config line is a request, and the later one is
    /// the more recent intent - but it is the opposite of the guarded fields,
    /// so the spelling the user wants must come last for these. The one
    /// exception is `KEYWORDS`, which ACCUMULATES across every line that names
    /// it and is never cleared - a `KEYWORDS=` line in the file and a
    /// `KEYWORDS` variable in the environment are both meant to contribute,
    /// and a test locks that. The environment pass runs after the file, so it
    /// overrides anything the file set and is not subject to the guards.
    fn set(&mut self, field: Field, k: &str, v: &str, from_file: bool) {
        let canonical_overrides = from_file && k == field.canonical();
        // The four string fields below share one guard, so the guard is one
        // function: they differ only in which field they write, and a fifth
        // field with the same alias-priority semantics should not have to
        // copy the same condition to get them. `canonical_overrides` has to be
        // read before `self` is borrowed mutably, hence two calls rather than
        // one with a closure over `self`.
        match field {
            Field::Protocol => self.protocol_str = v.to_string(),
            Field::ApiKey => guarded_set(
                &mut self.api_key,
                from_file,
                canonical_overrides,
                v,
            ),
            Field::Model => guarded_set(&mut self.model, from_file, canonical_overrides, v),
            Field::Language => guarded_set(&mut self.language, from_file, canonical_overrides, v),
            Field::BaseUrl => guarded_set(&mut self.base_url, from_file, canonical_overrides, v),
            Field::OrbColor => self.orb_color_str = v.to_string(),
            Field::OrbFluid => self.orb_fluid_str = v.to_string(),
            Field::Hotkey => self.hotkey_str = v.to_string(),
            Field::CancelKey => self.cancel_key_str = v.to_string(),
            Field::FillerWords => self.filler_words_str = v.to_string(),
            Field::MaxSeconds => self.max_seconds = v.to_string(),
            // Deliberately the one key where an unknown spelling keeps the
            // default on: TRAILING_SPACE defaults to enabled, and treating a
            // typo as "off" would let `TRAILING_SPACE=flase` silently strip
            // the space. Only the exact spelling 0 disables.
            // Trimmed like every other field's value: the file pass trims and
            // strips quotes (see `parse`), the environment pass does not, so
            // `TRAILING_SPACE=" 0"` from a shell or a wrapper script would
            // otherwise keep the trailing space that the user asked to turn
            // off. Treating a padded "0" as on is the same typo the module's
            // other fields already refuse to treat as off.
            Field::TrailingSpace => self.trailing_space = v.trim() != "0",
            Field::Keywords => parse_keywords_text(v, &mut self.keywords),
            Field::VadSilenceMs => self.vad_silence_ms = v.to_string(),
            Field::VadRmsThreshold => self.vad_rms_threshold = v.to_string(),
        }
    }
}

/// A `Config` for tests, with every field at a known-good value.
///
/// One constructor for all of them: the four separate 19-field literals this
/// replaced had to be edited in four places every time a field was added, and
/// two of them had already drifted. Individual tests override only what they
/// are actually about, which also makes it obvious when a test needs a field
/// the others never touch.
#[cfg(test)]
pub fn test_config() -> Config {
    Config {
        protocol: Protocol::Streaming,
        api_key: "tok".into(),
        model: "nova-3".into(),
        language: "en".into(),
        base_url: "https://api.deepgram.com".into(),
        max_seconds: DEFAULT_MAX_SECONDS,
        trailing_space: true,
        keywords: vec!["Kubernetes".into()],
        orb_color: DEFAULT_ORB_COLOR,
        orb_fluid_level: DEFAULT_ORB_FLUID_LEVEL,
        hotkey: (0x4001, 0x20),
        hotkey_str: "Alt+Space".into(),
        cancel_key: (DEFAULT_CANCEL_MOD, DEFAULT_CANCEL_VK),
        cancel_key_str: DEFAULT_CANCEL_STR.into(),
        vad_silence_ms: DEFAULT_VAD_SILENCE_MS,
        vad_rms_threshold: DEFAULT_VAD_RMS_THRESHOLD,
        strip_fillers: true,
    }
}

// CANCEL_KEY defaults to a chord, not a bare Escape. Measured on
// Kubuntu/KDE 2026-10-20: a GlobalShortcuts binding on plain Esc is grabbed by
// kglobalaccel for the process's whole life, and closing the portal session
// does NOT release the grab (verified: after the session was closed the grab
// stayed live until the process exited). A bare Esc could never be given up
// from inside the app, so it is dead in every other app the whole time
// mnvoice runs. Alt+Esc is unbound by default on KDE (checked against
// kglobalshortcutsrc/kwinrc) and a chord remains grabbable without costing
// Esc. Set CANCEL_KEY=none to disable cancel, or CANCEL_KEY=Alt+Escape to
// pin this, or pick any other chord.
pub const DEFAULT_CANCEL_MOD: u32 = 0x0001; // MOD_ALT | MOD_NOREPEAT
pub const DEFAULT_CANCEL_VK: u32 = 0x1B; // VK_ESCAPE
pub const DEFAULT_CANCEL_STR: &str = "Alt+Escape";

/// How long one dictation may run before it stops itself.
pub const DEFAULT_MAX_SECONDS: u32 = 120;

/// How long the detector waits for speech after the last word before ending a
/// dictation.
pub const DEFAULT_VAD_SILENCE_MS: u32 = 3000;

/// The RMS level at or below which a chunk counts as silence. Deliberately a
/// raw amplitude, not a percentage: the value is compared against the RMS both
/// engines compute over i16 samples (see `rms_of`), so the scale runs 0-32767.
pub const DEFAULT_VAD_RMS_THRESHOLD: f64 = 400.0;

/// The cancel key to register plus the spelling to show for it, decided in
/// one place: the display must never advertise a key other than the pair
/// that was registered, and the fallbacks live here rather than being
/// re-derived by each caller.
fn cancel_key_with_display(s: &str) -> ((u32, u32), String) {
    hotkey_with_display(s, (DEFAULT_CANCEL_MOD, DEFAULT_CANCEL_VK), DEFAULT_CANCEL_STR)
}

/// Writes `v` into `dest` under the alias-priority rule the string fields
/// share: a real environment variable always lands, a file's canonical key
/// overrides an alias the file set first, and a file's alias only fills a
/// field that is still empty.
///
/// The three conditions are the field-independent half of that rule, so they
/// live here rather than in four match arms that have to be edited together
/// whenever the rule changes.
fn guarded_set(dest: &mut String, from_file: bool, canonical_overrides: bool, v: &str) {
    if !from_file || canonical_overrides || dest.is_empty() {
        *dest = v.to_string();
    }
}

/// One key's registration pair plus the spelling to show for it.
///
/// Shared by the toggle hotkey and the cancel key so the two cannot drift:
/// they answer the same question ("what pair is registered, and what does the
/// tray tip say") with the same three fallbacks - empty and unparseable both
/// land on `default`, and the case-insensitive word "none" registers the
/// zero-pair sentinel that means "bind nothing".
///
/// The display rule for "none" is lowercase, and it is a rule rather than an
/// accident of one caller's input: the tray tip and the log read better with
/// one canonical spelling, and both callers route through here so they cannot
/// show `NONE` to a user who typed `none`.
fn hotkey_with_display(s: &str, default: (u32, u32), default_str: &str) -> ((u32, u32), String) {
    let s = s.trim();
    if s.eq_ignore_ascii_case("none") {
        return ((0, 0), "none".to_string());
    }
    match parse_hotkey(s) {
        Some(pair) if !s.is_empty() => (pair, s.to_string()),
        _ => (default, default_str.to_string()),
    }
}

pub const DEFAULT_ORB_COLOR: (f32, f32, f32) = (1.0, 0.18, 0.58);
pub const DEFAULT_ORB_FLUID_LEVEL: f32 = 0.75;

/// Parses color from preset name or hex `#RRGGBB` / `RRGGBB`.
/// Returns RGB float triple in range `0.0..=1.0`.
pub fn parse_color(s: &str) -> (f32, f32, f32) {
    let s = s.trim().to_lowercase();
    match s.as_str() {
        "cyan" | "teal" => (0.0, 0.95, 0.90),
        "purple" | "violet" => (0.68, 0.25, 0.98),
        "blue" | "sapphire" => (0.20, 0.55, 1.0),
        "emerald" | "green" => (0.12, 0.85, 0.45),
        "amber" | "orange" | "gold" => (1.0, 0.62, 0.05),
        "red" | "ruby" => (1.0, 0.22, 0.22),
        "white" | "silver" => (0.95, 0.95, 1.0),
        "pink" | "hot_pink" | "magenta" => DEFAULT_ORB_COLOR,
        _ => {
            let hex = s.trim_start_matches('#');
            // Six ASCII hex digits and nothing else. The length check alone is
            // not enough: `hex.len()` is bytes, so a six-byte string of
            // multi-byte characters passes it and the byte-offset slices below
            // then split a character - `ORB_COLOR=€€` is exactly six bytes
            // with boundaries at 0/3/6 and panicked at startup instead of
            // falling back to the default. Validating the alphabet first also
            // makes the three parses below guaranteed to succeed, so the
            // `if let (Ok, Ok, Ok)` is a shape check rather than the real test.
            if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                if let (Ok(r), Ok(g), Ok(b)) = (
                    u8::from_str_radix(&hex[0..2], 16),
                    u8::from_str_radix(&hex[2..4], 16),
                    u8::from_str_radix(&hex[4..6], 16),
                ) {
                    return (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
                }
            }
            // Default vibrant hot pink
            DEFAULT_ORB_COLOR
        }
    }
}

/// Parses fluid level string into float in range `0.05..=1.0`.
pub fn parse_fluid_level(s: &str) -> f32 {
    let s = s.trim().trim_end_matches('%');
    if let Ok(val) = s.parse::<f32>() {
        if val.is_finite() {
            let val = if val > 1.0 { val / 100.0 } else { val };
            return val.clamp(0.05, 1.0);
        }
    }
    DEFAULT_ORB_FLUID_LEVEL // default
}

pub const DEFAULT_HOTKEY_MOD: u32 = 0x0001 | 0x4000; // MOD_ALT | MOD_NOREPEAT
pub const DEFAULT_HOTKEY_VK: u32 = 0x20; // VK_SPACE
pub const DEFAULT_HOTKEY_STR: &str = "Alt+Space";

/// The virtual key for the two parts `parse_hotkey`'s table cannot name
/// directly: an F-number (`f13`, 1..=24) or a single alphanumeric character
/// (`d` -> `D`). These used to be nested inline in the match arm, four and five
/// levels deep, which made the arm's `other =>` read like a maze; as a function
/// each rule is one flat branch.
///
/// Anything else returns None, and the caller leaves `vk` at whatever an
/// earlier part set - the same behaviour as before the extraction.
///
/// The F-number branch is guarded on a non-empty remainder, and that guard is
/// load-bearing: a bare `f` is the letter F (VK_F, 0x46), not a malformed
/// F-number. Taking the branch on `"f"` itself made `"".parse::<u32>()` fail
/// and the `?` return from the whole function, so the character branch below
/// was never reached and `HOTKEY=F` silently fell back to the default.
fn key_vk(part: &str) -> Option<u32> {
    if let Some(f_num) = part.strip_prefix('f') {
        if !f_num.is_empty() {
            let num = f_num.parse::<u32>().ok()?;
            if (1..=24).contains(&num) {
                return Some(0x70 + (num - 1)); // VK_F1 = 0x70
            }
            return None;
        }
    }
    let mut chars = part.chars();
    let ch = chars.next()?.to_ascii_uppercase();
    if chars.next().is_none() && ch.is_ascii_alphanumeric() {
        return Some(ch as u32);
    }
    None
}

/// Parses a hotkey string like "Alt+Space", "Ctrl+Shift+D", "Win+Space", "F8", "Escape" into (modifiers, vk).
pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return None;
    }

    let mut modifiers: u32 = 0x4000; // MOD_NOREPEAT
    let mut vk: u32 = 0;

    for part in s.split('+').map(|p| p.trim()) {
        match part.to_lowercase().as_str() {
            "alt" | "option" => modifiers |= 0x0001,      // MOD_ALT
            "ctrl" | "control" => modifiers |= 0x0002,  // MOD_CONTROL
            "shift" => modifiers |= 0x0004,             // MOD_SHIFT
            "win" | "windows" | "super" | "cmd" => modifiers |= 0x0008, // MOD_WIN
            "space" => vk = 0x20,                       // VK_SPACE
            "esc" | "escape" => vk = 0x1B,              // VK_ESCAPE
            "tab" => vk = 0x09,                         // VK_TAB
            "enter" | "return" => vk = 0x0D,            // VK_RETURN
            "backquote" | "tilde" | "`" | "~" => vk = 0xC0, // VK_OEM_3
            "pause" => vk = 0x13,
            "caps" | "capslock" => vk = 0x14,
            "insert" => vk = 0x2D,
            "delete" | "del" => vk = 0x2E,
            "home" => vk = 0x24,
            "end" => vk = 0x23,
            "pageup" | "pgup" => vk = 0x21,
            "pagedown" | "pgdn" => vk = 0x22,
            other => {
                if let Some(v) = key_vk(other) {
                    vk = v;
                }
            }
        }
    }

    if vk != 0 {
        Some((modifiers, vk))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BOM is what Windows Notepad and a `>` redirect actually write, so a
    /// config authored on this platform carries one. `lines()` does not treat it
    /// as whitespace, so without the strip the first key reads as
    /// `\u{feff}API_KEY` and is reported missing - which looks like a corrupted
    /// file rather than an editor artefact.
    #[test]
    fn a_bom_does_not_rename_the_first_key() {
        let mut keys = Vec::new();
        parse("\u{feff}API_KEY=abc\nMODEL=nova-3\n", |k, v| {
            keys.push(format!("{k}={v}"));
        });
        assert_eq!(keys, vec!["API_KEY=abc", "MODEL=nova-3"]);
    }

    #[test]
    fn test_parse_keywords_text() {
        let mut kw = Vec::new();
        let input = "
        # comment
        mnvoice, herdr
        Kubernetes
        TypeScript, kubernetes
        ";
        parse_keywords_text(input, &mut kw);
        assert_eq!(kw, vec!["mnvoice", "herdr", "Kubernetes", "TypeScript"]);
    }

    /// The keyword files are edited on this platform with the same editors that
    /// write a BOM, and the first word is the one they corrupt. Order is part
    /// of the contract too: the list is sent to the provider as a prompt.
    #[test]
    fn a_bom_does_not_rename_the_first_keyword() {
        let mut kw = Vec::new();
        parse_keywords_text("\u{feff}alpha\nbeta", &mut kw);
        assert_eq!(kw, vec!["alpha", "beta"]);
    }

    #[test]
    fn test_parse_color() {
        assert_eq!(parse_color("cyan"), (0.0, 0.95, 0.90));
        assert_eq!(parse_color("purple"), (0.68, 0.25, 0.98));
        assert_eq!(parse_color("emerald"), (0.12, 0.85, 0.45));
        assert_eq!(parse_color("pink"), DEFAULT_ORB_COLOR);
        assert_eq!(parse_color("hot_pink"), DEFAULT_ORB_COLOR);
        assert_eq!(parse_color("magenta"), DEFAULT_ORB_COLOR);
        assert_eq!(parse_color("not-a-color"), DEFAULT_ORB_COLOR);
        let (r, g, b) = parse_color("#FF2D78");
        assert!((r - 1.0).abs() < 0.01);
        assert!((g - 0.176).abs() < 0.01);
        assert!((b - 0.470).abs() < 0.01);
    }

    #[test]
    fn test_parse_fluid_level() {
        assert_eq!(parse_fluid_level("0.5"), 0.5);
        assert_eq!(parse_fluid_level("80%"), 0.8);
        assert_eq!(parse_fluid_level("1.0"), 1.0);
        assert_eq!(parse_fluid_level("150%"), 1.0);
        assert_eq!(parse_fluid_level("0.01"), 0.05);
        assert_eq!(parse_fluid_level("nan"), 0.75);
        assert_eq!(parse_fluid_level("NaN"), 0.75);
    }

    #[test]
    fn test_auto_update_is_opt_in_ignoring_typo_and_empty() {
        for on in ["1", "true", "TRUE", " On ", "yes", "yes\n", "1 "] {
            assert!(parse_auto_update(on), "{on:?} should arm auto-update");
        }
        for off in ["", "  ", "0", "false", "no", "off", "ture", "enabled"] {
            assert!(!parse_auto_update(off), "{off:?} must not arm auto-update");
        }
    }

    #[test]
    fn test_auto_update_is_read_from_the_env_file_without_the_api_key() {
        // A config that fails load() outright - no API key anywhere - must not
        // also take AUTO_UPDATE down with it, so this runs the same extraction
        // the fallback uses and only needs the env text.
        let broken = "PROVIDER=deepgram\nHOTKEY=Alt+Space\n";
        assert_eq!(parse_auto_update_text(broken), None);

        // Named but empty counts as present, and empty is off.
        assert_eq!(parse_auto_update_text("AUTO_UPDATE=\n"), Some(String::new()));
        assert!(!parse_auto_update(""), "an empty value is off");

        // Last entry wins, so a later correction overrides an earlier one, and
        // comments, stray spaces, quotes and case do not derail the read.
        let text = "# not this: AUTO_UPDATE=0\n'AUTO_UPDATE'=\"\n1\"\nAPI_KEY=\nAUTO_UPDATE = on \n";
        assert_eq!(parse_auto_update_text(text).as_deref(), Some("on"));
        assert!(
            parse_auto_update(parse_auto_update_text(text).as_deref().unwrap()),
            "a corrected later entry must arm the updater"
        );
        assert_eq!(parse_auto_update_text("auto_update=true").as_deref(), Some("true"));
    }

    #[test]
    fn test_parse_hotkey() {
        // Alt+Space -> MOD_ALT (0x01) | MOD_NOREPEAT (0x4000) = 0x4001, VK_SPACE = 0x20
        assert_eq!(parse_hotkey("Alt+Space"), Some((0x4001, 0x20)));

        // Ctrl+Shift+D -> MOD_CONTROL (0x02) | MOD_SHIFT (0x04) | MOD_NOREPEAT (0x4000) = 0x4006, 'D' = 0x44
        assert_eq!(parse_hotkey("Ctrl+Shift+D"), Some((0x4006, 0x44)));

        // F9 -> MOD_NOREPEAT (0x4000), VK_F9 = 0x78
        assert_eq!(parse_hotkey("F9"), Some((0x4000, 0x78)));

        // Escape -> MOD_NOREPEAT (0x4000), VK_ESCAPE = 0x1B
        assert_eq!(parse_hotkey("Escape"), Some((0x4000, 0x1B)));

        // None
        assert_eq!(parse_hotkey("none"), None);

        // A bare F is the letter key (VK_F = 0x46), not a malformed F-number.
        // The F-number branch used to `?` out of `key_vk` on the empty
        // remainder, so this parsed to nothing and HOTKEY=F silently kept the
        // default hotkey while the tray tip said "F".
        assert_eq!(parse_hotkey("F"), Some((0x4000, 0x46)));
        assert_eq!(parse_hotkey("Ctrl+F"), Some((0x4002, 0x46)));
        // A malformed F-number still names no key: f300 is out of the 1..=24
        // range, and the character branch must not rescue it into VK_F.
        assert_eq!(parse_hotkey("f300"), None);
    }

    #[test]
    fn cancel_key_none_disables_instead_of_rearming_the_default() {
        // The docs promise "none" disables the cancel key. The old fallback
        // silently re-registered Escape, so a user who asked for none still
        // lost transcripts to a stray Escape press - and a bare Escape
        // registered on KDE is grabbed by kglobalaccel for the process's
        // whole life (see DEFAULT_CANCEL_STR).
        //
        // Driven through `cancel_key_with_display`, the same function the live
        // tray and log paths call, rather than a test-only `resolve_cancel_key`
        // wrapper that existed solely to be called here. The wrapper widened
        // the module's public surface for one test and duplicated coverage the
        // three hotkey tests above already provide.
        assert_eq!(cancel_key_with_display("none").0, (0, 0));
        assert_eq!(cancel_key_with_display("NONE").0, (0, 0));
        assert_eq!(cancel_key_with_display("Escape").0, (0x4000, 0x1B));
        assert_eq!(
            cancel_key_with_display("unparseable").0,
            (DEFAULT_CANCEL_MOD, DEFAULT_CANCEL_VK),
            "unknown spelling falls back to the default"
        );
    }

    #[test]
    fn every_field_has_a_unique_canonical_name() {
        // The whole table's names must be unique, not merely each row's own
        // first name absent from its own alias list - the assertion the old
        // body made, which no reachable table could fail because `set` matches
        // a name by scanning rows in order, so a name in two rows is silently
        // claimed by the first and the second row's field never fills.
        //
        // Two properties, then, both about the table as a whole: every name
        // appears exactly once across all rows, and every row has a canonical
        // name (its first) at all.
        let mut seen: Vec<&str> = Vec::new();
        for (names, field) in FIELDS {
            let (canonical, _aliases) = names.split_first().expect("every field needs a canonical name");
            assert!(!canonical.is_empty(), "{field:?}: a canonical name cannot be empty");
            for name in names.iter() {
                assert!(
                    !seen.contains(name),
                    "{name} is claimed by more than one field: {field:?} and the earlier row"
                );
                seen.push(name);
            }
        }
        // The count is the cheap proof the loop actually looked at everything:
        // a row that failed to reach the inner loop would show up here.
        let total: usize = FIELDS.iter().map(|(names, _)| names.len()).sum();
        assert_eq!(seen.len(), total, "some name was never checked");
    }

    #[test]
    fn a_name_accepted_from_the_file_is_accepted_from_the_environment() {
        // The drift this table exists to prevent: these five names were once
        // file-only, so `KEYBIND=F9` as an environment variable silently did
        // nothing while the same line in mnvoice.env worked. Prove each one
        // lands in its field from BOTH sources, observed in the RawFields the
        // two passes actually fill.
        let mut raw = RawFields::new();

        raw.set(Field::Hotkey, "KEYBIND", "F9", true);
        assert_eq!(
            raw.hotkey_str, "F9",
            "KEYBIND must reach the hotkey from the file"
        );
        raw.set(Field::Hotkey, "", "F10", false);
        assert_eq!(
            raw.hotkey_str, "F10",
            "KEYBIND must reach the hotkey from the env"
        );

        raw.set(Field::OrbColor, "COLOR", "#A855F7", true);
        assert_eq!(
            raw.orb_color_str, "#A855F7",
            "COLOR must reach the orb color from the file"
        );
        raw.set(Field::OrbColor, "", "cyan", false);
        assert_eq!(
            raw.orb_color_str, "cyan",
            "COLOR must reach the orb color from the env"
        );

        raw.set(Field::OrbFluid, "FLUID_LEVEL", "0.9", true);
        assert_eq!(
            raw.orb_fluid_str, "0.9",
            "FLUID_LEVEL must reach the fluid level from the file"
        );
        raw.set(Field::OrbFluid, "", "50%", false);
        assert_eq!(
            raw.orb_fluid_str, "50%",
            "FLUID_LEVEL must reach the fluid level from the env"
        );

        raw.set(Field::Keywords, "CUSTOM_WORDS", "herdr, mnvoice", true);
        assert!(
            raw.keywords.contains(&"herdr".to_string())
                && raw.keywords.contains(&"mnvoice".to_string()),
            "CUSTOM_WORDS must reach the keywords from the file, got {:?}",
            raw.keywords
        );
        raw.set(Field::Keywords, "", "PostgreSQL", false);
        assert!(
            raw.keywords.contains(&"PostgreSQL".to_string()),
            "CUSTOM_WORDS must reach the keywords from the env, got {:?}",
            raw.keywords
        );

        raw.set(Field::Keywords, "VOCABULARY", "six", true);
        assert!(
            raw.keywords.contains(&"six".to_string()),
            "VOCABULARY must reach the keywords from the file"
        );
    }

    #[test]
    fn a_whitespace_only_model_falls_back_to_the_default() {
        // An environment variable is untrimmed and `load` hands it straight to
        // `raw.set`, so `MODEL=" "` used to produce a Config whose model was
        // one space and no error - and listen_url then spliced that space into
        // the query. The emptiness rule the API key uses at the top of `derive`
        // now applies to model, base_url and language too, so "empty" means one
        // thing across every field.
        //
        // Built through RawFields rather than `load_from` on purpose: `load_from`
        // goes through `parse`, which already trims, so it would pass with or
        // without the fix. The environment path is the one that does not.
        let mut raw = RawFields::new();
        raw.set(Field::ApiKey, "API_KEY", "k", false);
        raw.set(Field::Model, "MODEL", "   ", false);
        raw.set(Field::Language, "LANGUAGE", "\t", false);
        raw.set(Field::BaseUrl, "BASE_URL", "  ", false);
        let cfg = derive(raw).expect("a whitespace value is not an error, it is a default");
        assert_eq!(cfg.model, "nova-3", "a whitespace model falls back");
        assert_eq!(cfg.language, "en", "a whitespace language falls back");
        assert_eq!(
            cfg.base_url, "https://api.deepgram.com",
            "a whitespace base URL falls back"
        );
        // A real value survives the same trim, and only the padding goes.
        let mut raw = RawFields::new();
        raw.set(Field::ApiKey, "API_KEY", "k", false);
        raw.set(Field::Model, "MODEL", "  nova-3  ", false);
        let cfg = derive(raw).expect("a padded value derives");
        assert_eq!(cfg.model, "nova-3", "a padded real value is trimmed");
    }

    #[test]
    fn an_inline_comment_is_not_part_of_the_value() {
        // The parser had no notion of a malformed line, so
        // `MODEL=nova-3  # my model` yielded the literal string
        // `nova-3  # my model`, which then went onto the wire verbatim. A `#`
        // preceded by whitespace is a comment; one inside a value with no space
        // before it (a URL fragment) is left alone.
        let cfg = load_from("API_KEY=k\nMODEL=nova-3  # my model");
        assert_eq!(cfg.model, "nova-3", "a trailing comment is stripped");
        let cfg = load_from("API_KEY=k\nBASE_URL=https://api.deepgram.com/v1#frag");
        assert_eq!(
            cfg.base_url, "https://api.deepgram.com/v1#frag",
            "a # with no preceding whitespace is part of the value"
        );
    }

    #[test]
    fn only_one_matched_quote_pair_is_stripped() {
        // `trim_matches('"').trim_matches('\'')` stripped every leading and
        // trailing quote character, so a value that legitimately begins or ends
        // with one was silently altered. One matched pair is the whole rule.
        let cfg = load_from("API_KEY=\"quoted-key\"");
        assert_eq!(cfg.api_key, "quoted-key", "a matched pair is stripped");
        let cfg = load_from("API_KEY='single'");
        assert_eq!(cfg.api_key, "single", "a matched single-quote pair too");
        let cfg = load_from("API_KEY=\"unclosed");
        assert_eq!(
            cfg.api_key, "\"unclosed",
            "an unmatched quote is data, not a delimiter"
        );
        let cfg = load_from("API_KEY=\"\"doublequoted\"\"");
        assert_eq!(
            cfg.api_key, "\"doublequoted\"",
            "only the outermost pair is stripped"
        );
    }

    #[test]
    fn a_multibyte_hex_color_falls_back_instead_of_panicking() {
        // The hex branch gated on `hex.len() == 6`, which is BYTES, and then
        // sliced `&hex[0..2]` at byte offsets. `€€` is exactly six bytes with
        // boundaries at 0/3/6, so the slice split a character and panicked -
        // at startup, because `derive` calls parse_color unconditionally.
        // Verified by measurement before the fix: the panic message was
        // "end byte index 2 is not a char boundary; it is inside '€'".
        // Both of these must fall back to the documented default.
        assert_eq!(parse_color("€€"), DEFAULT_ORB_COLOR);
        assert_eq!(parse_color("ééé"), DEFAULT_ORB_COLOR);
        // The ASCII cases still work, including the ones that used to take the
        // safe path only by accident.
        assert_eq!(parse_color("zzzzzz"), DEFAULT_ORB_COLOR);
        assert_eq!(parse_color("ff0080"), (1.0, 0.0, 128.0 / 255.0));
        assert_eq!(parse_color("#ff0080"), (1.0, 0.0, 128.0 / 255.0));
    }

    /// A line with no key names no field. `f` filters on the FIELDS list, so an
    /// empty key was already inert - but rejecting it in the parser makes it a
    /// parse decision rather than something a later filter happened to catch,
    /// and this pins that the callback is never handed one.
    #[test]
    fn a_line_with_no_key_names_no_field() {
        let mut seen: Vec<(String, String)> = Vec::new();
        parse(
            "=orphan\n   = spaced orphan\nAPI_KEY=k\n=another\n",
            |k, v| seen.push((k.to_string(), v.to_string())),
        );
        assert_eq!(
            seen,
            vec![("API_KEY".to_string(), "k".to_string())],
            "only the line that names a field may reach the callback"
        );
    }

    #[test]
    fn the_file_pass_lets_the_canonical_key_override_an_alias() {
        let mut raw = RawFields::new();
        raw.set(Field::ApiKey, "DEEPGRAM_API_KEY", "alias", true);
        assert_eq!(raw.api_key, "alias", "an alias fills an unset field");
        raw.set(Field::ApiKey, "API_KEY", "canon", true);
        assert_eq!(raw.api_key, "canon", "the canonical key overrides");
        raw.set(Field::ApiKey, "DEEPGRAM_API_KEY", "alias-2", true);
        assert_eq!(
            raw.api_key, "canon",
            "a later alias does not override the canonical key"
        );
    }

    #[test]
    fn the_environment_is_the_highest_priority_source() {
        let mut raw = RawFields::new();
        raw.set(Field::ApiKey, "API_KEY", "from-file", true);
        raw.set(Field::ApiKey, "", "from-env", false);
        assert_eq!(raw.api_key, "from-env");
        raw.set(Field::Hotkey, "KEYBIND", "F9", true);
        raw.set(Field::Hotkey, "", "Ctrl+Shift+D", false);
        assert_eq!(raw.hotkey_str, "Ctrl+Shift+D");
    }

    /// Runs the real file pass over `text` and the real derivation, standing
    /// in for the mnvoice.env beside the exe. The API key load() demands is
    /// always supplied so the config derives instead of erroring.
    fn load_from(text: &str) -> Config {
        let mut raw = RawFields::new();
        apply_env_text(text, &mut raw);
        if raw.api_key.trim().is_empty() {
            raw.api_key = "test-key".to_string();
        }
        derive(raw).expect("test config should derive")
    }

    /// The merged setter keeps both sources' semantics: an alias from the
    /// file yields to the canonical name, the same alias from the environment
    /// always lands.
    #[test]
    fn file_alias_yields_but_env_alias_always_lands() {
        let mut raw = RawFields::new();
        raw.set(Field::ApiKey, "DEEPGRAM_API_KEY", "alias", true);
        assert_eq!(raw.api_key, "alias", "a file alias fills an empty field");
        raw.set(Field::ApiKey, "DEEPGRAM_API_KEY", "alias2", true);
        assert_eq!(raw.api_key, "alias", "a file alias never overrides");
        raw.set(Field::ApiKey, "API_KEY", "canonical", true);
        assert_eq!(raw.api_key, "canonical", "the canonical name overrides");
        raw.set(Field::ApiKey, "DEEPGRAM_API_KEY", "alias3", false);
        assert_eq!(raw.api_key, "alias3", "an environment alias always lands");
    }

    /// An unparseable HOTKEY registers the default pair, so the displayed
    /// spelling must fall back with it - the tray tip and the log must not
    /// advertise a key that does nothing.
    #[test]
    fn an_unparseable_hotkey_displays_the_pair_actually_registered() {
        let cfg = load_from("API_KEY=k\nHOTKEY=Foobar\nCANCEL_KEY=AlsoNotAKey");
        assert_eq!(cfg.hotkey, (0x0001 | 0x4000, 0x20));
        assert_eq!(cfg.hotkey_str, "Alt+Space");
        assert_eq!(cfg.cancel_key, (DEFAULT_CANCEL_MOD, DEFAULT_CANCEL_VK));
        assert_eq!(cfg.cancel_key_str, DEFAULT_CANCEL_STR);
    }

    /// "none" means the same thing for both keys: off. It used to disable
    /// only CANCEL_KEY while HOTKEY=none silently registered the default
    /// Alt+Space - one spelling, two meanings. The zero pair is the same
    /// "register nothing" sentinel the cancel key uses.
    #[test]
    fn hotkey_none_disables_the_toggle_hotkey_like_cancel_none() {
        let cfg = load_from("API_KEY=k\nHOTKEY=none\nCANCEL_KEY=none");
        assert_eq!(cfg.hotkey, (0, 0), "the disabled sentinel pair");
        assert_eq!(cfg.hotkey_str, "none");
        assert_eq!(cfg.cancel_key, (0, 0));
        assert_eq!(cfg.cancel_key_str, "none");
        // Case-insensitive, like every other spelling.
        let cfg = load_from("API_KEY=k\nHOTKEY=NONE");
        assert_eq!(cfg.hotkey, (0, 0));
    }

    /// A key that parses keeps its spelling on screen, and "none" still
    /// disables the cancel key under its own name.
    #[test]
    fn a_parsed_hotkey_displays_its_own_spelling() {
        let cfg = load_from("API_KEY=k\nHOTKEY=Ctrl+Shift+D\nCANCEL_KEY=none");
        assert_eq!(cfg.hotkey_str, "Ctrl+Shift+D");
        assert_eq!(cfg.cancel_key, (0, 0));
        assert_eq!(cfg.cancel_key_str, "none");
    }

    /// Both keys now resolve through one helper, so "none" displays the same
    /// canonical spelling whichever key it disables. They used to disagree:
    /// the hotkey normalized to lowercase while the cancel key echoed the
    /// user's case, so `CANCEL_KEY=NONE` showed `NONE` in the tray tip and
    /// `HOTKEY=NONE` showed `none` for the identical input.
    #[test]
    fn none_displays_one_spelling_for_both_keys() {
        let cfg = load_from("API_KEY=k\nHOTKEY=NONE\nCANCEL_KEY=NoNe");
        assert_eq!(cfg.hotkey_str, "none");
        assert_eq!(cfg.cancel_key_str, "none");
        assert_eq!(cfg.hotkey, (0, 0));
        assert_eq!(cfg.cancel_key, (0, 0));
    }

    /// A padded value is the same value: the file pass trims every field, so
    /// `TRAILING_SPACE=" 0"` written in the config file must disable the
    /// trailing space exactly as the unpadded spelling does.
    #[test]
    fn a_padded_zero_still_turns_the_trailing_space_off() {
        let cfg = load_from("API_KEY=k\nTRAILING_SPACE=\" 0 \"");
        assert!(
            !cfg.trailing_space,
            "a whitespace-padded 0 is the 0 the user typed"
        );
    }

    #[test]
    fn the_stored_api_key_is_trimmed() {
        // The empty check trims, and `auth_value` interpolates the stored value
        // straight into the Authorization header. The two used to disagree: an
        // env-style padded key passed the check and went out as `Token tok `.
        let mut raw = RawFields::new();
        raw.set(Field::ApiKey, "", " padded-key ", false);
        let cfg = derive(raw).expect("a padded key is a key");
        assert_eq!(cfg.api_key, "padded-key");
        assert_eq!(
            super::auth_value(&cfg.api_key, "Token"),
            "Token padded-key",
            "the header value carries no stray space"
        );
    }

    #[test]
    fn auth_value_passes_a_prefixed_scheme_through() {
        // The rule lives here now that both call sites use `config::auth_value`
        // directly: a key that already carries a scheme is passed through, so a
        // user who pasted `Bearer sk-...` is not sent `Token Bearer sk-...`.
        assert_eq!(auth_value("Bearer sk-x", "Token"), "Bearer sk-x");
        assert_eq!(auth_value("Token xyz", "Bearer"), "Token xyz");
        assert_eq!(auth_value("raw-key", "Token"), "Token raw-key");
        assert_eq!(auth_value("raw-key", "Bearer"), "Bearer raw-key");
    }

    #[test]
    fn a_bad_number_is_an_error_naming_its_key() {
        // These used to be `if let Ok(n) = v.parse()` inside `set`: a typo kept
        // the default silently, so `MAX_SECONDS=12o` recorded 120 and nothing
        // ever reported it.
        let err = |text: &str| {
            let mut raw = RawFields::new();
            apply_env_text(text, &mut raw);
            derive(raw).err().unwrap_or_else(|| "no error raised".into())
        };
        assert!(err("API_KEY=k\nMAX_SECONDS=12o").contains("MAX_SECONDS"));
        assert!(err("API_KEY=k\nVAD_SILENCE_MS=abc").contains("VAD_SILENCE_MS"));
        assert!(err("API_KEY=k\nVAD_RMS_THRESHOLD=4OO").contains("VAD_RMS_THRESHOLD"));
        // 0 ends the session before it captures anything: refused.
        assert!(err("API_KEY=k\nMAX_SECONDS=0").contains("MAX_SECONDS"));
        // A non-whole VAD_SILENCE_MS is as loud as a non-number.
        assert!(err("API_KEY=k\nVAD_SILENCE_MS=3000.5").contains("VAD_SILENCE_MS"));
        // The RMS threshold's documented range is enforced, not just its
        // numeric-ness. Each of these parses as an f64 and then breaks every
        // dictation silently: NaN/inf make every chunk read as silence (so
        // "No speech detected" at 10 s, forever), a negative makes digital
        // silence read as speech (so only MAX_SECONDS ever ends a session).
        // `1e400` is in the list because it parses to +inf rather than failing.
        for bad in ["-400", "-inf", "NaN", "inf", "1e400", "40000"] {
            let e = err(&format!("API_KEY=k\nVAD_RMS_THRESHOLD={bad}"));
            assert!(
                e.contains("VAD_RMS_THRESHOLD"),
                "{bad} must be refused by name, got: {e}"
            );
        }
        // The two VAD zeroes stay legal (documented "always"/"first tick" meanings).
        let cfg = load_from("API_KEY=k\nVAD_SILENCE_MS=0\nVAD_RMS_THRESHOLD=0");
        assert_eq!(cfg.vad_silence_ms, 0);
        assert_eq!(cfg.vad_rms_threshold, 0.0);
        // Clean numbers still land, and blank still means default.
        let cfg = load_from("API_KEY=k\nMAX_SECONDS=45\nVAD_RMS_THRESHOLD=250.5");
        assert_eq!(cfg.max_seconds, 45);
        assert_eq!(cfg.vad_rms_threshold, 250.5);
        let cfg = load_from("API_KEY=k");
        assert_eq!(cfg.max_seconds, DEFAULT_MAX_SECONDS);
    }

    #[test]
    fn a_quoted_value_keeps_a_spaced_hash() {
        // The comment scan ran before the quote strip, so `KEYWORDS="k8s #cluster"`
        // lost ` #cluster` to the comment rule and then failed the quote match,
        // leaving a lone leading quote in the value. A quoted value is quoted
        // precisely so its content is not reinterpreted.
        let cfg = load_from("API_KEY=k\nKEYWORDS=\"k8s #cluster\"");
        assert_eq!(
            cfg.keywords,
            vec!["k8s #cluster"],
            "a # inside quotes is data, and both quotes are gone"
        );
        // Unquoted behaviour is unchanged.
        let cfg = load_from("API_KEY=k\nMODEL=nova-3  # my model");
        assert_eq!(cfg.model, "nova-3", "an unquoted spaced # is still a comment");
    }
}
