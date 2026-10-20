// The Linux tray: a StatusNotifierItem, the surface the Windows tray has a
// direct equivalent of on KDE/freedesktop desktops.
//
// The tray is the StatusNotifierItem protocol (what Plasma's system tray
// consumes), driven by `ksni` with its `blocking` API so no async runtime
// reaches the app: the D-Bus service lives on ksni's own thread and every
// choice arrives at the control loop over a plain `std::sync::mpsc` channel,
// as the same `AppEvent` the hotkey produces. One channel, one message loop.
//
// Two differences from the Windows tray are deliberate and stated rather than
// papered over:
//
//   - No "Check for updates" and no "Start with Windows" checkbox. The
//     self-updater is Windows-only in this release (`update.rs` is gated to
//     it), and autostart on Linux is the desktop file the app writes at
//     startup (`linux_app::ensure_autostart`), not a checkbox that can flip a
//     task. A menu item that could not do the thing would be a lie.
//   - The icon is the orb, not a separate asset: `orb_render` paints the same
//     pixels and the tray blits them, so one render serves both surfaces. It is
//     a still, not an animation - see `icon_pixmap`.

use crate::orb_render::{render_gas_fluid, OrbState, ORB_HEIGHT, ORB_WIDTH};
use crate::platform::linux_app::AppEvent;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, Status, ToolTip, Tray};
use std::sync::mpsc::Sender;
use std::sync::Arc;

/// Which recording control the user asked for, before it becomes an app event.
///
/// Named apart from `AppEvent` because the menu's rows are about dictation,
/// while `AppEvent` is about the process: keeping them separate is what lets a
/// test assert that the row a user sees and the command it sends are the same
/// pair, which is the thing that goes wrong when a menu is edited.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TrayCommand {
    /// The same thing the toggle hotkey does: start, or stop, a dictation.
    Toggle,
    /// Stop this process and launch a fresh one (the tray's "Restart").
    Restart,
    /// Stop this process and leave it stopped (the tray's "Exit").
    Quit,
}

/// The label the user reads for a command. The one place each command's label
/// is spelled, so the menu and the tests that assert on it cannot drift.
fn label(command: TrayCommand, recording: bool) -> &'static str {
    match command {
        // While a dictation runs the first row stops it, exactly like the
        // Windows tray's "Stop & transcribe".
        TrayCommand::Toggle if recording => "Stop & transcribe",
        TrayCommand::Toggle => "Dictate",
        TrayCommand::Restart => "Restart",
        TrayCommand::Quit => "Quit",
    }
}

/// The app event a command becomes. The tray offers recording controls and
/// process controls; the app names both, and this is the one boundary where the
/// two names meet.
fn event(command: TrayCommand) -> AppEvent {
    match command {
        TrayCommand::Toggle => AppEvent::Toggle,
        TrayCommand::Restart => AppEvent::Restart,
        TrayCommand::Quit => AppEvent::Quit,
    }
}

/// The tray. `ksni` renders it on its own thread; every menu callback only
/// sends on `commands`, which is what keeps the D-Bus service from doing any of
/// the app's work - and from freezing the menu while it tried.
#[derive(Clone)]
pub struct MnvoiceTray {
    commands: Sender<AppEvent>,
    /// Whether a dictation is in flight, read when the menu is built or the
    /// icon is painted. A `fn`-shaped read rather than a copy of the flag
    /// because the panel asks for these at any moment, not when we last knew.
    recording: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The orb's look, from the config: the tray icon IS the orb.
    color: (f32, f32, f32),
    fluid_level: f32,
    /// The hotkey's display spelling, for the tooltip that says what to press.
    hotkey_str: String,
    /// Which still of the breathing orb the icon shows. Advanced on refresh
    /// (see `TrayHandle::refresh`) so a repaint is not a no-op to a compositor
    /// that compares pixmaps.
    frame: u32,
}

/// A still from mid-breath: frame 0 is the dimmest point of the cycle, which
/// reads as an empty orb at tray-icon size.
const DEFAULT_FRAME: u32 = 10;

impl MnvoiceTray {
    pub fn new(
        commands: Sender<AppEvent>,
        recording: Arc<dyn Fn() -> bool + Send + Sync>,
        color: (f32, f32, f32),
        fluid_level: f32,
        hotkey_str: String,
    ) -> Self {
        Self {
            commands,
            recording,
            color,
            fluid_level,
            hotkey_str,
            frame: DEFAULT_FRAME,
        }
    }
}

impl Tray for MnvoiceTray {
    /// Left click opens the menu, the convention for a status-only item: there
    /// is no window to raise, so an activation has nowhere else to go.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "mnvoice".into()
    }

    fn title(&self) -> String {
        if (self.recording)() {
            "mnvoice - recording".into()
        } else {
            "mnvoice".into()
        }
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn tool_tip(&self) -> ToolTip {
        // A disabled hotkey has no key to name; saying "Alt+Space" then would
        // point the user at a press that does nothing.
        let description = if self.hotkey_str.eq_ignore_ascii_case("none") {
            "Push-to-talk dictation".into()
        } else {
            format!(
                "Push-to-talk dictation - {} to start or stop",
                self.hotkey_str
            )
        };
        ToolTip {
            title: "mnvoice".into(),
            description,
            ..Default::default()
        }
    }

    /// The tray icon is the orb: the indicator the user watches is the one that
    /// already means "recording" in the overlay, painted by the same renderer.
    ///
    /// `ksni` wants the bytes A,R,G,B per pixel, which is the big-endian
    /// spelling of the `u32` the shared renderer packs, so no re-packing - just
    /// a byte order. A still rather than an animation: driving a 30 fps timer
    /// through D-Bus to move a 22px tray icon costs far more than the motion is
    /// worth when the full-size orb on screen already breathes.
    fn icon_pixmap(&self) -> Vec<Icon> {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_gas_fluid(
            self.frame,
            &mut buf,
            if (self.recording)() {
                OrbState::Recording
            } else {
                // At rest the orb is hidden, so the icon takes the quieter
                // pose: the two states differ in how fast the fluid drifts,
                // and a still of the fast one looks mid-sentence.
                OrbState::Transcribing
            },
            self.color,
            self.fluid_level,
        );
        let data = buf.iter().flat_map(|p| p.to_be_bytes()).collect();
        vec![Icon {
            width: ORB_WIDTH as i32,
            height: ORB_HEIGHT as i32,
            data,
        }]
    }

    /// The menu, in the Windows tray's order: the recording control, a
    /// separator, then the app's own controls. Every row is built from `label`,
    /// so a label cannot drift from the command it belongs to.
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let recording = (self.recording)();
        let mut items = Vec::new();
        for (index, command) in [TrayCommand::Toggle, TrayCommand::Restart, TrayCommand::Quit]
            .into_iter()
            .enumerate()
        {
            if index == 1 {
                // The separator splits the recording control from the app's,
                // as the Windows menu draws it.
                items.push(MenuItem::Separator);
            }
            let text = label(command, recording);
            let sender = self.commands.clone();
            items.push(
                StandardItem {
                    label: text.into(),
                    icon_name: if matches!(command, TrayCommand::Quit) {
                        "application-exit".into()
                    } else {
                        String::new()
                    },
                    activate: Box::new(move |_| {
                        // A closed channel means the loop is gone; the click
                        // then does nothing, which is the honest outcome, and
                        // the process is on its way out anyway.
                        let _ = sender.send(event(command));
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        items
    }
}

/// A handle to the running tray service, kept alive by the app for as long as
/// the item should exist.
pub struct TrayHandle {
    inner: ksni::blocking::Handle<MnvoiceTray>,
}

impl TrayHandle {
    /// Asks the panel to re-read the item - the tray's equivalent of the
    /// `NIM_MODIFY` the Windows tray sends when the state changes. `ksni`
    /// emits the property-change and layout-change signals, so the menu label,
    /// the title and the icon follow the recording.
    ///
    /// The frame counter moves with it: a compositor that compares pixmaps
    /// would otherwise see identical bytes and skip the repaint.
    pub fn refresh(&self) {
        let _ = self
            .inner
            .update(|tray| tray.frame = tray.frame.wrapping_add(1));
    }
}

/// Registers the tray. The service runs on ksni's own thread; choices reach the
/// control loop through `commands`.
///
/// A desktop whose tray host is not up yet is not fatal: an app that starts
/// before Plasma has finished drawing the panel would otherwise die for want of
/// an indicator. The failure is returned for the caller to log, exactly as the
/// orb's missing Wayland connection is.
pub fn spawn(
    commands: Sender<AppEvent>,
    recording: Arc<dyn Fn() -> bool + Send + Sync>,
    color: (f32, f32, f32),
    fluid_level: f32,
    hotkey_str: String,
) -> Result<TrayHandle, String> {
    use ksni::blocking::TrayMethods;
    let tray = MnvoiceTray::new(commands, recording, color, fluid_level, hotkey_str);
    let inner = tray
        .spawn()
        .map_err(|e| format!("system tray unavailable ({e})"))?;
    Ok(TrayHandle { inner })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_ORB_COLOR, DEFAULT_ORB_FLUID_LEVEL};

    /// A tray with no D-Bus behind it: the menu and the pixmap are plain data,
    /// so the labels the user reads and the commands they send can be pinned
    /// without a panel running.
    fn tray(recording: bool) -> (MnvoiceTray, std::sync::mpsc::Receiver<AppEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let t = MnvoiceTray::new(
            tx,
            Arc::new(move || recording),
            DEFAULT_ORB_COLOR,
            DEFAULT_ORB_FLUID_LEVEL,
            "Alt+Space".into(),
        );
        (t, rx)
    }

    /// The standard rows in order, skipping the separator.
    fn rows(menu: &[MenuItem<MnvoiceTray>]) -> Vec<&str> {
        menu.iter()
            .filter_map(|item| match item {
                MenuItem::Standard(row) => Some(row.label.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_first_row_follows_the_recording() {
        let (idle, _) = tray(false);
        assert_eq!(rows(&idle.menu()), vec!["Dictate", "Restart", "Quit"]);
        let (live, _) = tray(true);
        assert_eq!(
            rows(&live.menu()),
            vec!["Stop & transcribe", "Restart", "Quit"]
        );
    }

    #[test]
    fn the_menu_carries_the_entries_that_have_a_linux_answer() {
        let (t, _) = tray(false);
        let menu = t.menu();
        // Four rows: the three commands and the separator that splits the
        // recording control from the app's. No "Check for updates" and no
        // autostart checkbox - the updater is Windows-only and the autostart
        // entry is written at startup, so an item that could not do the thing
        // would be a lie.
        assert_eq!(menu.len(), 4);
        assert!(matches!(menu[1], MenuItem::Separator));
    }

    #[test]
    fn a_row_and_the_command_it_sends_are_the_same_pair() {
        // The defect this prevents is the menu being edited so the label says
        // The defect this prevents is the menu being edited so the label says
        // one thing and the click sends another: `label` and `event` are the
        // one place each is named, and this walks the menu comparing what the
        // label produced to what the callback delivered.
        let (t, rx) = tray(false);
        let menu = t.menu();
        let sent = [AppEvent::Toggle, AppEvent::Restart, AppEvent::Quit];
        let seen = ["Dictate", "Restart", "Quit"];
        let mut index = 0;
        for item in &menu {
            let MenuItem::Standard(std) = item else {
                continue;
            };
            assert_eq!(std.label, seen[index]);
            // `activate` takes &mut the tray; a throwaway copy keeps the menu
            // alive for the next row.
            (std.activate)(&mut t.clone());
            assert_eq!(rx.recv().unwrap(), sent[index]);
            index += 1;
        }
        assert_eq!(index, 3, "every row was walked");
        assert!(rx.try_recv().is_err(), "one click, one event");
    }

    #[test]
    fn the_tray_icon_is_the_orb_painted_by_the_shared_renderer() {
        let (t, _) = tray(true);
        let pixmap = t.icon_pixmap();
        assert_eq!(pixmap.len(), 1, "one size: the orb");
        let icon = &pixmap[0];
        assert_eq!(icon.width as u32, ORB_WIDTH);
        assert_eq!(icon.height as u32, ORB_HEIGHT);
        assert_eq!(
            icon.data.len(),
            (ORB_WIDTH * ORB_HEIGHT * 4) as usize,
            "ARGB32, one word per pixel"
        );
        // An all-transparent buffer passes the length check and ships an
        // invisible icon, so the body must have alpha - the same assertion the
        // renderer's own tests make, kept here because the byte order is the
        // tray's added contract.
        let opaque = icon.data.chunks(4).filter(|px| px[0] > 40).count();
        assert!(
            opaque > 150,
            "the tray icon must be visible, got {opaque} opaque pixels"
        );
    }

    #[test]
    fn the_tooltip_names_the_hotkey_and_says_when_there_is_none() {
        let (t, _) = tray(false);
        assert!(t.tool_tip().description.contains("Alt+Space"));

        let (tx, _rx) = std::sync::mpsc::channel();
        let no_key = MnvoiceTray::new(
            tx,
            Arc::new(|| false),
            DEFAULT_ORB_COLOR,
            DEFAULT_ORB_FLUID_LEVEL,
            "none".into(),
        );
        // A tooltip naming a key that does nothing is worse than no key text.
        assert!(!no_key.tool_tip().description.contains("none"));
        assert_eq!(no_key.tool_tip().description, "Push-to-talk dictation");
    }

    #[test]
    fn the_item_is_named_for_the_app_and_shows_as_active() {
        let (t, _) = tray(false);
        assert_eq!(t.id(), "mnvoice");
        assert_eq!(t.title(), "mnvoice");
        assert!(matches!(t.status(), Status::Active));
        let (live, _) = tray(true);
        assert_eq!(live.title(), "mnvoice - recording");
    }
}
