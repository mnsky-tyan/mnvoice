// The Linux tray: a StatusNotifierItem, the surface the Windows tray has a
// direct equivalent of on KDE/freedesktop desktops.
//
// The tray is the StatusNotifierItem protocol (what Plasma's system tray
// consumes), driven by `ksni` with its `blocking` API so no async runtime
// reaches the app: the D-Bus service lives on ksni's own thread and every
// choice arrives at the control loop over a plain `std::sync::mpsc` channel,
// as the same `AppEvent` the hotkey produces. One channel, one message loop.
//
// One difference from the Windows tray is deliberate and stated rather than
// papered over:
//
//   - No "Check for updates" and no "Start with Windows" checkbox. The
//     self-updater is Windows-only in this release (`update.rs` is gated to
//     it), and autostart on Linux is the desktop file the app writes at
//     startup (`linux_app::ensure_autostart`), not a checkbox that can flip a
//     task. A menu item that could not do the thing would be a lie.
//
// The icon is the brand card - the mark with the "mnvoice" wordmark beneath it -
// baked from `assets/mnvoice-linux.ico` by `build.rs` and served by
// `linux_icon` at seventeen sizes. The wordmark belongs in the icon, and the
// dense size ladder is what keeps it sharp: the host picks the frame nearest
// the size it draws at instead of resampling one, which is what made the
// wordmark look blurred before. The orb survives only as the fallback a build
// without a baked icon serves; see `icon_pixmap`.

use crate::orb_render::{render_gas_fluid, OrbState, ORB_HEIGHT, ORB_WIDTH};
use crate::platform::linux_app::AppEvent;
use crate::platform::linux_app::{PHASE_RECORDING, PHASE_TRANSCRIBING};
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
///
/// The first row has a label per phase, as the Windows tray's `dictate_item`
/// does: what a click will do depends on what is happening, and a row that
/// offered "Dictate" during a capture would start a second one if clicked.
fn label(command: TrayCommand, phase: u8) -> &'static str {
    match command {
        TrayCommand::Toggle => match phase {
            PHASE_RECORDING => "Stop & transcribe",
            PHASE_TRANSCRIBING => "Transcribing...",
            _ => "Dictate",
        },
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
    /// What the app is doing, read when the menu is built or the icon painted.
    /// A `fn`-shaped read rather than a copy of the value because the panel
    /// asks for these at any moment, not when we last knew.
    phase: Arc<dyn Fn() -> u8 + Send + Sync>,
    /// The orb's look, from the config: what the fallback icon paints with
    /// when the build carries no brand mark.
    color: (f32, f32, f32),
    fluid_level: f32,
    /// The hotkey's display spelling, for the tooltip that says what to press.
    hotkey_str: String,
    /// Which still of the breathing orb the fallback icon shows. Advanced on
    /// refresh (see `TrayHandle::refresh`); the brand pixmap is identical on
    /// every call, so this only matters while there is no baked mark.
    frame: u32,
}

/// A still from mid-breath: frame 0 is the dimmest point of the cycle, which
/// reads as an empty orb at tray-icon size.
const DEFAULT_FRAME: u32 = 10;

impl MnvoiceTray {
    pub fn new(
        commands: Sender<AppEvent>,
        phase: Arc<dyn Fn() -> u8 + Send + Sync>,
        color: (f32, f32, f32),
        fluid_level: f32,
        hotkey_str: String,
    ) -> Self {
        Self {
            commands,
            phase,
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
        match (self.phase)() {
            PHASE_RECORDING => "mnvoice - recording".into(),
            PHASE_TRANSCRIBING => "mnvoice - transcribing".into(),
            _ => "mnvoice".into(),
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

    /// The tray icon is the brand mark, not the orb: this is the surface a
    /// Linux desktop identifies the app by. The mark is mark-only at every
    /// size - the wordmark card lives in the launcher icon instead - because
    /// the tray is drawn small (Plasma's status popup measured at ~94 device
    /// px), where the wordmark is unreadable mush. The orb stays on screen as
    /// the live indicator, where motion carries the state; a still of it in
    /// the panel said nothing and was not the product's icon.
    ///
    /// A still rather than an animation either way: driving a 30 fps timer
    /// through D-Bus to move a 22px tray icon costs far more than the motion
    /// is worth when the full-size orb on screen already breathes.
    fn icon_pixmap(&self) -> Vec<Icon> {
        // Every baked frame, not just the nearest one: the StatusNotifierItem
        // spec's IconPixmap is an array so the host can pick the frame that
        // matches the size it draws at, and ksni only re-emits the signal when
        // the content actually changes (it hashes it), so the whole set costs
        // one send at registration rather than one per refresh.
        //
        // The result is the responsive set doing its job: a 22px panel icon gets
        // the 24px drawing, a 44px HiDPI one gets 44px exactly, and the status
        // popup's ~94px gets 96px. Every frame is the same card, drawn for its
        // own size, so the wordmark is the artwork's rather than a resample of
        // a bigger one - resampling it is what looked blurred.
        let frames = crate::platform::linux_icon::all_frames();
        if frames.is_empty() {
            // No baked brand mark (a checkout whose tray `.ico` was missing -
            // the build script warns and continues): the orb is still a better
            // tray icon than nothing, so the app keeps identifying itself.
            return orb_pixmap((self.phase)(), self.frame, self.color, self.fluid_level);
        }
        frames
            .into_iter()
            .map(|(width, height, rgba)| {
                // ksni wants A,R,G,B per pixel; the baked frames are straight
                // RGBA. `as_chunks` states the four-byte pixel width in the
                // type. It still excludes a trailing partial pixel from the
                // slices, so the assertion is what actually guards the
                // invariant - and it is an `assert!`, not a debug-only one,
                // because a short buffer would otherwise be truncated
                // silently in a release build. The store is compile-time
                // baked, so the check costs one comparison at start-up.
                let (pixels, remainder) = rgba.as_chunks::<4>();
                assert!(remainder.is_empty(), "RGBA buffer is not whole pixels");
                Icon {
                    width: width as i32,
                    height: height as i32,
                    data: pixels
                        .iter()
                        .flat_map(|px| [px[3], px[0], px[1], px[2]])
                        .collect(),
                }
            })
            .collect()
    }

    /// The menu, in the Windows tray's order: the recording control, a
    /// separator, then the app's own controls. Every row is built from `label`,
    /// so a label cannot drift from the command it belongs to.
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let phase = (self.phase)();
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
            let text = label(command, phase);
            let sender = self.commands.clone();
            items.push(
                StandardItem {
                    label: text.into(),
                    // A click during transcription cannot transcribe faster, and
                    // a row that offered a control that does nothing would read
                    // as a broken click - the Windows row is greyed exactly
                    // here (`dictate_item`).
                    enabled: !(phase == PHASE_TRANSCRIBING && command == TrayCommand::Toggle),
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

/// The pre-brand tray icon: the orb, painted by the shared renderer.
///
/// Kept as the fallback `icon_pixmap` calls, so a build without the brand asset
/// is still covered rather than silently shipping an empty tray. A free function
/// rather than a method because `Tray` has exactly one icon slot: an inherent
/// method would need a second `impl` block for the same type, and a second trait
/// method would advertise an API the trait does not have.
fn orb_pixmap(phase: u8, frame: u32, color: (f32, f32, f32), fluid_level: f32) -> Vec<Icon> {
    let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
    render_gas_fluid(
        frame,
        &mut buf,
        if phase == PHASE_RECORDING {
            OrbState::Recording
        } else {
            // At rest the orb is hidden, so the icon takes the quieter pose:
            // the two states differ in how fast the fluid drifts, and a still
            // of the fast one looks mid-sentence.
            OrbState::Transcribing
        },
        color,
        fluid_level,
    );
    let data = buf.iter().flat_map(|p| p.to_be_bytes()).collect();
    vec![Icon {
        width: ORB_WIDTH as i32,
        height: ORB_HEIGHT as i32,
        data,
    }]
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
    /// The frame counter moves with it for the fallback icon's sake: a
    /// compositor that compares pixmaps would otherwise see identical bytes
    /// and skip the repaint. The brand pixmap is identical on every refresh,
    /// and the label and title - what a refresh is really for - still follow.
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
    phase: Arc<dyn Fn() -> u8 + Send + Sync>,
    color: (f32, f32, f32),
    fluid_level: f32,
    hotkey_str: String,
) -> Result<TrayHandle, String> {
    use ksni::blocking::TrayMethods;
    let tray = MnvoiceTray::new(commands, phase, color, fluid_level, hotkey_str);
    let inner = tray
        .spawn()
        .map_err(|e| format!("system tray unavailable ({e})"))?;
    Ok(TrayHandle { inner })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_ORB_COLOR, DEFAULT_ORB_FLUID_LEVEL};
    use crate::platform::linux_app::PHASE_IDLE;

    /// A tray with no D-Bus behind it: the menu and the pixmap are plain data,
    /// so the labels the user reads and the commands they send can be pinned
    /// without a panel running.
    fn tray(phase: u8) -> (MnvoiceTray, std::sync::mpsc::Receiver<AppEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let t = MnvoiceTray::new(
            tx,
            Arc::new(move || phase),
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
    fn the_first_row_names_each_phase() {
        let (idle, _) = tray(PHASE_IDLE);
        assert_eq!(rows(&idle.menu()), vec!["Dictate", "Restart", "Quit"]);
        let (live, _) = tray(PHASE_RECORDING);
        assert_eq!(
            rows(&live.menu()),
            vec!["Stop & transcribe", "Restart", "Quit"]
        );
        let (typing, _) = tray(PHASE_TRANSCRIBING);
        let menu = typing.menu();
        let rows_live = rows(&menu);
        assert_eq!(
            rows_live,
            vec!["Transcribing...", "Restart", "Quit"],
            "the row named what the app was doing, so a hotkey press that \
             dropped the transcript was not something a click could do"
        );
        // And it is not clickable either, as on Windows.
        for item in &menu {
            if let MenuItem::Standard(row) = item {
                if row.label == "Transcribing..." {
                    assert!(!row.enabled);
                }
            }
        }
    }

    #[test]
    fn the_menu_carries_the_entries_that_have_a_linux_answer() {
        let (t, _) = tray(PHASE_IDLE);
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
        // one thing and the click sends another: `label` and `event` are the
        // one place each is named, and this walks the menu comparing what the
        // label produced to what the callback delivered.
        let (t, rx) = tray(PHASE_IDLE);
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
    fn the_tray_icon_is_the_brand_mark_at_a_panel_sized_frame() {
        let (t, _) = tray(PHASE_RECORDING);
        let pixmap = t.icon_pixmap();
        // The whole baked set is offered, so the host can pick the frame that
        // matches the size it draws at instead of upscaling one.
        assert_eq!(
            pixmap.len(),
            crate::platform::linux_icon::SIZES.len(),
            "every baked frame is served"
        );
        // A panel asks for ~22px; the baked set's nearest frame is 24px, and
        // handing the host only the 256px mark would make it resample down
        // instead of getting the drawing made for its size.
        let icon = &pixmap[2];
        assert_eq!((icon.width, icon.height), (24, 24), "the 24px frame");
        assert_eq!(
            icon.data.len(),
            (24 * 24 * 4) as usize,
            "ARGB32, one word per pixel"
        );
        // The mark is drawn on a light card, so the tray frame must be mostly
        // opaque - a fully transparent buffer would pass the length check and
        // ship an invisible icon. The A,R,G,B layout these counts read is
        // pinned byte for byte by every_served_frame_matches_the_size_it_declares.
        let opaque = icon.data.chunks(4).filter(|px| px[0] > 40).count();
        assert!(
            opaque > 300,
            "the tray icon must be visible, got {opaque} opaque pixels"
        );
        // And it must be the mark, not the orb: the orb's palette is the
        // configured accent colour, while the brand mark is near-white.
        let pale = icon
            .data
            .chunks(4)
            .filter(|px| px[0] > 200 && px[1] > 200 && px[2] > 200)
            .count();
        assert!(
            pale > 100,
            "expected the pale brand card, got {pale} pale pixels"
        );
    }

    /// Every served frame is well-formed: its own declared size, a whole
    /// number of pixels, and every pixel in A,R,G,B order - a pixmap that put
    /// alpha last would render as noise in every host. Serving the set is what
    /// lets a HiDPI panel pick 48px instead of upscaling 24px.
    #[test]
    fn every_served_frame_matches_the_size_it_declares() {
        let (t, _) = tray(PHASE_RECORDING);
        let pixmap = t.icon_pixmap();
        let sizes: Vec<u32> = crate::platform::linux_icon::SIZES.to_vec();
        assert_eq!(pixmap.len(), sizes.len(), "one pixmap per baked frame");
        for (icon, expected) in pixmap.iter().zip(sizes) {
            assert_eq!(icon.width as u32, expected, "{expected}px frame");
            assert_eq!(icon.data.len(), (expected * expected * 4) as usize);
            let (_, _, rgba) =
                crate::platform::linux_icon::frame(expected).expect("the baked frame resolves");
            for (served, source) in icon.data.chunks(4).zip(rgba.chunks(4)) {
                assert_eq!(
                    served,
                    [source[3], source[0], source[1], source[2]].as_slice(),
                    "{expected}px pixel is not A,R,G,B"
                );
            }
            // The largest frame is the same card at 256px; it must be opaque
            // enough to see.
            if expected == 256 {
                let opaque = icon.data.chunks(4).filter(|px| px[0] > 40).count();
                assert!(
                    opaque > 40_000,
                    "the 256px mark must be dense, got {opaque}"
                );
            }
        }
    }

    /// The orb fallback is still exercised: a build with no baked brand asset
    /// must keep identifying itself rather than shipping an empty tray.
    #[test]
    fn the_orb_remains_the_fallback_icon() {
        let icon = &orb_pixmap(PHASE_RECORDING, DEFAULT_FRAME, (1.0, 0.5, 0.9), 0.75)[0];
        assert_eq!(icon.width as u32, ORB_WIDTH);
        assert_eq!(icon.data.len(), (ORB_WIDTH * ORB_HEIGHT * 4) as usize);
        let opaque = icon.data.chunks(4).filter(|px| px[0] > 40).count();
        assert!(
            opaque > 150,
            "the orb fallback must be visible, got {opaque}"
        );
    }

    #[test]
    fn the_tooltip_names_the_hotkey_and_says_when_there_is_none() {
        let (t, _) = tray(PHASE_IDLE);
        assert!(t.tool_tip().description.contains("Alt+Space"));

        let (tx, _rx) = std::sync::mpsc::channel();
        let no_key = MnvoiceTray::new(
            tx,
            Arc::new(|| PHASE_IDLE),
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
        let (t, _) = tray(PHASE_IDLE);
        assert_eq!(t.id(), "mnvoice");
        assert_eq!(t.title(), "mnvoice");
        assert!(matches!(t.status(), Status::Active));
        let (live, _) = tray(PHASE_RECORDING);
        assert_eq!(live.title(), "mnvoice - recording");
        assert_eq!(tray(PHASE_TRANSCRIBING).0.title(), "mnvoice - transcribing");
    }
}
