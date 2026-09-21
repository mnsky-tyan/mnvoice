// Configuration: environment variables first, then mnvoice.env next to the exe,
// plus optional keywords.txt / vocabulary.txt for custom terminology.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Deepgram,
    Groq,
}

#[derive(Clone)]
pub struct Config {
    pub provider: Provider,
    pub api_key: String,
    pub model: String,
    pub language: String,
    pub base_url: String,
    pub max_seconds: u32,
    pub trailing_space: bool,
    pub keywords: Vec<String>,
}

pub fn load() -> Result<Config, String> {
    let mut provider_str = String::new();
    let mut deepgram_key = String::new();
    let mut groq_key = String::new();
    let mut generic_key = String::new();

    let mut model = String::new();
    let mut language = String::new();
    let mut base_url = String::new();
    let mut max_seconds = 120u32;
    let mut trailing_space = true;
    let mut keywords: Vec<String> = Vec::new();

    // Check for keywords.txt / vocabulary.txt beside the executable
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
                    &mut provider_str,
                    &mut deepgram_key,
                    &mut groq_key,
                    &mut generic_key,
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
    if let Ok(v) = std::env::var("PROVIDER") { provider_str = v; }
    if let Ok(v) = std::env::var("DEEPGRAM_API_KEY") { deepgram_key = v; }
    if let Ok(v) = std::env::var("GROQ_API_KEY") { groq_key = v; }
    if let Ok(v) = std::env::var("API_KEY") { generic_key = v; }

    if let Ok(v) = std::env::var("MODEL").or_else(|_| std::env::var("DEEPGRAM_MODEL")).or_else(|_| std::env::var("GROQ_MODEL")) {
        model = v;
    }
    if let Ok(v) = std::env::var("LANGUAGE").or_else(|_| std::env::var("DEEPGRAM_LANGUAGE")).or_else(|_| std::env::var("GROQ_LANGUAGE")) {
        language = v;
    }
    if let Ok(v) = std::env::var("BASE_URL").or_else(|_| std::env::var("DEEPGRAM_BASE_URL")).or_else(|_| std::env::var("GROQ_BASE_URL")) {
        base_url = v;
    }
    if let Ok(v) = std::env::var("MAX_SECONDS").or_else(|_| std::env::var("GROQ_MAX_SECONDS")) {
        if let Ok(n) = v.parse() { max_seconds = n; }
    }
    if let Ok(v) = std::env::var("TRAILING_SPACE").or_else(|_| std::env::var("GROQ_TRAILING_SPACE")) {
        trailing_space = v != "0";
    }
    if let Ok(v) = std::env::var("KEYWORDS").or_else(|_| std::env::var("KEYTERMS")) {
        parse_keywords_text(&v, &mut keywords);
    }

    // Determine provider
    let provider = match provider_str.to_lowercase().as_str() {
        "deepgram" => Provider::Deepgram,
        "groq" => Provider::Groq,
        _ => {
            if !deepgram_key.trim().is_empty() {
                Provider::Deepgram
            } else if !groq_key.trim().is_empty() {
                Provider::Groq
            } else {
                Provider::Deepgram
            }
        }
    };

    let api_key = match provider {
        Provider::Deepgram => {
            if !deepgram_key.trim().is_empty() {
                deepgram_key
            } else {
                generic_key
            }
        }
        Provider::Groq => {
            if !groq_key.trim().is_empty() {
                groq_key
            } else {
                generic_key
            }
        }
    };

    if api_key.trim().is_empty() {
        return Err("No API key configured. Set DEEPGRAM_API_KEY (or GROQ_API_KEY) in mnvoice.env next to mnvoice.exe.".into());
    }

    // Apply defaults based on provider if not explicitly overridden
    if model.is_empty() {
        model = match provider {
            Provider::Deepgram => "nova-3".into(),
            Provider::Groq => "whisper-large-v3-turbo".into(),
        };
    }
    if base_url.is_empty() {
        base_url = match provider {
            Provider::Deepgram => "https://api.deepgram.com".into(),
            Provider::Groq => "https://api.groq.com".into(),
        };
    }
    if language.is_empty() {
        language = "en".into();
    }

    Ok(Config {
        provider,
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
    provider: &mut String,
    deepgram_key: &mut String,
    groq_key: &mut String,
    generic_key: &mut String,
    model: &mut String,
    language: &mut String,
    base_url: &mut String,
    max_seconds: &mut u32,
    trailing_space: &mut bool,
    keywords: &mut Vec<String>,
) {
    match k {
        "PROVIDER" => *provider = v.to_string(),
        "DEEPGRAM_API_KEY" => *deepgram_key = v.to_string(),
        "GROQ_API_KEY" => *groq_key = v.to_string(),
        "API_KEY" => *generic_key = v.to_string(),

        "MODEL" | "DEEPGRAM_MODEL" | "GROQ_MODEL" => *model = v.to_string(),
        "LANGUAGE" | "DEEPGRAM_LANGUAGE" | "GROQ_LANGUAGE" => *language = v.to_string(),
        "BASE_URL" | "DEEPGRAM_BASE_URL" | "GROQ_BASE_URL" => *base_url = v.to_string(),

        "MAX_SECONDS" | "GROQ_MAX_SECONDS" => {
            if let Ok(n) = v.parse() { *max_seconds = n; }
        }
        "TRAILING_SPACE" | "GROQ_TRAILING_SPACE" => *trailing_space = v != "0",
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
