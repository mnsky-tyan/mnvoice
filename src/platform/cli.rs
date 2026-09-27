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

/// The chunk size the Windows engine pushes, in samples: 40 ms at the
/// provider rate. Purely a granularity choice here, nothing depends on it.
const PACKET_SAMPLES: usize = 640;

/// Queues a recorded clip for the streaming loop.
///
/// Every sample is in the channel and the sender is gone before the stream
/// opens, so the loop's drain phase hands the provider the whole clip. That
/// ordering is the point: the loop stops sending the moment the provider
/// reports the end of an utterance, and its drain phase only flushes what is
/// already queued, so a clip still being fed at that point loses everything
/// after the first utterance.
fn queue_clip(samples: &[i16]) -> Receiver<Vec<i16>> {
    let (tx, rx) = mpsc::channel();
    for chunk in samples.chunks(PACKET_SAMPLES) {
        let _ = tx.send(chunk.to_vec());
    }
    drop(tx);
    rx
}

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
            // Capture is already drained, so the clip is queued whole and the
            // loop types as frames arrive, exactly as on Windows.
            let rx = queue_clip(&samples);
            let typed = crate::stream::run_stream(cfg, &stop, &cancelled, rx);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The streaming loop stops sending as soon as the provider reports the end
    /// of an utterance, and its drain phase only flushes packets that are
    /// already queued. A clip still being fed at that point loses everything
    /// after the first utterance - a pause of 1.5 s is enough for the provider
    /// to end an utterance while the CLI's own silence detector is still
    /// recording - so the whole clip has to be queued, and the sender dropped,
    /// before the stream opens.
    #[test]
    fn the_whole_clip_is_queued_before_the_stream_opens() {
        // Three seconds of audio with a two second pause in the middle: the
        // shape that truncated.
        let mut clip: Vec<i16> = Vec::new();
        clip.extend(std::iter::repeat(2_000).take(16_000));
        clip.extend(std::iter::repeat(0).take(16_000 * 2));
        clip.extend(std::iter::repeat(3_000).take(16_000));

        let rx = queue_clip(&clip);

        // Nothing may block: the loop's send phase is either skipped or ends on
        // disconnect, and its drain phase is a non-blocking poll.
        let mut queued: Vec<i16> = Vec::new();
        while let Ok(packet) = rx.try_recv() {
            queued.extend_from_slice(&packet);
        }

        assert_eq!(queued, clip);
    }

    /// The sender must be gone once the clip is queued, or the loop's send
    /// phase waits out its 250 ms poll timeout per packet instead of ending on
    /// disconnect.
    #[test]
    fn queuing_a_clip_leaves_the_channel_disconnected() {
        let clip: Vec<i16> = (0..PACKET_SAMPLES * 3).map(|i| i as i16).collect();
        let rx = queue_clip(&clip);
        while rx.try_recv().is_ok() {}
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }
}