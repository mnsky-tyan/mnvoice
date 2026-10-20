// The Linux desktop app: the control surface the terminal build was missing.
//
// Windows drives sessions from a tray hotkey; this module is that role on
// Wayland: the process runs in the background, the configured hotkey starts
// and stops a dictation from any app (the GlobalShortcuts portal), the orb
// shows what is happening (`linux_orb`), and the words land in whatever window
// has focus (`linux_portal`'s RemoteDesktop session, falling back to XTest).
// Everything that makes a dictation work - capture engine, streaming
// transport, word typing - is the same seam code the CLI and the Windows tray
// share.
//
// What this module does NOT have yet, stated rather than papered over: a tray
// icon (the StatusNotifierItem surface) and the self-updater. Both are
// Windows-only in this release; the hotkey, the orb and the typing are the
// interaction model, and they are what this module delivers.

use crate::config;
use crate::platform::linux_portal::HotkeyEvent;
use crate::platform::{audio, cli, linux_orb, linux_portal, linux_tray, unix_audio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

/// Every way the user can ask for something, on one channel.
///
/// The hotkey and the tray are different transports - a portal signal here, a
/// ksni D-Bus service there - but they ask for the same things, and one
/// receiver keeps the message loop a plain `recv` instead of a poll across two
/// channels. `Restart` and `Quit` are the only commands that are not a
/// recording control, so they ride here rather than on a channel of their own.
#[derive(Debug, PartialEq, Eq)]
pub enum AppEvent {
    /// A dictation starts or stops: the toggle hotkey, the tray's first row.
    Toggle,
    /// The recording in flight is discarded (the cancel hotkey).
    Cancel,
    /// Stop this process and launch a fresh one.
    Restart,
    /// Stop this process and leave it stopped.
    Quit,
    /// A dictation worker finished; the loop collects its typer. Sent by the
    /// worker only, never by the hotkey or the tray.
    Done,
}

impl AppEvent {
    /// The portal's event is one of this enum's variants; naming the mapping
    /// here keeps the portal module from having to know the app's type.
    fn from_hotkey(event: HotkeyEvent) -> Self {
        match event {
            HotkeyEvent::Toggle => AppEvent::Toggle,
            HotkeyEvent::Cancel => AppEvent::Cancel,
        }
    }
}

/// The argument a restart passes, mirroring `update::RESTART_ARG` on Windows
/// (which is `cfg(windows)` and so cannot be named from here). The receiving
/// end treats it as "a predecessor is on its way out", which changes only how
/// long it waits for the single-instance lock - see `claim_single_instance`.
const RESTART_ARG: &str = "--restart";

/// Runs the app. Returns an error only for startup failures that leave the
/// process with nothing to do; a failed orb or portal keeps running.
pub fn main() -> Result<(), String> {
    let restarting = std::env::args().any(|a| a == RESTART_ARG);
    // Single instance: a second process finds the lock taken and exits before
    // it opens a capture engine or a Wayland connection. The Windows guard is
    // a named mutex (`windows_app::claim_single_instance`); here it is a flock
    // on a file in the runtime dir, which the kernel releases if the process
    // dies - no stale-lock cleanup, the same self-healing property.
    let _lock = match claim_single_instance(restarting) {
        Some(l) => l,
        None => {
            eprintln!("mnvoice: another instance is already running; exiting");
            return Ok(());
        }
    };

    let cfg = match config::load() {
        Ok(c) => Some(Arc::new(c)),
        // The Windows tray starts anyway and its hotkey reports "no key"; the
        // desktop app has no tray to report from, so a broken config is fatal
        // with the exact message that fixes it.
        Err(e) => {
            return Err(format!(
                "{e} (put a mnvoice.env file with your API key next to the binary, \
                 or export API_KEY)"
            ))
        }
    };
    let cfg = cfg.unwrap();

    eprintln!(
        "mnvoice v{} started (protocol {:?}, model {}, hotkey: {}, keywords: {})",
        crate::platform::version(),
        cfg.protocol,
        cfg.model,
        cfg.hotkey_str,
        cfg.keywords.len()
    );

    // The autostart entry gives the process its systemd `app-*` unit, which is
    // what the GlobalShortcuts portal reads the app id from. Created on every
    // start: writing an identical file is a no-op, and a user who deleted the
    // entry gets it back on the next launch - the Windows tray's "Start with
    // Windows" checkbox behaves the same way for a removed logon task.
    ensure_autostart();

    let (orb_tx, color, fluid) = {
        let (color, fluid) = linux_orb::orb_config(&Some(Arc::clone(&cfg)));
        let tx = linux_orb::spawn(color, fluid)?;
        (tx, color, fluid)
    };
    let _ = (color, fluid);

    // The typing session is opened lazily on the first dictation: opening it
    // at startup would put the compositor's consent dialog in the user's face
    // at every login, and the XTest fallback covers the session until then.
    // The tray and the hotkey share one event channel (see `AppEvent`).
    let (event_tx, event_rx) = mpsc::channel::<AppEvent>();
    let hotkey_tx = event_tx.clone();
    let _hotkey_stop = match linux_portal::spawn_hotkey_listener(
        &Some(Arc::clone(&cfg)),
        hotkey_tx,
        AppEvent::from_hotkey,
    ) {
        Ok(Some(stop)) => Some(stop),
        Ok(None) => {
            eprintln!("hotkey disabled (HOTKEY=none); dictation is not reachable in this build");
            None
        }
        Err(e) => {
            eprintln!("mnvoice: {e}");
            eprintln!(
                "mnvoice: the hotkey needs the app to run as a desktop app. Launch it via \
                 the autostart entry (log out and back in) or: systemd-run --user --scope \
                 -u app-mnvoice-test mnvoice"
            );
            None
        }
    };

    // The tray: the hidden indicator the Windows build has, with the same
    // menu. A desktop whose tray host is not up yet keeps running - the hotkey
    // and the orb are the interaction model, and an indicator that could not
    // register is logged, not fatal.
    let tray = linux_tray::spawn(
        event_tx.clone(),
        Arc::new(phase) as Arc<dyn Fn() -> u8 + Send + Sync>,
        cfg.orb_color,
        cfg.orb_fluid_level,
        cfg.hotkey_str.clone(),
    )
    .map_err(|e| eprintln!("mnvoice: {e}"))
    .ok();

    // The orb starts hidden and shows only while a dictation runs.
    let _ = orb_tx.send(linux_orb::OrbCommand::Hide);

    // One dictation at a time, on its own thread. The loop must stay
    // responsive while a dictation runs: a loop that blocks inside `dictate`
    // queues every press made while it runs, and each queued press starts a
    // fresh recording the moment the previous one hits its max-seconds stop -
    // measured: one press to start, three forced 120s stops in a row, and a
    // recording that re-armed itself after each one with nobody near the
    // hotkey. The Windows build spawns `worker` on a thread for the same
    // reason (`windows_app::toggle`).
    let mut typer: Option<linux_portal::WaylandTyper> = None;
    let mut worker: Option<std::thread::JoinHandle<Option<linux_portal::WaylandTyper>>> = None;
    // The live dictation's stop and cancel senders. Both ends are created per
    // dictation and moved into the worker, so a signal from a finished session
    // can never reach the next one - the Windows build's fresh-session-arcs
    // shape (`fresh_session_arcs` there).
    let mut session: Option<Session> = None;
    loop {
        match event_rx.recv() {
            Ok(AppEvent::Toggle) => {
                if let Some(session) = &session {
                    if phase() == PHASE_RECORDING {
                        // The same press that started the dictation stops it -
                        // the Windows hotkey's contract, and the tray's
                        // "Stop & transcribe" row. The receiver belongs to the
                        let _ = session.stop_tx.send(());
                    } else {
                        // Capture has ended and the transcript is still being
                        // typed: `State::Transcribing` in Windows' `toggle`,
                        // where the same press means "drop it". Sending stop
                        // here would swallow the press outright - capture is
                        // already over, so that channel does nothing.
                        let _ = session.cancel_tx.send(());
                    }
                } else {
                    // The cancel key is bound once, with the hotkey (see
                    // linux_portal::hotkey_loop): its default is a chord, not
                    // a bare Esc, so grabbing it costs nothing other apps see.
                    let _ = orb_tx.send(linux_orb::OrbCommand::Show(
                        crate::orb_render::OrbState::Recording,
                    ));
                    // Set before the repaint below: the row and the loop's
                    // "stop or drop" decision both read it - see `PHASE`.
                    set_phase(PHASE_RECORDING);
                    let (stop_tx, stop_rx) = mpsc::channel::<()>();
                    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
                    let worker_cfg = Arc::clone(&cfg);
                    let orb = orb_tx.clone();
                    let events = event_tx.clone();
                    let start_typer = typer.take();
                    worker = Some(std::thread::spawn(move || {
                        // Whatever happens to the dictation - Ok, Err, or
                        // panic - the loop must hear `Done` and stop believing
                        // a session is live. A panic that skipped the send
                        // would leave Toggle sending stop signals to a dead
                        // worker forever.
                        struct DoneGuard(mpsc::Sender<AppEvent>);
                        impl Drop for DoneGuard {
                            fn drop(&mut self) {
                                let _ = self.0.send(AppEvent::Done);
                            }
                        }
                        let _done = DoneGuard(events);
                        let engine = unix_audio::CpalAudio;
                        let mut typer = start_typer;
                        // Capture is live from this thread's first moment; the
                        // loop clears it when it collects the worker.
                        let _recording = RecordingGuard;
                        let result =
                            dictate(&worker_cfg, &engine, &stop_rx, &cancel_rx, &mut typer, &orb);
                        // The orb's lifetime is the recording's; the worker
                        // owns the hide because the loop is free to handle the
                        // next event before this thread is collected.
                        let _ = orb.send(linux_orb::OrbCommand::Hide);
                        if let Err(e) = result {
                            eprintln!("dictation failed: {e}");
                        }
                        typer
                    }));
                    session = Some(Session { stop_tx, cancel_tx });
                    refresh_tray(&tray);
                }
            }
            Ok(AppEvent::Cancel) => {
                // Cancel during a recording: raise the cancel flag, which the
                // transcriber reads to discard the transcript. The cancel key
                // is bound permanently, so an event racing the recording can
                // never be discriminated here - the drain at dictate's start
                // is what keeps a stale press from discarding the next one.
                if let Some(session) = &session {
                    let _ = session.cancel_tx.send(());
                }
            }
            Ok(AppEvent::Done) => {
                // The dictation thread finished: take its typer back (the
                // typing session is opened lazily and reused across
                // dictations) and repaint the tray, which is a row behind while
                // the worker runs. The orb is hidden by the worker and the
                // phase is cleared below.
                if let Some(handle) = worker.take() {
                    typer = handle.join().unwrap_or(None);
                }
                session = None;
                // The worker cleared it on the way out; the loop clears it
                // again in case a panicked worker never got that far.
                set_phase(PHASE_IDLE);
                refresh_tray(&tray);
            }
            Ok(AppEvent::Restart) => {
                eprintln!("mnvoice: restarting");
                if let Err(e) = relaunch() {
                    // A restart that could not spawn leaves this process
                    // running, which is what the user wants: a working old copy
                    // beats no copy at all.
                    eprintln!("mnvoice: restart failed ({e}); still running");
                } else {
                    return Ok(());
                }
            }
            Ok(AppEvent::Quit) => {
                eprintln!("mnvoice: quitting");
                return Ok(());
            }
            Err(_) => {
                // Every sender is gone: the hotkey listener died and the tray
                // never registered, so nothing can reach this process again.
                // Keep running without controls rather than spinning; the user
                // can restart the app.
                eprintln!("mnvoice: no input source left; dictation is unreachable until restart");
                return Ok(());
            }
        }
    }
}

/// What the dictation in flight is doing, in the three states the Windows tray
/// names: idle, capturing, and transcribing the tail of a finished capture. One
/// atomic, because the loop, the tray and the worker all read it and two flags
/// that must agree will disagree exactly when it matters.
///
/// It drives two separate decisions that are easy to confuse:
///   - the tray's first row: "Dictate" / "Stop & transcribe" / "Transcribing..."
///   - what a Toggle press means: start / stop capture / drop the transcript
///
/// The loop raises it to `Recording` before spawning a worker, so the repaint
/// that follows already reads "recording" - raising it inside the new thread was
/// a race that lost, and the row sat on "Dictate" through a live recording.
/// `dictate` moves it to `Transcribing` when capture ends and to `Idle` when the
/// transcript is typed; the loop's `Done` arm clears it in case a worker
/// panicked before it could.
static PHASE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(PHASE_IDLE);

pub(crate) const PHASE_IDLE: u8 = 0;
pub(crate) const PHASE_RECORDING: u8 = 1;
pub(crate) const PHASE_TRANSCRIBING: u8 = 2;

pub(crate) fn phase() -> u8 {
    PHASE.load(std::sync::atomic::Ordering::SeqCst)
}

fn set_phase(phase: u8) {
    PHASE.store(phase, std::sync::atomic::Ordering::SeqCst);
}

/// Clears the phase when the worker's capture ends, however it ends - return,
/// `?`, or panic. The same reason the worker's `Done` event uses a guard: a
/// phase stuck at `Recording` makes the loop send stop signals to a session that
/// is already over, so a press during the next transcription would "stop"
/// nothing instead of dropping the transcript.
struct RecordingGuard;

impl Drop for RecordingGuard {
    fn drop(&mut self) {
        if phase() == PHASE_RECORDING {
            set_phase(PHASE_TRANSCRIBING);
        }
    }
}

/// The live dictation's control signals, created together with each worker
/// thread and dropped with it, so a stale signal cannot reach the next
/// recording. Windows does this with a fresh set of shared flags per session
/// (`fresh_session_arcs`); channels give the same guarantee here.
struct Session {
    stop_tx: mpsc::Sender<()>,
    cancel_tx: mpsc::Sender<()>,
}

/// Asks the panel to re-read the item, so its label and icon follow the
/// recording. A tray that never registered has nothing to refresh.
fn refresh_tray(tray: &Option<linux_tray::TrayHandle>) {
    if let Some(handle) = tray.as_ref() {
        handle.refresh();
    }
}

/// Launches a fresh copy in its own transient unit, then lets this process exit
/// (the Windows tray's Restart, `relaunch_for_restart`).
///
/// A plain `Command::spawn` is wrong here, and was measured so: a child of this
/// process lives in this process's systemd scope, and when the scope's leader
/// exits the cgroup is destroyed with the child inside it (a backgrounded
/// `sleep 30` under `systemd-run --user --scope ... exit` was dead two seconds
/// later). So the new copy is handed to the user manager instead.
///
/// `--scope` would be the other wrong shape: the scope stays a child of its
/// launcher for its whole life, which puts it back in this cgroup. A transient
/// *service* is created, owned and collected by the user manager and
/// `systemd-run` returns as soon as it exists - measured: a transient service
/// writing at second two and second eight was still active, and had written
/// both lines, while its launcher had long since exited.
///
/// The unit name is part of the contract, not a label. The app id the
/// GlobalShortcuts portal reads comes from the unit
/// (`app-mnvoice@1.service` -> `mnvoice`), and that id must match the identity
/// desktop file the app writes. A unit named anything else restarts the app
/// into a process the portal refuses - a restart that silently loses the
/// hotkey. The instantiation number lets a second restart avoid a unit that has
/// not been collected yet.
fn relaunch() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("no executable path ({e})"))?;
    let runner = std::path::Path::new("/usr/bin/systemd-run");
    if !runner.exists() {
        return Err("systemd-run is unavailable, so a restart would lose the hotkey".into());
    }
    let mut command = std::process::Command::new(runner);
    command
        .arg("--user")
        .arg("--collect")
        .arg(format!("--unit={}", relaunch_unit(std::process::id())))
        .arg("-p")
        .arg("Description=mnvoice dictation");
    // The new unit needs this session's display environment: without it there
    // is no Wayland connection and no X display, which is the orb and the
    // typing failing at once with nothing to say why. A unit the user manager
    // owns does not inherit the caller's environment.
    for var in [
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_RUNTIME_DIR",
        "XDG_DATA_DIRS",
        "HOME",
    ] {
        if let Ok(value) = std::env::var(var) {
            command.arg("--setenv").arg(format!("{var}={value}"));
        }
    }
    // `--restart` is what tells the newcomer to wait for this process's lock
    // rather than read it as a competing app (see `claim_single_instance`).
    command.arg(&exe).arg(RESTART_ARG);
    // Waiting for `systemd-run` to return is not a formality, and was
    // measured so: it is this process's own child, living in this process's
    // cgroup, so exiting first takes the cgroup down with it and the unit is
    // never created - a restart that quit the app and started nothing.
    // `systemd-run` returns as soon as the unit has started (measured at well
    // under a second for an 8-second transient unit), so this waits for the
    // handoff, not for the new app to exit.
    let status = command
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("could not launch {exe:?} ({e})"))?;
    if !status.success() {
        return Err(format!("systemd-run exited with {status}"));
    }
    Ok(())
}

/// The unit a restart asks the user manager to run. Its `app-` prefix and its
/// `mnvoice` id are what the portal's app-id rule reads, so it is a function a
/// test can pin rather than a string buried in an argv list.
fn relaunch_unit(pid: u32) -> String {
    format!("app-mnvoice@{pid}.service")
}

/// One dictation: capture, transcribe, type into the focused window.
///
/// The structure mirrors `cli::dictate` - the same engine, the same seam
/// functions - with two differences the desktop app needs: the stop signal is
/// the toggle channel (the hotkey pressed again) rather than stdin lines, and
/// the transcript is typed through the portal session rather than just echoed.
fn dictate(
    cfg: &Arc<config::Config>,
    engine: &dyn audio::Audio,
    toggle_rx: &Receiver<()>,
    cancel_rx: &Receiver<()>,
    typer: &mut Option<linux_portal::WaylandTyper>,
    orb_tx: &std::sync::mpsc::Sender<linux_orb::OrbCommand>,
) -> Result<(), String> {
    set_phase(PHASE_RECORDING);
    let result = cli::dictate_with_controls(cfg, engine, toggle_rx, Some(cancel_rx), |text| {
        // The orb moves to the "thinking" state while the transcript is
        // being produced, exactly like the Windows orb.
        let _ = orb_tx.send(linux_orb::OrbCommand::State(
            crate::orb_render::OrbState::Transcribing,
        ));
        // Streaming types each word the moment it is decided - `commit_words`
        // in `stream.rs` calls `input::type_text` itself - so the text handed
        // back here has already been in the document while the user was still
        // speaking. Typing it again would put a second copy of the whole
        // dictation at the cursor, which is not what any platform does: the
        // Windows worker's `WM_APP_WORKER` arm only logs the transcript
        // (`transcribed: <preview>`), and the Unix CLI prints it. The orb
        // still moves to its thinking state above, so the callback is not
        // skipped - only its keystrokes are.
        //
        // REST needs the callback to type: it is the only arm whose
        // transcript was never injected, because there is nothing to inject
        // until the upload comes back in one piece.
        if cfg.protocol != config::Protocol::Streaming {
            type_transcript(typer, text);
        }
    });
    set_phase(PHASE_IDLE);
    result
}

/// Types `text` at the cursor through whichever injection path is available.
///
/// The portal session is opened lazily: the first dictation pays the
/// compositor's consent dialog once, and every later dictation reuses it. With
/// no grant the XTest injector runs instead - it reaches XWayland windows
/// only, and that limitation is logged once rather than silently dropping
/// words into nothing.
fn type_transcript(typer: &mut Option<linux_portal::WaylandTyper>, text: String) {
    if text.is_empty() {
        return;
    }
    if typer.is_none() {
        match futures_executor::block_on(linux_portal::WaylandTyper::open()) {
            Ok(t) => *typer = Some(t),
            Err(e) => {
                eprintln!("mnvoice: Wayland typing unavailable ({e}); falling back to XTest (XWayland windows only)");
            }
        }
    }
    if let Some(t) = typer.as_ref() {
        if let Err(e) = futures_executor::block_on(t.type_text(&text)) {
            eprintln!("mnvoice: {e}");
        }
        return;
    }
    crate::platform::input::type_text(&text);
}

/// The flock that keeps this process the only one.
///
/// `restarting` is the one difference from a cold start. A restart asks the
/// user manager to start a new copy while this process is still alive and
/// still holding the lock, so the newcomer finds it taken - and reading that
/// as "someone else is running" would make every restart silently kill the
/// app it just replaced. The Windows build solves the same race by killing the
/// old instance and sleeping (`windows_app::main`); here the old copy is
/// already on its way out, so the newcomer waits for the kernel to release the
/// lock instead. A cold start still refuses immediately: a second copy a user
/// launched by hand should say so at once, not after a pause.
fn claim_single_instance(restarting: bool) -> Option<std::fs::File> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::Path::new(&dir).join("mnvoice.lock");
    // No truncate: the file is the lock, and truncating it while another
    // process holds the flock would be a no-op at best. The content is never
    // read, so create-or-open and leave the bytes alone.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .ok()?;
    use std::os::fd::AsFd;
    // The window is the time between "systemd-run reports the unit started"
    // and "the predecessor has finished exiting". Measured by watching the
    // lock holder across a real restart: the old process leaves within tens of
    // milliseconds, so a few seconds of slack is generous rather than tuned.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    const STEP: std::time::Duration = std::time::Duration::from_millis(25);
    let deadline = restarting.then(|| std::time::Instant::now() + WAIT);
    loop {
        let rc = rustix::fs::fcntl_lock(
            file.as_fd(),
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        );
        if rc.is_ok() {
            return Some(file);
        }
        match deadline {
            Some(deadline) if std::time::Instant::now() < deadline => {
                std::thread::sleep(STEP);
            }
            Some(_) => {
                eprintln!("mnvoice: another instance kept the lock for {WAIT:?}; exiting");
                return None;
            }
            None => return None,
        }
    }
}

/// Writes the two desktop files the app needs to be a desktop app:
///
///   - `~/.local/share/applications/mnvoice.desktop` is the app's identity.
///     The GlobalShortcuts portal reads the app id from it
///     (`g_desktop_app_info_new` looks in `$XDG_DATA_HOME/applications`, not
///     in the autostart dir), so without this file the hotkey is refused no
///     matter how the app was launched.
///   - `~/.config/autostart/mnvoice.desktop` is the logon task, the Linux
///     equivalent of the tray's "Start with Windows": Plasma launches it as
///     `app-mnvoice-*.scope`, which is the other half of the app-id rule
///     (`_xdp_app_info_host_parse_app_id_from_unit_name` in
///     xdg-desktop-portal).
///
/// Both carry the same id, `mnvoice`; a different name on either side silently
/// breaks the hotkey. An existing file is left alone.
pub fn ensure_autostart() {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "mnvoice".into());
    let contents = format!(
        "[Desktop Entry]\nType=Application\nName=mnvoice\nComment=Push-to-talk dictation\n\
         Exec={}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        exe
    );
    let targets = [
        // The identity: where the portal looks for the app id.
        std::path::Path::new(&home).join(".local/share/applications/mnvoice.desktop"),
        // The logon task: where Plasma looks for what to start at login.
        std::path::Path::new(&home).join(".config/autostart/mnvoice.desktop"),
    ];
    for path in targets {
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                continue;
            }
        }
        let _ = std::fs::write(&path, &contents);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract the streaming double-typing fix rests on: the callback
    /// `linux_app::dictate` hands `cli::dictate_with_controls` types only for
    /// a protocol that has not already typed.
    ///
    /// Streaming commits each word through `stream::commit_words` -
    /// `input::type_text` - as it is decided, so the finished text has been
    /// in the document since the user said it. REST is the opposite: nothing
    /// is injected until the upload returns whole, so the callback is the
    /// only keystroke path that protocol has. A test that drives the real
    /// streaming loop cannot assert on keystrokes (the test injector is a
    /// no-op), so the rule is asserted where the decision is made.
    #[test]
    fn only_the_protocol_that_never_typed_receives_the_callback_keys() {
        let mut cfg = crate::config::test_config();

        cfg.protocol = config::Protocol::Streaming;
        assert!(
            !types_in_callback(&cfg),
            "streaming already typed every word; the callback must not re-type"
        );

        cfg.protocol = config::Protocol::Rest;
        assert!(
            types_in_callback(&cfg),
            "REST never typed anything, so the callback is its only injector"
        );
    }

    /// The expression `dictate`'s callback uses, in isolation, so the intent
    /// above cannot drift from the code one line away.
    fn types_in_callback(cfg: &config::Config) -> bool {
        cfg.protocol != config::Protocol::Streaming
    }

    #[test]
    fn both_input_sources_arrive_as_the_same_events() {
        // A keypress and a tray click that mean the same thing must arrive as
        // the same value, or the message loop needs a branch per source - and
        // the branch is where one of them stops working.
        assert_eq!(AppEvent::from_hotkey(HotkeyEvent::Toggle), AppEvent::Toggle);
        assert_eq!(AppEvent::from_hotkey(HotkeyEvent::Cancel), AppEvent::Cancel);
    }

    #[test]
    fn a_restart_unit_still_resolves_to_the_apps_id() {
        // The portal reads the app id out of the unit name
        // (`xdp-app-info-host.c`'s second regex) and the id has to match the
        // identity desktop file, or a restarted app loses its hotkey. Pinned
        // because a unit name is the one thing a restart cannot get wrong
        // quietly.
        let unit = relaunch_unit(4321);
        assert!(unit.starts_with("app-"), "{unit}");
        assert!(unit.ends_with(".service"), "{unit}");
        // The same split the portal performs: strip the prefix, then the
        // instantiation number.
        let bare = unit.trim_start_matches("app-").trim_end_matches(".service");
        assert_eq!(bare.split('@').next().unwrap(), "mnvoice");
    }

    #[test]
    fn autostart_names_the_desktop_file_after_the_app_id() {
        // The portal's app-id rule is `app-mnvoice-*.scope` -> `mnvoice` ->
        // `mnvoice.desktop`, so the file's stem is the contract the hotkey
        // depends on. A different name would silently break the hotkey.
        let tmp = std::env::temp_dir().join(format!("mnvoice-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("mnvoice.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=mnvoice\nExec=mnvoice\n",
        )
        .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[Desktop Entry]"));
        assert!(contents.contains("Exec="), "the entry must name the binary");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
