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
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

/// The packet size the streaming loop and the provider expect, in samples.
const PACKET_SAMPLES: usize = 640;

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
    println!(
        "{}ms of silence or {}s total also stops it.",
        cfg.vad_silence_ms, cfg.max_seconds
    );
    println!("Ctrl+C quits.");
    println!();

    // Stdin's lock is process-wide and not reentrant, so it gets exactly one
    // owner: this thread holds it for the life of the process and hands the
    // main loop a line at a time. A dictation therefore never runs with the
    // lock held, and the "Enter again" control reads through the same channel.
    let (line_tx, lines) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for _ in stdin.lock().lines() {
            if line_tx.send(()).is_err() {
                break;
            }
        }
    });

    loop {
        // A closed stdin ends the session instead of spinning on Enter.
        if lines.recv().is_err() {
            break;
        }
        dictate(&cfg, &engine, &lines)?;
        println!();
        println!("Press Enter for the next dictation.");
    }
    Ok(())
}

fn dictate(
    cfg: &config::Config,
    engine: &dyn audio::Audio,
    lines: &Receiver<()>,
) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let done = engine.capture_to_channel(
        Arc::clone(&stop),
        cfg.max_seconds,
        cfg.vad_silence_ms,
        cfg.vad_rms_threshold,
        tx,
    )?;

    println!("recording... (Enter to stop)");

    let mut samples: Vec<i16> = Vec::new();
    let mut ended: Option<Result<(), String>> = None;
    while ended.is_none() {
        // The "Enter again" control: the next line from the single reader
        // thread raises the same stop signal the VAD would.
        if lines.try_recv().is_ok() {
            stop.store(true, Ordering::SeqCst);
        }
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
    if let Some(Err(e)) = ended {
        return Err(e);
    }

    println!("stopped ({}s of audio)", samples.len() / 16_000);

    let text = match cfg.protocol {
        config::Protocol::Streaming => {
            let cancelled = Arc::new(AtomicBool::new(false));
            // The clip is handed to the streaming loop in real time, the way
            // the Windows engine feeds it from the live microphone: one 40 ms
            // packet every 40 ms. Emptying the whole clip into the channel at
            // once instead would leave the provider transcribing a backlog
            // long after the final-transcript wait had expired, and the tail
            // of the dictation would be dropped from the transcript.
            let (tx, rx) = mpsc::channel();
            let packet_ms = (PACKET_SAMPLES as u64 * 1000) / crate::platform::audio::SAMPLE_RATE as u64;
            std::thread::spawn(move || {
                for chunk in samples.chunks(PACKET_SAMPLES) {
                    // A failed send means the stream is finished and its
                    // receiver is gone, which is the only early exit here.
                    if tx.send(chunk.to_vec()).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(packet_ms));
                }
            });
            // The capture's own stop flag is spent by the time the clip is
            // recorded; the streaming loop drives this one itself.
            let sending = Arc::new(AtomicBool::new(false));
            let typed = crate::stream::run_stream(cfg, &sending, &cancelled, rx);
            typed?
        }
        config::Protocol::Rest => {
            let wav = audio::wav_bytes(&samples);
            let raw = crate::rest::transcribe(cfg, &wav)?;
            // REST has no provider-side filler parameter, so the local
            // stoplist is the only filter (see strip_disfluencies).
            let text = crate::rest::strip_disfluencies(&raw);
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