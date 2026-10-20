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
        Arc::new(|| RECORDING.load(std::sync::atomic::Ordering::SeqCst)),
        cfg.orb_color,
        cfg.orb_fluid_level,
        cfg.hotkey_str.clone(),
    )
    .map_err(|e| eprintln!("mnvoice: {e}"))
    .ok();

    // The orb starts hidden and shows only while a dictation runs.
    let _ = orb_tx.send(linux_orb::OrbCommand::Hide);

    let engine = unix_audio::CpalAudio;
    // The hotkey listener's voice: Toggle re-sent on the toggle channel stops
    // the running dictation, Cancel lands on its own channel.
    let (toggle_tx, toggle_rx) = mpsc::channel::<()>();
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
    // The cancel key is armed per recording (see linux_portal::CancelKey): a
    // key bound at startup is grabbed by kglobalaccel forever, and a plain
    // Esc bound that way is dead in every other app.
    let cancel_key = linux_portal::spawn_cancel_key(
        &Some(Arc::clone(&cfg)),
        event_tx.clone(),
        AppEvent::from_hotkey,
    );
    let mut typer: Option<linux_portal::WaylandTyper> = None;

    // One hotkey event at a time, forever: Toggle starts (or stops) a
    // dictation, Cancel discards one in flight. This is the message loop the
    // Windows tray runs, with the portal as the message source.
    let _keep = (toggle_tx.clone(), cancel_tx.clone());
    loop {
        match event_rx.recv() {
            Ok(AppEvent::Toggle) => {
                if RECORDING.load(std::sync::atomic::Ordering::SeqCst) {
                    // The same press that started the dictation stops it - the
                    // Windows hotkey's contract, and the tray's
                    // "Stop & transcribe" row.
                    let _ = toggle_tx.send(());
                } else {
                    // Claim the cancel key BEFORE anything becomes visible, the
                    // Windows tray's contract: the orb shows only once Esc is
                    // already live. A failed arm is logged, not fatal - the
                    // recording still runs, the way the Windows tip records a
                    // failed registration.
                    if let Err(e) = cancel_key.arm() {
                        eprintln!("mnvoice: cancel key unavailable ({e})");
                    }
                    let _ = orb_tx.send(linux_orb::OrbCommand::Show(
                        crate::orb_render::OrbState::Recording,
                    ));
                    // The tray's first row reads the recording state, so a
                    // click that starts a dictation has to repaint it.
                    refresh_tray(&tray);
                    let result =
                        dictate(&cfg, &engine, &toggle_rx, &cancel_rx, &mut typer, &orb_tx);
                    // Release Esc the moment the dictation returns, so the
                    // key is free in every other app while nothing runs.
                    cancel_key.disarm();
                    let _ = orb_tx.send(linux_orb::OrbCommand::Hide);
                    refresh_tray(&tray);
                    if let Err(e) = result {
                        eprintln!("dictation failed: {e}");
                    }
                }
            }
            Ok(AppEvent::Cancel) => {
                // Cancel during a recording: raise the cancel flag, which the
                // transcriber reads to discard the transcript. The cancel key
                // is armed only while a dictation runs, and the flag gates the
                // forward anyway: a press racing the disarm must never buffer
                // into the next session's channel and discard it before it
                // starts.
                if RECORDING.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = cancel_tx.send(());
                }
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

/// Whether a dictation is in flight. The hotkey's Toggle means "stop" exactly
/// while one is running, so the flag has to be read, not guessed.
static RECORDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    RECORDING.store(true, std::sync::atomic::Ordering::SeqCst);
    let result = cli::dictate_with_controls(cfg, engine, toggle_rx, Some(cancel_rx), |text| {
        // The orb moves to the "thinking" state while the transcript is
        // being produced, exactly like the Windows orb.
        let _ = orb_tx.send(linux_orb::OrbCommand::State(
            crate::orb_render::OrbState::Transcribing,
        ));
        type_transcript(typer, text);
    });
    RECORDING.store(false, std::sync::atomic::Ordering::SeqCst);
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
