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
use crate::platform::{audio, cli, linux_orb, linux_portal, unix_audio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

/// Runs the app. Returns an error only for startup failures that leave the
/// process with nothing to do; a failed orb or portal keeps running.
pub fn main() -> Result<(), String> {
    // Single instance: a second process finds the lock taken and exits before
    // it opens a capture engine or a Wayland connection. The Windows guard is
    // a named mutex (`windows_app::claim_single_instance`); here it is a flock
    // on a file in the runtime dir, which the kernel releases if the process
    // dies - no stale-lock cleanup, the same self-healing property.
    let _lock = match claim_single_instance() {
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
    let (hotkey_tx, hotkey_rx) = mpsc::channel::<HotkeyEvent>();
    let _hotkey_stop = match linux_portal::spawn_hotkey_listener(&Some(Arc::clone(&cfg)), hotkey_tx)
    {
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

    // The orb starts hidden and shows only while a dictation runs.
    let _ = orb_tx.send(linux_orb::OrbCommand::Hide);

    let engine = unix_audio::CpalAudio;
    // The hotkey listener's voice: Toggle re-sent on the toggle channel stops
    // the running dictation, Cancel lands on its own channel.
    let (toggle_tx, toggle_rx) = mpsc::channel::<()>();
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
    let mut typer: Option<linux_portal::WaylandTyper> = None;

    // One hotkey event at a time, forever: Toggle starts (or stops) a
    // dictation, Cancel discards one in flight. This is the message loop the
    // Windows tray runs, with the portal as the message source.
    let _keep = (toggle_tx.clone(), cancel_tx.clone());
    loop {
        match hotkey_rx.recv() {
            Ok(HotkeyEvent::Toggle) => {
                if RECORDING.load(std::sync::atomic::Ordering::SeqCst) {
                    // The same press that started the dictation stops it - the
                    // Windows hotkey's contract.
                    let _ = toggle_tx.send(());
                } else {
                    let _ = orb_tx.send(linux_orb::OrbCommand::Show(
                        crate::orb_render::OrbState::Recording,
                    ));
                    let result =
                        dictate(&cfg, &engine, &toggle_rx, &cancel_rx, &mut typer, &orb_tx);
                    let _ = orb_tx.send(linux_orb::OrbCommand::Hide);
                    if let Err(e) = result {
                        eprintln!("dictation failed: {e}");
                    }
                }
            }
            Ok(HotkeyEvent::Cancel) => {
                // Cancel during a recording: raise the cancel flag, which the
                // transcriber reads to discard the transcript.
                let _ = cancel_tx.send(());
            }
            Err(_) => {
                // The hotkey listener died. Keep running without it rather
                // than spinning: the user can restart the app.
                eprintln!("mnvoice: hotkey listener ended; dictation is unreachable until restart");
                return Ok(());
            }
        }
    }
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
fn claim_single_instance() -> Option<std::fs::File> {
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
    let rc = rustix::fs::fcntl_lock(
        file.as_fd(),
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    );
    if rc.is_err() {
        return None;
    }
    Some(file)
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
