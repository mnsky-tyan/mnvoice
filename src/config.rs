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
    /// Install a newer published release automatically when one appears.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub auto_update: bool,
}

pub fn load() -> Result<Config, String> {
    let mut raw = RawFields::new();

    // Check for keywords.txt beside the executable
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
            parse(&text, |k, v| {
                if let Some((_, field)) = FIELDS.iter().find(|(names, _)| names.contains(&k)) {
                    raw.set(*field, k, v, true);
                }
            });
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
            if raw.base_url.contains("groq.com")
                || raw.base_url.contains("openai.com")
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

    let mut model = raw.model;
    if model.is_empty() {
        model = match protocol {
            Protocol::Streaming => "nova-3".into(),
            Protocol::Rest => "whisper-large-v3-turbo".into(),
        };
    }
    let mut base_url = raw.base_url;
    if base_url.is_empty() {
        base_url = match protocol {
            Protocol::Streaming => "https://api.deepgram.com".into(),
            Protocol::Rest => "https://api.groq.com".into(),
        };
    }
    let mut language = raw.language;
    if language.is_empty() {
        language = "en".into();
    }

    let orb_color = parse_color(&raw.orb_color_str);
    let orb_fluid_level = parse_fluid_level(&raw.orb_fluid_str);

    // The displayed spelling must name the pair that was actually registered:
    // an unparseable HOTKEY falls back to the default below, so echoing
    // "Foobar" back into the tray tip and the log would advertise a key that
    // does nothing.
    let hotkey_input = raw.hotkey_str.trim().to_string();
    let hotkey_parsed = parse_hotkey(&hotkey_input);
    let hotkey = hotkey_parsed.unwrap_or((0x0001 | 0x4000, 0x20)); // MOD_ALT | MOD_NOREPEAT, VK_SPACE
    let hotkey_actual_str = match (hotkey_input.is_empty(), hotkey_parsed) {
        (false, Some(_)) => hotkey_input,
        _ => "Alt+Space".to_string(),
    };

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

    // AUTO_UPDATE is read by auto_update_enabled() above, not from this pass.

    Ok(Config {
        protocol,
        api_key: raw.api_key,
        model,
        language,
        base_url,
        max_seconds: raw.max_seconds,
        trailing_space: raw.trailing_space,
        keywords: raw.keywords,
        orb_color,
        orb_fluid_level,
        hotkey,
        hotkey_str: hotkey_actual_str,
        cancel_key,
        cancel_key_str: cancel_actual_str,
        vad_silence_ms: raw.vad_silence_ms,
        vad_rms_threshold: raw.vad_rms_threshold,
        strip_fillers,
        // Deliberately not parsed inline below: a broken API key aborts load(),
        // and if AUTO_UPDATE were derived from the same pass the updater would
        // silently go dark exactly when the config is least trustworthy.
        auto_update: auto_update_enabled(),
    })
}

/// Whether `AUTO_UPDATE` asks for automatic installs, read without depending on
/// the rest of the config being valid. `load()` fails outright on a missing API
/// key, so this is what keeps a typo'd config from also silencing updates.
pub fn auto_update_enabled() -> bool {
    let mut raw = auto_update_from_file();
    // The real environment wins over the file beside the exe, matching load().
    if let Ok(v) = std::env::var("AUTO_UPDATE") {
        raw = Some(v);
    }
    raw.as_deref().map(parse_auto_update).unwrap_or(false)
}

/// The `AUTO_UPDATE` value from `mnvoice.env` beside the executable, if it names
/// one. Last entry wins so a later correction overrides an earlier one.
fn auto_update_from_file() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let text = std::fs::read_to_string(exe.with_file_name("mnvoice.env")).ok()?;
    parse_auto_update_text(&text)
}

/// The `AUTO_UPDATE` value out of an env file body, or `None` if it never names
/// one. `Some("")` means the key was present but empty, which is "off".
pub fn parse_auto_update_text(text: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case("AUTO_UPDATE") {
                found = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    found
}

/// Whether an `AUTO_UPDATE` value asks for automatic installs. Opt-in, so an
/// empty string, a typo and every other spelling all mean "off".
pub fn parse_auto_update(s: &str) -> bool {
    matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

pub fn parse_keywords_text(text: &str, keywords: &mut Vec<String>) {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        for part in line.split(',') {
            let word = part.trim();
            if !word.is_empty() && !keywords.iter().any(|k| k.eq_ignore_ascii_case(word)) {
                keywords.push(word.to_string());
            }
        }
    }
}

fn parse<F: FnMut(&str, &str)>(text: &str, mut f: F) {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            f(k.trim(), v);
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
    max_seconds: u32,
    trailing_space: bool,
    keywords: Vec<String>,
    orb_color_str: String,
    orb_fluid_str: String,
    hotkey_str: String,
    cancel_key_str: String,
    vad_silence_ms: u32,
    vad_rms_threshold: f64,
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
            max_seconds: 120,
            trailing_space: true,
            keywords: Vec::new(),
            orb_color_str: String::new(),
            orb_fluid_str: String::new(),
            hotkey_str: String::new(),
            cancel_key_str: String::new(),
            vad_silence_ms: 3000,
            vad_rms_threshold: 400.0,
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
    /// line that names it. The environment pass runs after the file, so it
    /// overrides anything the file set and is not subject to the guards.
    fn set(&mut self, field: Field, k: &str, v: &str, from_file: bool) {
        let canonical_overrides = from_file && k == field.canonical();
        match field {
            Field::Protocol => self.protocol_str = v.to_string(),
            Field::ApiKey if !from_file || canonical_overrides || self.api_key.is_empty() => {
                self.api_key = v.to_string()
            }
            Field::Model if !from_file || canonical_overrides || self.model.is_empty() => {
                self.model = v.to_string()
            }
            Field::Language if !from_file || canonical_overrides || self.language.is_empty() => {
                self.language = v.to_string()
            }
            Field::BaseUrl if !from_file || canonical_overrides || self.base_url.is_empty() => {
                self.base_url = v.to_string()
            }
            Field::OrbColor => self.orb_color_str = v.to_string(),
            Field::OrbFluid => self.orb_fluid_str = v.to_string(),
            Field::Hotkey => self.hotkey_str = v.to_string(),
            Field::CancelKey => self.cancel_key_str = v.to_string(),
            Field::FillerWords => self.filler_words_str = v.to_string(),
            Field::MaxSeconds => {
                if let Ok(n) = v.parse() {
                    self.max_seconds = n;
                }
            }
            // Deliberately the one key where an unknown spelling keeps the
            // default on: TRAILING_SPACE defaults to enabled, and treating a
            // typo as "off" would let `TRAILING_SPACE=flase` silently strip
            // the space. Only the exact spelling 0 disables.
            Field::TrailingSpace => self.trailing_space = v != "0",
            Field::Keywords => parse_keywords_text(v, &mut self.keywords),
            Field::VadSilenceMs => {
                if let Ok(n) = v.parse() {
                    self.vad_silence_ms = n;
                }
            }
            Field::VadRmsThreshold => {
                if let Ok(n) = v.parse() {
                    self.vad_rms_threshold = n;
                }
            }
            // The alias guards leave the four guarded variants uncovered when
            // a guard fails, so this fallback is what makes the match legal -
            // not dead code.
            _ => {}
        }
    }
}

/// The cancel key to register. `none` means disabled - a zero virtual key,
/// which the hotkey registration reads as "register nothing" - and anything
/// else parses as a hotkey, falling back to the documented Escape default
/// only when the spelling matches nothing.
pub fn resolve_cancel_key(s: &str) -> (u32, u32) {
    cancel_key_with_display(s).0
}

/// The cancel key to register plus the spelling to show for it, decided in
/// one place: the display must never advertise a key other than the pair
/// that was registered, and the fallbacks live here rather than being
/// re-derived by each caller.
fn cancel_key_with_display(s: &str) -> ((u32, u32), String) {
    let s = s.trim();
    if s.is_empty() {
        return ((0x4000, 0x1B), "Escape".to_string());
    }
    if s.eq_ignore_ascii_case("none") {
        return ((0, 0), s.to_string());
    }
    match parse_hotkey(s) {
        Some(pair) => (pair, s.to_string()),
        None => ((0x4000, 0x1B), "Escape".to_string()),
    }
}

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
        "pink" | "hot_pink" | "magenta" => (1.0, 0.18, 0.58),
        _ => {
            let hex = s.trim_start_matches('#');
            if hex.len() == 6 {
                if let (Ok(r), Ok(g), Ok(b)) = (
                    u8::from_str_radix(&hex[0..2], 16),
                    u8::from_str_radix(&hex[2..4], 16),
                    u8::from_str_radix(&hex[4..6], 16),
                ) {
                    return (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
                }
            }
            // Default vibrant hot pink
            (1.0, 0.18, 0.58)
        }
    }
}

/// Parses fluid level string into float in range `0.05..=1.0`.
pub fn parse_fluid_level(s: &str) -> f32 {
    let s = s.trim().trim_end_matches('%');
    if let Ok(val) = s.parse::<f32>() {
        let val = if val > 1.0 { val / 100.0 } else { val };
        val.clamp(0.05, 1.0)
    } else {
        0.75 // default
    }
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
                if let Some(f_num) = other.strip_prefix('f') {
                    if let Ok(num) = f_num.parse::<u32>() {
                        if (1..=24).contains(&num) {
                            vk = 0x70 + (num - 1); // VK_F1 = 0x70
                        }
                    }
                } else if other.len() == 1 {
                    let ch = other.chars().next().unwrap().to_ascii_uppercase();
                    if ch.is_ascii_alphanumeric() {
                        vk = ch as u32;
                    }
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

    #[test]
    fn test_parse_color() {
        assert_eq!(parse_color("cyan"), (0.0, 0.95, 0.90));
        assert_eq!(parse_color("purple"), (0.68, 0.25, 0.98));
        assert_eq!(parse_color("emerald"), (0.12, 0.85, 0.45));
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
    }

    #[test]
    fn cancel_key_none_disables_instead_of_rearming_escape() {
        // The docs promise "none" disables the cancel key. The old fallback
        // silently re-registered Escape, so a user who asked for none still
        // lost transcripts to a stray Escape press.
        assert_eq!(resolve_cancel_key("none"), (0, 0));
        assert_eq!(resolve_cancel_key("NONE"), (0, 0));
        assert_eq!(resolve_cancel_key("Escape"), (0x4000, 0x1B));
        assert_eq!(resolve_cancel_key("unparseable"), (0x4000, 0x1B), "unknown spelling falls back to the default");
    }

    #[test]
    fn every_field_has_a_unique_canonical_name() {
        for (names, _) in FIELDS {
            let (canonical, aliases) = names.split_first().expect("every field needs a canonical name");
            assert!(!aliases.contains(canonical), "{canonical} must appear exactly once");
        }
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
        parse(text, |k, v| {
            if let Some((_, field)) = FIELDS.iter().find(|(names, _)| names.contains(&k)) {
                raw.set(*field, k, v, true);
            }
        });
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
        assert_eq!(cfg.cancel_key, (0x4000, 0x1B));
        assert_eq!(cfg.cancel_key_str, "Escape");
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
}
