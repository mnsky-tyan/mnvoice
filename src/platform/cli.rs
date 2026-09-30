// The Linux and macOS control surface.
//
// Windows drives sessions from a tray hotkey; here the terminal plays that
// role. Everything that makes a dictation work - capture engine, streaming
// transport, word typing into the focused window - is the same seam code
// Windows uses. Enter starts, the voice-activity detector or Enter stops.

use crate::config;
use crate::platform::{audio, input, unix_audio};
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn run() -> Result<(), String> {
    let cfg = config::load().map_err(|e| {
        format!(
            "{e} (put a mnvoice.env file with your API key next to the binary, \
             or export API_KEY)"
        )
    })?;

    let engine = unix_audio::CpalAudio::new()?;

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

    dictation_loop(&lines, || dictate(&cfg, &engine, &lines))?;
    Ok(())
}

/// Runs dictations until stdin closes.
///
/// One dictation failing must not end the session: the Windows product logs the
/// error and returns its tray to idle, so a transient network or audio failure
/// costs the user one attempt rather than the whole process - which matters
/// most exactly when it hurts, because the words already typed into the focused
/// window stay there and the user is not asked to restart the binary.
fn dictation_loop<F>(lines: &Receiver<()>, mut dictate: F) -> Result<(), String>
where
    F: FnMut() -> Result<(), String>,
{
    loop {
        // A closed stdin ends the session instead of spinning on Enter.
        if lines.recv().is_err() {
            break;
        }
        // Every line already queued is stale: it arrived while the previous
        // dictation was transcribing, so it belongs to no recording. The
        // recording loop's "Enter again" control would otherwise take one and
        // stop the next dictation before it had captured any audio at all.
        while lines.try_recv().is_ok() {}
        if let Err(e) = dictate() {
            println!("dictation failed: {e}");
        }
        println!();
        println!("Press Enter for the next dictation.");
    }
    Ok(())
}

/// Waits for the capture to end, raising `stop` on the "Enter again" control.
///
/// The engine owns the device and reports how the session ended on its own
/// channel, so all this does is watch the two channels that already exist: the
/// line reader, whose next line is the user asking to stop early, and the
/// engine's report. Returns that report with how long the microphone was open.
fn watch_recording(
    stop: &Arc<AtomicBool>,
    lines: &Receiver<()>,
    capture_done: &Receiver<Result<(), String>>,
) -> (Result<(), String>, Duration) {
    let opened = Instant::now();
    loop {
        // The "Enter again" control: the next line from the single reader
        // thread raises the same stop signal the VAD would.
        if lines.try_recv().is_ok() {
            stop.store(true, Ordering::SeqCst);
        }
        match capture_done.try_recv() {
            Ok(result) => return (result, opened.elapsed()),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Runs the transcriber and the capture together.
///
/// The microphone feeds the loop while the user is still speaking, the way the
/// Windows engine feeds it from the live microphone: the loop's stop flag is
/// the capture's own, so the provider reporting the end of an utterance ends
/// the recording and the loop then flushes what has already been captured.
/// Buffering the clip first instead types nothing until the user stops
/// speaking, and it buys no safety either - a clip still being fed when the
/// loop stops sending loses everything after the first utterance, which is why
/// the capture and the loop share one stop signal rather than one buffer.
///
/// The transcriber runs on its own thread so this one can keep reading stdin:
/// "Enter again" has to be able to stop a recording that is still happening.
fn transcribe_while_recording<T, F>(
    stop: &Arc<AtomicBool>,
    rx: Receiver<Vec<i16>>,
    capture_done: &Receiver<Result<(), String>>,
    lines: &Receiver<()>,
    transcribe: F,
) -> (Result<T, String>, Duration)
where
    F: FnOnce(&Arc<AtomicBool>, Receiver<Vec<i16>>) -> Result<T, String> + Send,
    T: Send,
{
    std::thread::scope(|scope| {
        let transcriber = scope.spawn(|| transcribe(stop, rx));
        let (captured, recorded) = watch_recording(stop, lines, capture_done);
        let transcribed = transcriber
            .join()
            .unwrap_or_else(|_| Err("transcription stopped unexpectedly".to_string()));
        (captured.and(transcribed), recorded)
    })
}

fn dictate(
    cfg: &config::Config,
    engine: &dyn audio::Audio,
    lines: &Receiver<()>,
) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let capture_done = engine.capture_to_channel(
        Arc::clone(&stop),
        cfg.max_seconds,
        cfg.vad_silence_ms,
        cfg.vad_rms_threshold,
        tx,
    )?;

    println!("recording... (Enter to stop)");

    let text = match cfg.protocol {
        config::Protocol::Streaming => {
            let cancelled = Arc::new(AtomicBool::new(false));
            let (text, recorded) =
                transcribe_while_recording(&stop, rx, &capture_done, lines, |stop, rx| {
                    crate::stream::run_stream(cfg, stop, &cancelled, rx)
                });
            println!("stopped ({}s of audio)", recorded.as_secs());
            text?
        }
        config::Protocol::Rest => {
            let (captured, _) = watch_recording(&stop, lines, &capture_done);
            let mut samples: Vec<i16> = Vec::new();
            while let Ok(chunk) = rx.try_recv() {
                samples.extend_from_slice(&chunk);
            }
            captured?;
            println!("stopped ({}s of audio)", samples.len() / 16_000);
            let wav = audio::wav_bytes(&samples);
            let raw = crate::rest::transcribe(cfg, &wav)?;
            let (text, trailing) =
                crate::rest::rest_typing(&raw, cfg.strip_fillers, cfg.trailing_space);
            if !text.is_empty() {
                input::type_text(&text);
                if trailing {
                    input::type_text(" ");
                }
            }
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
    use std::sync::Mutex;

    /// The transcriber has to consume the microphone while it is still
    /// capturing. Handing it a finished clip instead types nothing until the
    /// user stops speaking, which is the opposite of what the banner and the
    /// README promise - and it buys no safety, because a clip still being fed
    /// when the loop stops sending loses everything after the first utterance.
    #[test]
    fn the_transcriber_is_fed_while_the_microphone_is_still_capturing() {
        // A capture engine that hands over one packet every 20 ms and reports
        // done once it has sent ten.
        let (tx, rx) = mpsc::channel::<Vec<i16>>();
        let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
        let ended_at = Arc::new(Mutex::new(None::<Instant>));
        let engine_ended = Arc::clone(&ended_at);
        std::thread::spawn(move || {
            for _ in 0..10 {
                if tx.send(vec![0i16; 320]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(tx);
            *engine_ended.lock().unwrap() = Some(Instant::now());
            let _ = done_tx.send(Ok(()));
        });

        let (line_tx, line_rx) = mpsc::channel::<()>();
        // Nobody presses Enter: the engine ends the session on its own.
        drop(line_tx);
        let stop = Arc::new(AtomicBool::new(false));

        let (outcome, _recorded) =
            transcribe_while_recording(&stop, rx, &done_rx, &line_rx, |_, rx| {
                let mut packets = 0usize;
                let mut fed_at: Option<Instant> = None;
                for _ in rx.iter() {
                    packets += 1;
                    fed_at.get_or_insert_with(Instant::now);
                }
                Ok((packets, fed_at))
            });

        let (packets, fed_at) = outcome.expect("the transcriber ran to completion");
        assert_eq!(packets, 10, "every captured packet reached the transcriber");
        let fed_at = fed_at.expect("the transcriber was handed packets");
        let ended_at = ended_at.lock().unwrap().expect("the capture reported done");
        assert!(
            fed_at < ended_at,
            "the transcriber was handed the clip only after the capture ended"
        );
    }

    /// "Enter again" has to stop a recording that is still happening, which is
    /// only possible while the transcriber runs on its own thread.
    #[test]
    fn enter_stops_a_recording_that_is_still_running() {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Vec<i16>>();
        let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
        let engine_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            // Captures until something stops it, the way the real engine does.
            while !engine_stop.load(Ordering::SeqCst) {
                if tx.send(vec![0i16; 320]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(tx);
            let _ = done_tx.send(Ok(()));
        });

        let (line_tx, line_rx) = mpsc::channel::<()>();
        line_tx.send(()).unwrap();

        let began = Instant::now();
        let (outcome, _recorded) =
            transcribe_while_recording(&stop, rx, &done_rx, &line_rx, |_, rx| {
                let mut packets = 0usize;
                for _ in rx.iter() {
                    packets += 1;
                }
                Ok(packets)
            });

        assert!(
            outcome.is_ok(),
            "the transcriber saw the session end cleanly"
        );
        assert!(
            stop.load(Ordering::SeqCst),
            "Enter must raise the capture's stop signal"
        );
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "Enter did not stop the recording: {:?}",
            began.elapsed()
        );
    }

    /// A capture that fails still has to surface its error: the transcriber
    /// finishes with nothing to show for it, and the session reports why
    /// instead of pretending the dictation was simply empty.
    #[test]
    fn a_capture_error_is_reported_alongside_the_transcript() {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Vec<i16>>();
        let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
        // The engine drops its sender before it reports, exactly as the real
        // one does.
        drop(tx);
        let _ = done_tx.send(Err("no input audio device found".to_string()));

        let (line_tx, line_rx) = mpsc::channel::<()>();
        drop(line_tx);

        let (outcome, _recorded) =
            transcribe_while_recording(&stop, rx, &done_rx, &line_rx, |_, rx| {
                let mut packets = 0usize;
                for _ in rx.iter() {
                    packets += 1;
                }
                Ok(packets)
            });

        assert_eq!(outcome, Err("no input audio device found".to_string()));
    }

    /// A failed dictation must cost one attempt, not the session. The Windows
    /// product logs a streaming or transcription error and returns its tray to
    /// idle; propagating it out of the loop instead exits the process, so a
    /// single transient network failure ends a session the user was still in.
    #[test]
    fn a_failed_dictation_does_not_end_the_session() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        // The sender is kept, not dropped: the next Enter is pressed once the
        // failed dictation is over, and the channel has to close with that
        // press or the loop never sees stdin end.
        let mut next_press = Some(tx);

        let mut attempts = 0usize;
        let result = dictation_loop(&rx, || {
            attempts += 1;
            if attempts == 1 {
                if let Some(tx) = next_press.take() {
                    tx.send(()).unwrap();
                }
                Err("streaming error: handshake failed".to_string())
            } else {
                Ok(())
            }
        });

        // The loop only ever ends on a closed stdin, and a failure costs one
        // attempt rather than the session.
        assert_eq!(result, Ok(()));
        assert_eq!(attempts, 2);
    }

    /// A closed stdin still ends the session, which is how the user quits.
    #[test]
    fn a_closed_stdin_ends_the_session() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);

        let mut attempts = 0usize;
        let result = dictation_loop(&rx, || {
            attempts += 1;
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert_eq!(attempts, 0);
    }

    /// A line that arrives while a dictation is transcribing is stale by the
    /// time the next one starts. The recording loop reads the same channel as
    /// its "Enter again" control, so a queued line reaches it and stops the
    /// next dictation before it captured any audio - a double-tap during the
    /// streaming wait silently burns the following attempt.
    #[test]
    fn stale_lines_do_not_stop_the_next_dictation() {
        let (tx, rx) = mpsc::channel::<()>();
        // One line starts the dictation; two more arrive while it transcribes,
        // which is what a double-tap during the streaming wait produces.
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        drop(tx);

        let mut attempts = 0usize;
        let mut stale = 0usize;
        let result = dictation_loop(&rx, || {
            attempts += 1;
            // What the recording loop's "Enter again" control would see.
            if rx.try_recv().is_ok() {
                stale += 1;
            }
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert_eq!(
            attempts, 1,
            "only the line that started it may begin a dictation"
        );
        assert_eq!(stale, 0, "no stale line may reach the recording loop");
    }
}
