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
use ashpd::desktop::PersistMode;
use enumflags2::BitFlags;
use futures_util::StreamExt;
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

/// Whether a key can be bound through the portal without taking it away from
/// every other application.
///
/// A `GlobalShortcuts` binding is grabbed by kglobalaccel for the whole life of
/// the process, and closing the session does NOT give it back (measured
/// 2026-10-20: the bare-Esc grab stayed live after the session was closed and
/// only disappeared when mnvoice exited). So a key with no modifier is a
/// different class of mistake here than anywhere else in the config: not a
/// setting that does not work, but a system-wide key the user cannot press in
/// any other app until mnvoice quits. Windows can bind a bare Esc because it
/// registers per recording and unregisters at the stop (`windows_app::toggle`);
/// the portal has no equivalent, so this platform declines the binding instead.
///
/// `MOD_NOREPEAT` is the parser's own flag on every key, not a modifier the user
/// pressed, so a real chord is any other bit set: Alt, Ctrl, Shift or Win.
fn has_modifier(modifiers: u32) -> bool {
    modifiers & !config::MOD_NOREPEAT != 0
}

/// The warning a bare toggle key deserves, or `None` for a chord.
///
/// The opposite decision from `cancel_binding`, and for one reason: refusing the
/// cancel key still leaves two ways to stop a recording (the toggle and the
/// tray's "Stop & transcribe"), while refusing the toggle would leave the app
/// running with no way in at all. A bare toggle key is also a far less common
/// choice than a bare Esc was, and less damaging: `A` steals a letter, `Space`
/// steals a key the user is unlikely to need while dictating into an app.
/// So this says what is happening instead of declining - and returns the words
/// rather than printing them, so the choice is something a test can pin.
fn bare_toggle_warning(hotkey_str: &str) -> Option<String> {
    let bare = match config::parse_hotkey(hotkey_str) {
        Some((modifiers, _)) => !has_modifier(modifiers),
        None => false,
    };
    bare.then(|| {
        format!(
            "mnvoice: HOTKEY={hotkey_str} is a bare key; the desktop portal keeps it for every \
             app until mnvoice quits, so typing it elsewhere will not work. A chord such as \
             Alt+Space avoids that."
        )
    })
}

/// The cancel key's portal trigger, or `None` when binding it would steal a
/// bare key from every other app.
///
/// "none" and an unspellable key are already handled by the config (they yield
/// the sentinel pair and the default string), so this is the last gate: it reads
/// the same string the bind site sends to the portal, and refuses exactly what
/// would hurt - a key with no modifier. The refusal is a skipped binding, not a
/// dead app: the dictation key still binds, and the user can stop a recording
/// with the toggle key or the tray row as on any build without a cancel key.
fn cancel_binding(cancel_str: &str) -> Option<String> {
    if cancel_str.eq_ignore_ascii_case("none") {
        return None;
    }
    match config::parse_hotkey(cancel_str) {
        Some((modifiers, _)) if has_modifier(modifiers) => Some(portal_trigger(cancel_str)),
        Some(_) => {
            eprintln!(
                "mnvoice: CANCEL_KEY={cancel_str} is a bare key; the desktop portal grabs it \
                 for every app until mnvoice quits, so it is not bound. \
                 Use a chord (Alt+Escape) or CANCEL_KEY=none."
            );
            None
        }
        // Unspellable: the config already swapped in its own default, so this
        // only happens if a caller hands the raw spelling over.
        None => None,
    }
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
    let (hotkey_str, cancel_str) = match cfg.as_ref() {
        Some(c) => (c.hotkey_str.clone(), c.cancel_key_str.clone()),
        None => (
            config::DEFAULT_HOTKEY_STR.to_string(),
            config::DEFAULT_CANCEL_STR.to_string(),
        ),
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
        let result = futures_executor::block_on(hotkey_loop(
            hotkey_str, cancel_str, tx, map, stop_rx, ready_tx,
        ));
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

/// The listener's body: one portal session with both shortcuts bound, signals
/// forwarded to the app until the stop channel closes.
///
/// Both keys are bound here, once, for the process's life. Measured on
/// Kubuntu/KDE 2026-10-20: closing a GlobalShortcuts session does NOT release
/// KDE's kglobalaccel grab - it stayed live after the session was closed and
/// only went away when the process exited. So an arm/disarm shape cannot work:
/// whatever key is bound here is grabbed until mnvoice stops, which is why
/// CANCEL_KEY defaults to a chord (config's DEFAULT_CANCEL_STR) and Esc is
/// never bound by this app.
async fn hotkey_loop<E, F>(
    hotkey_str: String,
    cancel_str: String,
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

    let mut shortcuts = vec![NewShortcut::new("toggle", "Start or stop dictation")
        .preferred_trigger(Some(portal_trigger(&hotkey_str).as_str()))];
    // A bare toggle is bound anyway (see `bare_toggle_warning`), but said.
    if let Some(warning) = bare_toggle_warning(&hotkey_str) {
        eprintln!("{warning}");
    }
    // The guard, not the config, is what keeps a bare key out of the grab: a
    // user who copies `CANCEL_KEY=Escape` from the old README gets the refusal
    // logged and the dictation key still bound.
    if let Some(trigger) = cancel_binding(&cancel_str) {
        shortcuts.push(
            NewShortcut::new("cancel", "Cancel the recording")
                .preferred_trigger(Some(trigger.as_str())),
        );
    }

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
    // Deactivated (key release) is still received so the signal stream stays
    // drained, but it must NOT toggle: counting press + release as two presses
    // double-fires every physical tap (measured: one Alt+Space produced
    // `stopped (0s elapsed)` immediately followed by `recording...`, leaving
    // an unattended recording that ran to its 90s cap with the orb stuck up).
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
            futures_util::future::Either::Right((Some(_event), _)) => {
                // Key release: drained, deliberately ignored. See above.
            }
            _ => return Ok(()),
        }
    }
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
        let typed = latin1_typing_chars(text);
        if typed != text {
            eprintln!(
                "mnvoice: typing {} chars as plain keyboard text (smart punctuation and untypeable characters normalized)",
                typed.chars().count()
            );
        }
        for c in typed.chars() {
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

/// Rewrites `text` into what a portal typing session can deliver without a
/// keymap rebuild, in order, dropping what cannot be typed at all.
///
/// Why this exists: a character outside the active layout (the smart
/// punctuation a transcription model emits - em dash, curly quotes, ellipsis)
/// has no key in the compositor's keymap. KWin then REBUILDS the keymap
/// around a spare keycode to type it and rebuilds back, and the rebuild
/// resets the whole XKB state: any modifier the user is physically holding at
/// that moment is wiped mid-hold (`MODIFIERS depressed=0` at the focused
/// window), and the eventual release arrives against a keymap that changed
/// twice, so clients cannot match it. Measured in a private KWin 6.6.6 on
/// 2026-10-27: one em dash driven through `org_kde_kwin_fake_input` while
/// Alt_L was held produced two keymap swaps, a depressed=0 modifiers event
/// and an Alt release with keysym 0. A dictation transcript types over
/// seconds, overlapping the user's own keystrokes, so this is the mechanism
/// behind both "garbled words" and "a modifier feels stuck after a
/// dictation".
///
/// The replacement set is the punctuation a transcription model actually
/// emits; each maps to what a user would have typed on a us keyboard.
/// Zero-width and soft-hyphen controls are dropped by the explicit list
/// below, not because they lack a keysym. Only printable ASCII is keymap
/// safe: everything else that still resolves to a keysym - accented
/// Latin-1, CJK, emoji - is kept and types through KWin's keymap-rebuild
/// path, which beats dropping words.
pub fn latin1_typing_chars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            // Dashes, quotes and ellipsis: what the model emits, as the keys
            // a user would have pressed.
            '\u{2013}' | '\u{2014}' | '\u{2212}' => out.push('-'),
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => out.push('\''),
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => out.push('"'),
            '\u{2026}' => out.push_str("..."),
            '\u{00a0}' => out.push(' '),
            // Not typeable on any keyboard: skip rather than inject noise.
            '\u{00ad}' | '\u{200b}' | '\u{feff}' => {}
            other => {
                if xkbcommon::xkb::Keysym::from_char(other) != xkbcommon::xkb::Keysym::NoSymbol {
                    // Real content outside printable ASCII (accented
                    // Latin-1, CJK, emoji): it still types, through the
                    // keymap-rebuild path.
                    out.push(other);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_punctuation_types_without_a_keymap_reload() {
        // The rule that keeps the compositor's keymap (and the user's held
        // modifiers) untouched while a transcript types. Every character the
        // model's smart formatting emits must come out as a key a us layout
        // already has - anything else sends KWin through its keymap-rebuild
        // path, which wipes held modifiers and mismaps the next releases
        // (measured against a private KWin 6.6.6, see the function's note).
        let transcript = "He said \u{2018}ready\u{2019} \u{2014} then waited\u{2026} \u{2013} gone";
        assert_eq!(latin1_typing_chars(transcript), "He said 'ready' - then waited... - gone");
        assert_eq!(latin1_typing_chars("\u{201c}quoted\u{201d}"), "\"quoted\"");
        assert_eq!(latin1_typing_chars("it\u{2019}s"), "it's");
        // Plain text passes through untouched.
        assert_eq!(latin1_typing_chars("hello, world! 123"), "hello, world! 123");
    }

    #[test]
    fn untypeable_characters_dont_reach_the_portal() {
        // Zero-width and soft-hyphen controls are removed by the explicit
        // drop list, not by a missing keysym: from_char gives every one of
        // them a Unicode keysym, so only the list keeps them off the wire.
        assert_eq!(
            latin1_typing_chars("a\u{feff}b\u{200b}c\u{ad}d"),
            "abcd"
        );
    }

    #[test]
    fn real_content_outside_printable_ascii_still_types() {
        // Dropping words is worse than a keymap rebuild: CJK, accented text
        // and emoji all resolve to keysyms and keep their characters, even
        // though they type through the rebuild path.
        assert_eq!(
            latin1_typing_chars("caf\u{e9} \u{4e2d}\u{6587} \u{1f600}"),
            "caf\u{e9} \u{4e2d}\u{6587} \u{1f600}"
        );
    }

    #[test]
    fn the_log_decision_is_the_mapping_changing_the_text() {
        // What `type_text` compares to decide whether to log: the transcript
        // the mapping produced against the one it was given. Text that needs
        // no reshaping comes back byte-identical, so the log stays silent;
        // anything the model's smart formatting touched, or the drop list
        // removed, is not.
        assert_eq!(latin1_typing_chars("plain words"), "plain words");
        assert_ne!(latin1_typing_chars("em \u{2014} dash"), "em \u{2014} dash");
        assert_ne!(
            latin1_typing_chars("dropped \u{feff} mark"),
            "dropped \u{feff} mark"
        );
        // Content that types through the rebuild path is unchanged, so it
        // does not claim a reshape that never happened.
        assert_eq!(
            latin1_typing_chars("caf\u{e9} \u{4e2d}\u{6587} \u{1f600}"),
            "caf\u{e9} \u{4e2d}\u{6587} \u{1f600}"
        );
    }

    #[test]
    fn a_chord_is_the_only_thing_the_portal_will_grab_for_good() {
        // The rule that keeps the user's Esc. A binding here lasts the process's
        // whole life (measured 2026-10-20), so a key with no modifier must never
        // reach the portal - not from the default, not from a config a user
        // copied out of the old README.
        assert_eq!(cancel_binding("Escape").as_deref(), None);
        assert_eq!(cancel_binding("esc").as_deref(), None);
        assert_eq!(
            cancel_binding("Alt+Escape").as_deref(),
            Some("ALT+ESCAPE"),
            "the default spelling binds"
        );
        assert_eq!(
            cancel_binding("Ctrl+Shift+D").as_deref(),
            Some("CTRL+SHIFT+D")
        );
    }

    #[test]
    fn none_and_garbage_bind_nothing() {
        assert_eq!(cancel_binding("none").as_deref(), None);
        assert_eq!(cancel_binding("NONE").as_deref(), None);
        // The config replaces an unspellable value with its default before it
        // gets here; the raw spelling binds nothing rather than grabbing a key
        // nobody asked for.
        assert_eq!(cancel_binding("Foobar").as_deref(), None);
    }

    #[test]
    fn no_modifier_is_not_a_chord() {
        // MOD_NOREPEAT rides on every parsed key, so "has a modifier" cannot
        // mean "non-zero": bare Escape is (MOD_NOREPEAT, 0x1B).
        assert!(!has_modifier(config::MOD_NOREPEAT));
        assert!(has_modifier(config::MOD_ALT | config::MOD_NOREPEAT));
        let bare = config::parse_hotkey("Escape").unwrap();
        let chord = config::parse_hotkey("Alt+Escape").unwrap();
        assert!(!has_modifier(bare.0), "bare Esc is {bare:?}");
        assert!(has_modifier(chord.0), "Alt+Esc is {chord:?}");
    }

    #[test]
    fn a_bare_toggle_is_warned_about_and_still_bound() {
        // Refusing the toggle would leave the app with no way in, so a bare
        // toggle key is bound and said out loud instead - the opposite of the
        // cancel rule above, and the reason that rule is safe to enforce.
        let warning = bare_toggle_warning("Space").expect("a bare toggle warns");
        assert!(warning.contains("HOTKEY=Space"), "{warning}");
        assert!(warning.contains("bare key"), "{warning}");
        // The shipped default, and any chord, says nothing.
        assert!(bare_toggle_warning("Alt+Space").is_none());
        assert!(bare_toggle_warning("none").is_none());
    }
}
