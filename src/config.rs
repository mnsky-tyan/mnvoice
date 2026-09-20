// Configuration: environment variables first, then mnvoice.env next to the exe.

#[derive(Clone)]
pub struct Config {
    pub api_key: String,
    pub model: String,
    pub language: String,
    pub base_url: String,
    pub max_seconds: u32,
    pub trailing_space: bool,
}

pub fn load() -> Result<Config, String> {
    let mut api_key = String::new();
    let mut model = String::from("whisper-large-v3-turbo");
    let mut language = String::from("en");
    let mut base_url = String::from("https://api.groq.com");
    let mut max_seconds = 120u32;
    let mut trailing_space = true;

    // Lowest priority: mnvoice.env beside the executable.
    if let Ok(exe) = std::env::current_exe() {
        let path = exe.with_file_name("mnvoice.env");
        if let Ok(text) = std::fs::read_to_string(&path) {
            parse(&text, |k, v| apply(k, v, &mut api_key, &mut model, &mut language, &mut base_url, &mut max_seconds, &mut trailing_space));
        }
    }
    // Highest priority: real environment variables.
    if let Ok(v) = std::env::var("GROQ_API_KEY") { api_key = v; }
    if let Ok(v) = std::env::var("GROQ_MODEL") { model = v; }
    if let Ok(v) = std::env::var("GROQ_LANGUAGE") { language = v; }
    if let Ok(v) = std::env::var("GROQ_BASE_URL") { base_url = v; }
    if let Ok(v) = std::env::var("GROQ_MAX_SECONDS") {
        if let Ok(n) = v.parse() { max_seconds = n; }
    }
    if let Ok(v) = std::env::var("GROQ_TRAILING_SPACE") { trailing_space = v != "0"; }

    if api_key.trim().is_empty() {
        return Err("GROQ_API_KEY is not set. Put it in mnvoice.env next to mnvoice.exe or in the environment.".into());
    }
    Ok(Config { api_key, model, language, base_url, max_seconds, trailing_space })
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
fn apply(k: &str, v: &str, api_key: &mut String, model: &mut String, language: &mut String, base_url: &mut String, max_seconds: &mut u32, trailing_space: &mut bool) {
    match k {
        "GROQ_API_KEY" => *api_key = v.to_string(),
        "GROQ_MODEL" => *model = v.to_string(),
        "GROQ_LANGUAGE" => *language = v.to_string(),
        "GROQ_BASE_URL" => *base_url = v.to_string(),
        "GROQ_MAX_SECONDS" => { if let Ok(n) = v.parse() { *max_seconds = n; } }
        "GROQ_TRAILING_SPACE" => *trailing_space = v != "0",
        _ => {}
    }
}
