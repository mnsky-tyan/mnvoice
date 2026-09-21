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

    Ok(Config {
        protocol,
        api_key,
        model,
        language,
        base_url,
        max_seconds,
        trailing_space,
        keywords,
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
        _ => {}
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
}
