// The Linux and macOS control surface.
//
// Windows drives sessions from a tray hotkey; here the terminal plays that
// role. Everything that makes a dictation work - capture engine, streaming
// transport, word typing into the focused window - is the same seam code
// Windows uses. Enter starts, the voice-activity detector or Enter stops.

use crate::config;
use crate::platform::{audio, input};
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

pub fn run() -> Result<(), String> {
    let cfg = config::load().map_err(|e| {
        format!(
            "{e} (put a mnvoice.env file with your API key next to the binary, \
             or export API_KEY)"
        )
    })?;

    let engine = crate::platform::native::audio()?;

    println!("mnvoice v{}", crate::platform::version());
    println!(
        "protocol: {:?}, model: {}, language: {}, keywords: {}",
        cfg.protocol,
        cfg.model,
        cfg.language,
        cfg.keywords.len()
    );
    match cfg.protocol {
        config::Protocol::Streaming => {
            println!("words are typed into the focused window as they are recognised")
        }
        config::Protocol::Rest => println!("full transcript is typed after each dictation"),
    }
    println!();
    println!("Press Enter to start a dictation. Enter again to stop early;");
    println!("{}s of silence or {}s total also stops it.", 300, 120);
    println!("Ctrl+C quits.");
    println!();

    let stdin = std::io::stdin();
    for _ in stdin.lock().lines() {
        dictate(&cfg, &engine)?;
        println!();
        println!("Press Enter for the next dictation.");
    }
    Ok(())
}

fn dictate(cfg: &config::Config, engine: &dyn audio::Audio) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let done = engine.capture_to_channel(
        Arc::clone(&stop),
        cfg.max_seconds,
        cfg.vad_silence_ms,
        cfg.vad_rms_threshold,
        tx,
    )?;

    // The stop arm doubles as the "Enter again" control: a reader thread turns
    // a second line of input into the same stop signal the VAD would raise.
    let stop_reader = Arc::clone(&stop);
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            stop_reader.store(true, Ordering::SeqCst);
        }
    });

    println!("recording... (Enter to stop)");

    let mut samples: Vec<i16> = Vec::new();
    let mut ended: Option<Result<(), String>> = None;
    while ended.is_none() {
        while let Ok(chunk) = rx.try_recv() {
            samples.extend_from_slice(&chunk);
        }
        if let Ok(result) = done.try_recv() {
            ended = Some(result);
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    while let Ok(chunk) = rx.try_recv() {
        samples.extend_from_slice(&chunk);
    }
    let _ = reader.join();
    if let Some(Err(e)) = ended {
        return Err(e);
    }

    println!("stopped ({}s of audio)", samples.len() / 16_000);

    let text = match cfg.protocol {
        config::Protocol::Streaming => {
            let cancelled = Arc::new(AtomicBool::new(false));
            // The channel is already drained; the stream is given the whole
            // clip at once and the loop types as frames arrive, exactly as on
            // Windows.
            let (tx, rx) = mpsc::channel();
            for chunk in samples.chunks(640) {
                let _ = tx.send(chunk.to_vec());
            }
            drop(tx);
            let typed = crate::stream::run_stream(cfg, &stop, &cancelled, rx);
            typed?
        }
        config::Protocol::Rest => {
            let wav = audio::wav_bytes(&samples);
            let text = crate::rest::transcribe(cfg, &wav)?;
            input::type_text(&text);
            text
        }
    };

    if text.is_empty() {
        println!("(no speech detected)");
    } else {
        println!("transcript: {text}");
    }
    Ok(())
}