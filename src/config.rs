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
    pub orb_color: (f32, f32, f32),
    pub orb_fluid_level: f32,
    pub hotkey: (u32, u32),       // (modifiers, vk)
    pub hotkey_str: String,
    pub cancel_key: (u32, u32),   // (modifiers, vk)
    pub cancel_key_str: String,
}

pub fn load() -> Result<Config, String> {
    let mut protocol_str = String::new();
    let mut api_key = String::new();
    let mut model = String::new();
    let mut language = String::new();
    let mut base_url = String::new();
    let mut max_seconds = 120u32;
    let mut trailing_space = true;
    let mut keywords: Vec<String> = Vec::new();
    let mut orb_color_str = String::new();
    let mut orb_fluid_str = String::new();
    let mut hotkey_str = String::new();
    let mut cancel_key_str = String::new();

    // Check for keywords.txt beside the executable
    if let Ok(exe) = std::env::current_exe() {
        for filename in ["keywords.txt", "vocabulary.txt", "words.txt"] {
            let path = exe.with_file_name(filename);
            if let Ok(text) = std::fs::read_to_string(&path) {
                parse_keywords_text(&text, &mut keywords);
            }
        }
    }

    // Lowest priority: mnvoice.env beside the executable.
    if let Ok(exe) = std::env::current_exe() {
        let path = exe.with_file_name("mnvoice.env");
        if let Ok(text) = std::fs::read_to_string(&path) {
            parse(&text, |k, v| {
                apply(
                    k,
                    v,
                    &mut protocol_str,
                    &mut api_key,
                    &mut model,
                    &mut language,
                    &mut base_url,
                    &mut max_seconds,
                    &mut trailing_space,
                    &mut keywords,
                    &mut orb_color_str,
                    &mut orb_fluid_str,
                    &mut hotkey_str,
                    &mut cancel_key_str,
                )
            });
        }
    }

    // Highest priority: real environment variables.
    if let Ok(v) = std::env::var("PROTOCOL").or_else(|_| std::env::var("MODE")).or_else(|_| std::env::var("PROVIDER")) {
        protocol_str = v;
    }
    if let Ok(v) = std::env::var("API_KEY")
        .or_else(|_| std::env::var("DEEPGRAM_API_KEY"))
        .or_else(|_| std::env::var("GROQ_API_KEY"))
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
    {
        api_key = v;
    }

    if let Ok(v) = std::env::var("MODEL")
        .or_else(|_| std::env::var("DEEPGRAM_MODEL"))
        .or_else(|_| std::env::var("GROQ_MODEL"))
        .or_else(|_| std::env::var("OPENAI_MODEL"))
    {
        model = v;
    }
    if let Ok(v) = std::env::var("LANGUAGE")
        .or_else(|_| std::env::var("DEEPGRAM_LANGUAGE"))
        .or_else(|_| std::env::var("GROQ_LANGUAGE"))
    {
        language = v;
    }
    if let Ok(v) = std::env::var("BASE_URL")
        .or_else(|_| std::env::var("DEEPGRAM_BASE_URL"))
        .or_else(|_| std::env::var("GROQ_BASE_URL"))
        .or_else(|_| std::env::var("ENDPOINT"))
    {
        base_url = v;
    }
    if let Ok(v) = std::env::var("MAX_SECONDS") {
        if let Ok(n) = v.parse() { max_seconds = n; }
    }
    if let Ok(v) = std::env::var("TRAILING_SPACE") {
        trailing_space = v != "0";
    }
    if let Ok(v) = std::env::var("KEYWORDS").or_else(|_| std::env::var("KEYTERMS")) {
        parse_keywords_text(&v, &mut keywords);
    }
    if let Ok(v) = std::env::var("ORB_COLOR").or_else(|_| std::env::var("ORB_HEX")) {
        orb_color_str = v;
    }
    if let Ok(v) = std::env::var("ORB_FLUID_LEVEL").or_else(|_| std::env::var("ORB_FLUID_AMOUNT")) {
        orb_fluid_str = v;
    }
    if let Ok(v) = std::env::var("HOTKEY").or_else(|_| std::env::var("TRIGGER_HOTKEY")) {
        hotkey_str = v;
    }
    if let Ok(v) = std::env::var("CANCEL_KEY").or_else(|_| std::env::var("CANCEL_HOTKEY")) {
        cancel_key_str = v;
    }

    // Determine protocol: streaming vs rest
    let protocol = match protocol_str.to_lowercase().as_str() {
        "rest" | "http" | "batch" | "groq" | "openai" => Protocol::Rest,
        "streaming" | "stream" | "websocket" | "ws" | "deepgram" => Protocol::Streaming,
        _ => {
            if base_url.contains("groq.com") || base_url.contains("openai.com") || model.contains("whisper") {
                Protocol::Rest
            } else {
                Protocol::Streaming
            }
        }
    };

    if api_key.trim().is_empty() {
        return Err("No API key configured. Set API_KEY in mnvoice.env next to mnvoice.exe.".into());
    }

    if model.is_empty() {
        model = match protocol {
            Protocol::Streaming => "nova-3".into(),
            Protocol::Rest => "whisper-large-v3-turbo".into(),
        };
    }
    if base_url.is_empty() {
        base_url = match protocol {
            Protocol::Streaming => "https://api.deepgram.com".into(),
            Protocol::Rest => "https://api.groq.com".into(),
        };
    }
    if language.is_empty() {
        language = "en".into();
    }

    let orb_color = parse_color(&orb_color_str);
    let orb_fluid_level = parse_fluid_level(&orb_fluid_str);

    let hotkey_actual_str = if hotkey_str.trim().is_empty() {
        "Alt+Space".to_string()
    } else {
        hotkey_str.trim().to_string()
    };
    let hotkey = parse_hotkey(&hotkey_actual_str).unwrap_or((0x0001 | 0x4000, 0x20)); // MOD_ALT | MOD_NOREPEAT, VK_SPACE

    let cancel_key_actual_str = if cancel_key_str.trim().is_empty() {
        "Escape".to_string()
    } else {
        cancel_key_str.trim().to_string()
    };
    let cancel_key = parse_hotkey(&cancel_key_actual_str).unwrap_or((0x4000, 0x1B)); // MOD_NOREPEAT, VK_ESCAPE

    Ok(Config {
        protocol,
        api_key,
        model,
        language,
        base_url,
        max_seconds,
        trailing_space,
        keywords,
        orb_color,
        orb_fluid_level,
        hotkey,
        hotkey_str: hotkey_actual_str,
        cancel_key,
        cancel_key_str: cancel_key_actual_str,
    })
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

#[allow(clippy::too_many_arguments)]
fn apply(
    k: &str,
    v: &str,
    protocol_str: &mut String,
    api_key: &mut String,
    model: &mut String,
    language: &mut String,
    base_url: &mut String,
    max_seconds: &mut u32,
    trailing_space: &mut bool,
    keywords: &mut Vec<String>,
    orb_color_str: &mut String,
    orb_fluid_str: &mut String,
    hotkey_str: &mut String,
    cancel_key_str: &mut String,
) {
    match k {
        "PROTOCOL" | "MODE" | "PROVIDER" => *protocol_str = v.to_string(),
        "API_KEY" | "DEEPGRAM_API_KEY" | "GROQ_API_KEY" | "OPENAI_API_KEY" => {
            if api_key.is_empty() || k == "API_KEY" {
                *api_key = v.to_string();
            }
        }
        "MODEL" | "DEEPGRAM_MODEL" | "GROQ_MODEL" | "OPENAI_MODEL" => {
            if model.is_empty() || k == "MODEL" {
                *model = v.to_string();
            }
        }
        "LANGUAGE" | "DEEPGRAM_LANGUAGE" | "GROQ_LANGUAGE" => {
            if language.is_empty() || k == "LANGUAGE" {
                *language = v.to_string();
            }
        }
        "BASE_URL" | "DEEPGRAM_BASE_URL" | "GROQ_BASE_URL" | "ENDPOINT" => {
            if base_url.is_empty() || k == "BASE_URL" {
                *base_url = v.to_string();
            }
        }
        "MAX_SECONDS" => {
            if let Ok(n) = v.parse() { *max_seconds = n; }
        }
        "TRAILING_SPACE" => *trailing_space = v != "0",
        "KEYWORDS" | "KEYTERMS" | "CUSTOM_WORDS" | "VOCABULARY" => {
            parse_keywords_text(v, keywords);
        }
        "ORB_COLOR" | "ORB_HEX" | "COLOR" => *orb_color_str = v.to_string(),
        "ORB_FLUID_LEVEL" | "ORB_FLUID_AMOUNT" | "FLUID_LEVEL" => *orb_fluid_str = v.to_string(),
        "HOTKEY" | "TRIGGER_HOTKEY" | "KEYBIND" => *hotkey_str = v.to_string(),
        "CANCEL_KEY" | "CANCEL_HOTKEY" => *cancel_key_str = v.to_string(),
        _ => {}
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
}
