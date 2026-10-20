// The XDG portal clients: the global hotkey and the Wayland typing session.
//
// Both are the Wayland-native replacements for the Windows tray's
// `RegisterHotKey` and `SendInput`:
//
//   - The hotkey is `org.freedesktop.portal.GlobalShortcuts`. The compositor
//     binds the configured key through kglobalaccel, so it fires globally -
//     native Wayland windows included - which an X11 grab cannot do. The
//     portal needs an app id, which on this stack comes from the systemd user
//     unit the process runs in (`app-mnvoice-*.scope`, see
//     `xdp_app_info_host_new` in xdg-desktop-portal): a process started from a
//     terminal has no app id and the portal refuses it. The autostart entry
//     (`linux_app::ensure_autostart`) is what gives the app its identity, the
//     same way the Windows logon task does.
//
//   - Typing is `org.freedesktop.portal.RemoteDesktop` with the keyboard
//     device: the compositor synthesizes the keystrokes itself and routes them
//     to whatever window has focus, native Wayland or XWayland alike. The
//     first use shows the compositor's consent dialog once; `persist_mode`
//     makes the grant survive restarts. With no grant the app falls back to
//     the X11 XTest injector, which reaches XWayland windows only - the
//     fallback is logged, never silent.
//
// ashpd is the maintained XDG portals wrapper (used by the reis crate's own
// `type-text-ei.rs` example); hand-rolling the Request/session dance would be
// re-implementing it. The `async-io` feature keeps tokio out of the binary -
// the repo's engine stays free of async runtimes.

use crate::config;
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::desktop::remote_desktop::{
    DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{PersistMode, Session};
use enumflags2::BitFlags;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

/// What the hotkey listener reports to the app loop.
#[derive(Debug)]
pub enum HotkeyEvent {
    /// The toggle key was pressed: start a dictation, or stop the running one.
    Toggle,
    /// The cancel key was pressed: discard the recording in flight.
    Cancel,
}

/// Maps a config hotkey spelling to the portal's trigger format.
///
/// The portal's parser (`xdgshortcut.cpp` in xdg-desktop-portal-kde) only
/// accepts the modifiers SHIFT, CAPS, CTRL, ALT, NUM and LOGO - spelled
/// uppercase and joined by `+` - and resolves the key case-insensitively
/// through `xkb_keysym_from_name`. The config's form (`Alt+Space`) has a mixed
/// case modifier, which the parser rejects with `Unknown modifier "Alt"`, so
/// the modifier words go uppercase and the key rides along:
/// `Alt+Space` -> `ALT+SPACE`, `Ctrl+Shift+D` -> `CTRL+SHIFT+D`, `F9` -> `F9`.
/// The config is the single owner of what "none" means, so a disabled hotkey
/// never reaches this function.
fn portal_trigger(hotkey_str: &str) -> String {
    hotkey_str.to_ascii_uppercase()
}

/// Spawns the hotkey listener. On success the app receives [`HotkeyEvent`]s
/// forever; on failure the error explains what was missing, because a hotkey
/// that silently does not fire reads exactly like a frozen app.
///
/// `HOTKEY=none` returns `Ok(None)`: the disabled control is not a failure,
/// and the caller keeps running without it (the Windows tray does the same).
pub fn spawn_hotkey_listener<E, F>(
    cfg: &Option<Arc<config::Config>>,
    tx: Sender<E>,
    map: F,
) -> Result<Option<Sender<()>>, String>
where
    E: Send + 'static,
    F: Fn(HotkeyEvent) -> E + Send + 'static,
{
    let hotkey_str = match cfg.as_ref() {
        Some(c) => c.hotkey_str.clone(),
        None => config::DEFAULT_HOTKEY_STR.to_string(),
    };
    if hotkey_str.eq_ignore_ascii_case("none") {
        return Ok(None);
    }

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        // The ready signal's contract: Ok once the session is bound, Err if
        // the portal refused first. hotkey_loop sends Ok itself - the bind
        // happens inside, before its forever-loop, and a signal sent at the
        // end of a forever-loop never arrives (every start timed out at 30s:
        // "hotkey listener did not start within 30s", measured in the
        // journal on 2026-10-20). A failure before bind lands here.
        let err_tx = ready_tx.clone();
        let result =
            futures_executor::block_on(hotkey_loop(hotkey_str, tx, map, stop_rx, ready_tx));
        if let Err(e) = result {
            let _ = err_tx.send(Err(e));
        }
    });
    // The listener reports the startup outcome before the app proceeds, so a
    // refused portal is surfaced at launch rather than at the first press.
    match ready_rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(Ok(())) => Ok(Some(stop_tx)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("hotkey listener did not start within 30s".into()),
    }
}

/// The listener's body: one portal session, the toggle key only, signals
/// forwarded to the app until the stop channel closes.
///
/// Only the toggle is bound here, and only here. The cancel key deliberately
/// does NOT live in this session: a shortcut bound to a permanent session is
/// grabbed by kglobalaccel for the app's whole life, and a plain Esc bound
/// that way is dead in every other app (measured 2026-10-20 - the kglobalaccel
/// grab consumed Esc system-wide, kglobalshortcutsrc `[mnvoice] cancel=Esc`).
/// The Windows tray registers the cancel key per recording and unregisters it
/// at the stop press; `spawn_cancel_key` below is that shape.
async fn hotkey_loop<E, F>(
    hotkey_str: String,
    tx: Sender<E>,
    map: F,
    stop_rx: std::sync::mpsc::Receiver<()>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String>
where
    E: Send,
    F: Fn(HotkeyEvent) -> E,
{
    let gs = GlobalShortcuts::new()
        .await
        .map_err(|e| format!("GlobalShortcuts portal unavailable ({e})"))?;

    let shortcuts = [NewShortcut::new("toggle", "Start or stop dictation")
        .preferred_trigger(Some(portal_trigger(&hotkey_str).as_str()))];

    let session = gs
        .create_session(ashpd::desktop::CreateSessionOptions::default())
        .await
        .map_err(|e| format!("hotkey session refused ({e}); is the app launched as a desktop app (autostart entry) rather than from a terminal?"))?;
    gs.bind_shortcuts(
        &session,
        &shortcuts,
        None,
        ashpd::desktop::global_shortcuts::BindShortcutsOptions::default(),
    )
    .await
    .map_err(|e| format!("binding the hotkeys failed ({e})"))?;

    // The session is bound: signal ready NOW, before the forever-loop. This
    // is the message the 30s startup timeout waits for.
    let _ = ready_tx.send(Ok(()));

    // The compositor's confirmation dialog appears on the first bind; after it
    // is accepted the binding persists in kglobalaccel.
    let mut activated = gs
        .receive_activated()
        .await
        .map_err(|e| format!("listening for hotkey presses failed ({e})"))?;
    let mut deactivated = gs
        .receive_deactivated()
        .await
        .map_err(|e| format!("listening for hotkey releases failed ({e})"))?;

    loop {
        // The stop channel closes when the app drops its sender, which ends
        // the session - the same way the Windows message loop ends.
        if stop_rx.try_recv().is_ok() {
            return Ok(());
        }
        let any =
            futures_util::future::select(Box::pin(activated.next()), Box::pin(deactivated.next()));
        match any.await {
            futures_util::future::Either::Left((Some(event), _)) => {
                let id = event.shortcut_id().to_string();
                let event_type = if id == "toggle" {
                    HotkeyEvent::Toggle
                } else if id == "cancel" {
                    HotkeyEvent::Cancel
                } else {
                    continue;
                };
                if tx.send(map(event_type)).is_err() {
                    return Ok(());
                }
            }
            futures_util::future::Either::Right((Some(event), _)) => {
                // Deactivated mirrors Activated on KWin; both count as a press
                // so a compositor that only emits one still toggles.
                if event.shortcut_id() == "toggle" && tx.send(map(HotkeyEvent::Toggle)).is_err() {
                    return Ok(());
                }
            }
            _ => return Ok(()),
        }
    }
}

/// A message to the cancel-key worker.
enum CancelMsg {
    /// Bind the cancel key; the reply carries the arm outcome.
    Arm { reply: std::sync::mpsc::Sender<Result<(), String>> },
    /// Release the cancel key.
    Disarm,
}

/// The cancel key, armed for the length of one recording.
///
/// The portal grab is permanent while a session lives, so this is the Windows
/// tray's cancel-key shape, not its RegisterHotKey-at-startup shape: claimed
/// before the orb shows, released when the dictation returns, and free in
/// every other app the rest of the time.
///
/// `CANCEL_KEY=none` returns a key whose arm and disarm are no-ops: the
/// Windows tray treats "none" as a configured state, not a failure.
pub struct CancelKey {
    tx: Option<std::sync::mpsc::Sender<CancelMsg>>,
}

impl CancelKey {
    /// Binds the cancel key, blocking until it is live. A failed arm is an
    /// error, not a fatal one: the recording still runs without it, the way
    /// the Windows tip records a failed registration and keeps going.
    pub fn arm(&self) -> Result<(), String> {
        let Some(tx) = self.tx.as_ref() else { return Ok(()) };
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        tx.send(CancelMsg::Arm { reply: reply_tx })
            .map_err(|_| "cancel-key worker is gone".to_string())?;
        match reply_rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(result) => result,
            Err(_) => Err("cancel key did not arm within 30s".into()),
        }
    }

    /// Releases the cancel key. Fire-and-forget: the worker serializes
    /// arm/disarm, so a following arm always lands after the release.
    pub fn disarm(&self) {
        if let Some(tx) = self.tx.as_ref() {
            let _ = tx.send(CancelMsg::Disarm);
        }
    }
}

/// Spawns the cancel-key worker. Never fails at startup: a portal refusal
/// surfaces per arm, where the recording can still proceed without it.
pub fn spawn_cancel_key<E, F>(
    cfg: &Option<Arc<config::Config>>,
    tx: Sender<E>,
    map: F,
) -> CancelKey
where
    E: Send + 'static,
    F: Fn(HotkeyEvent) -> E + Send + 'static + Clone,
{
    let cancel_str = match cfg.as_ref() {
        Some(c) => c.cancel_key_str.clone(),
        None => config::DEFAULT_CANCEL_STR.to_string(),
    };
    if cancel_str.eq_ignore_ascii_case("none") {
        return CancelKey { tx: None };
    }
    let (msg_tx, msg_rx) = std::sync::mpsc::channel::<CancelMsg>();
    let spawned = std::thread::Builder::new()
        .name("mnvoice-cancel".into())
        .spawn(move || cancel_worker(cancel_str, tx, map, msg_rx));
    if spawned.is_err() {
        // A worker that could not spawn leaves no key to arm; arm stays a
        // no-op and the recording runs without cancel, which the arm-time log
        // never gets to say. Thread spawn failing means the process is dying
        // anyway, so a silent no-op here beats a second error channel.
        return CancelKey { tx: None };
    }
    CancelKey { tx: Some(msg_tx) }
}

/// One armed cancel key: the portal session and the forwarder's stop flag.
struct CancelSession {
    session: Session<GlobalShortcuts>,
    stop: Arc<AtomicBool>,
}

impl CancelSession {
    /// Releases the grab: the forwarder stops on its next tick and the
    /// session closes, which is what unbinds the key from kglobalaccel.
    fn close(self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Err(e) = futures_executor::block_on(self.session.close()) {
            eprintln!("mnvoice: closing the cancel session failed ({e})");
        }
    }
}

/// The worker: one live session at a time, arm/disarm serialized through its
/// channel, and any session still armed when the app exits closed on the way
/// out so Esc is never left grabbed by a dead process.
fn cancel_worker<E, F>(
    cancel_str: String,
    tx: Sender<E>,
    map: F,
    rx: std::sync::mpsc::Receiver<CancelMsg>,
) where
    E: Send + 'static,
    F: Fn(HotkeyEvent) -> E + Send + 'static + Clone,
{
    let mut live: Option<CancelSession> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            CancelMsg::Arm { reply } => {
                if live.is_some() {
                    let _ = reply.send(Ok(()));
                    continue;
                }
                match futures_executor::block_on(arm_cancel_session(
                    &cancel_str,
                    tx.clone(),
                    map.clone(),
                )) {
                    Ok(session) => {
                        live = Some(session);
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            CancelMsg::Disarm => {
                if let Some(session) = live.take() {
                    session.close();
                }
            }
        }
    }
    if let Some(session) = live.take() {
        session.close();
    }
}

/// Binds the cancel key in its own portal session and spawns the thread that
/// forwards its presses. Returns the session so the caller can close it and
/// free the key again.
async fn arm_cancel_session<E, F>(
    cancel_str: &str,
    tx: Sender<E>,
    map: F,
) -> Result<CancelSession, String>
where
    E: Send + 'static,
    F: Fn(HotkeyEvent) -> E + Send + 'static,
{
    let gs = GlobalShortcuts::new()
        .await
        .map_err(|e| format!("GlobalShortcuts portal unavailable ({e})"))?;
    let shortcuts = [NewShortcut::new("cancel", "Cancel the recording")
        .preferred_trigger(Some(portal_trigger(cancel_str).as_str()))];
    let session = gs
        .create_session(ashpd::desktop::CreateSessionOptions::default())
        .await
        .map_err(|e| format!("cancel session refused ({e})"))?;
    gs.bind_shortcuts(
        &session,
        &shortcuts,
        None,
        ashpd::desktop::global_shortcuts::BindShortcutsOptions::default(),
    )
    .await
    .map_err(|e| format!("binding the cancel key failed ({e})"))?;

    let mut activated = gs
        .receive_activated()
        .await
        .map_err(|e| format!("listening for cancel presses failed ({e})"))?;
    let mut deactivated = gs
        .receive_deactivated()
        .await
        .map_err(|e| format!("listening for cancel releases failed ({e})"))?;
    let stop = Arc::new(AtomicBool::new(false));

    // The forwarder owns the event streams. The 200ms tick is what lets it
    // notice a disarm: the streams never end on their own, and a parked
    // forwarder per recording is a thread leak. KWin emits Activated and
    // Deactivated both for one press (see hotkey_loop), and both count -
    // raising the cancel flag twice is the same as raising it once.
    let fwd_stop = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("mnvoice-cancel-fwd".into())
        .spawn(move || {
            futures_executor::block_on(async move {
                loop {
                    let tick = Box::pin(async_io::Timer::after(std::time::Duration::from_millis(
                        200,
                    )));
                    let which = futures_util::future::select(
                        // Both streams reduce to the shortcut id: an or-pattern
                        // cannot bind Activated and Deactivated as one value.
                        Box::pin(futures_util::future::select(
                            Box::pin(async {
                                activated.next().await.map(|e| e.shortcut_id().to_string())
                            }),
                            Box::pin(async {
                                deactivated.next().await.map(|e| e.shortcut_id().to_string())
                            }),
                        )),
                        tick,
                    )
                    .await;
                    match which {
                        futures_util::future::Either::Left((inner, _)) => match inner {
                            futures_util::future::Either::Left((Some(id), _))
                            | futures_util::future::Either::Right((Some(id), _)) => {
                                if id == "cancel" && tx.send(map(HotkeyEvent::Cancel)).is_err() {
                                    return;
                                }
                            }
                            // The session is gone from the other side: nothing
                            // left to forward.
                            _ => return,
                        },
                        futures_util::future::Either::Right((_, _)) => {
                            if fwd_stop.load(Ordering::SeqCst) {
                                return;
                            }
                        }
                    }
                }
            })
        });
    if spawned.is_err() {
        return Err("could not spawn the cancel forwarder".into());
    }
    Ok(CancelSession { session, stop })
}

/// The Wayland typing session.
pub struct WaylandTyper {
    remote: RemoteDesktop,
    session: ashpd::desktop::Session<ashpd::desktop::remote_desktop::RemoteDesktop>,
}

impl WaylandTyper {
    /// Opens the RemoteDesktop session and asks for the keyboard.
    ///
    /// The compositor's consent dialog appears on the first `start`; with
    /// `PersistMode::ExplicitlyRevoked` the grant survives restarts, so this cost is
    /// once per user, not once per dictation.
    pub async fn open() -> Result<Self, String> {
        let remote = RemoteDesktop::new()
            .await
            .map_err(|e| format!("RemoteDesktop portal unavailable ({e})"))?;
        let session = remote
            .create_session(ashpd::desktop::CreateSessionOptions::default())
            .await
            .map_err(|e| format!("typing session refused ({e})"))?;
        let options = SelectDevicesOptions::default()
            .set_devices(BitFlags::from(DeviceType::Keyboard))
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        remote
            .select_devices(&session, options)
            .await
            .map_err(|e| format!("selecting the keyboard device failed ({e})"))?;
        remote
            .start(&session, None, StartOptions::default())
            .await
            .map_err(|e| format!("the compositor refused keyboard access ({e}); accept the dialog to let mnvoice type"))?;
        Ok(Self { remote, session })
    }

    /// Types `text` into whatever window has focus, one keysym press and
    /// release per character.
    ///
    /// The pacing matches the XTest injector's contract (`input::KEY_GAP`
    /// after each character): a burst of D-Bus messages can outpace the target
    /// window exactly like a burst of XTest events can, so the gap applies
    /// here too. The state and casing come from the character itself -
    /// `notify_keyboard_keysym` takes the keysym, and the compositor applies
    /// the modifier it implies.
    pub async fn type_text(&self, text: &str) -> Result<(), String> {
        for c in text.chars() {
            let keysym = xkbcommon::xkb::Keysym::from_char(c);
            self.remote
                .notify_keyboard_keysym(
                    &self.session,
                    keysym.raw() as i32,
                    ashpd::desktop::remote_desktop::KeyState::Pressed,
                    ashpd::desktop::remote_desktop::NotifyKeyboardKeysymOptions::default(),
                )
                .await
                .map_err(|e| format!("key injection failed ({e})"))?;
            self.remote
                .notify_keyboard_keysym(
                    &self.session,
                    keysym.raw() as i32,
                    ashpd::desktop::remote_desktop::KeyState::Released,
                    ashpd::desktop::remote_desktop::NotifyKeyboardKeysymOptions::default(),
                )
                .await
                .map_err(|e| format!("key injection failed ({e})"))?;
            std::thread::sleep(crate::platform::input::KEY_GAP);
        }
        Ok(())
    }
}
