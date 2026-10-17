// mnvoice - push-to-talk dictation for Windows.
// Alt+Space starts recording, speech is streamed and typed directly into the focused window,
// auto-stops when silence is detected (or Esc stops).
// A compact translucent glass orb with flowing fluid inside indicates status at the screen bottom.

use crate::locks::lock_poisoned;
use crate::audio;
use crate::config;
use crate::orb;
use crate::platform;
use crate::platform::windows_impl::wide;
use crate::rest;
use crate::stream;
use crate::update;

use std::fs::OpenOptions;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId, CREATE_NO_WINDOW};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};
use std::os::windows::process::CommandExt;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_GUID, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_WORKER: u32 = WM_APP + 2;

const NO_SPEECH: &str = "No speech detected";

/// Resource id of the icon embedded by build.rs via assets/mnvoice.ico.
/// MAKEINTRESOURCEW(1) in the Windows API is `1 as *const u16`.
#[allow(clippy::manual_dangling_ptr)]
const IDI_APP: PCWSTR = PCWSTR(1 as *const u16);

/// Loads the embedded mnvoice icon at the default (large) size. Falls back to
/// the OS generic icon if the resource is somehow missing, so an icon is never
/// blank. Used for the window class; the tray uses `tray_icon`.
unsafe fn app_icon() -> HICON {
    if let Ok(mod_) = GetModuleHandleW(None) {
        if let Ok(h) = LoadIconW(HINSTANCE(mod_.0), IDI_APP) {
            if !h.is_invalid() {
                return h;
            }
        }
    }
    unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default()
}

/// The embedded icon at the size the taskbar tray actually draws into, named
/// for its one caller because that caller is the whole reason it exists.
///
/// The request is SM_CXSMICON rather than the default icon metric, and what
/// that changes is the SIZE chosen, not the artwork. Measured in this process:
/// `LoadIconW(IDI_APP)` resolves to SM_CXICON (32 px here) while the tray slot
/// is SM_CXSMICON (16 px at 100% scaling, 19 px at 125%, 24 px at 150%), so
/// the tray has been down-sampling a 32 px frame into a 16 px slot, and at
/// higher scaling a 32 px frame into a 19 or 24 px slot. This exe carries a
/// responsive .ico - mark-only at 16-48 px, the full brand card at 64-256 px
/// (see AGENTS.md, "App and tray icon") - and at 100% the tray was already
/// showing a mark; what was unreachable were the frames rasterised FOR the
/// tray's own size. `LoadIconW` cannot express a size at all, which is why
/// this is `LoadImageW`. Pinning the two metrics apart is what the test for
/// this call asserts.
///
/// `LR_SHARED` is required: without it every call leaks a fresh icon handle,
/// and the tray calls this on each modify.
///
/// The fallback keeps `app_icon`'s shape (a system icon is always loadable),
/// so an exe built without the resource, or a session reporting a zero metric,
/// still shows something.
unsafe fn tray_icon() -> HICON {
    let cx = GetSystemMetrics(SM_CXSMICON);
    let cy = GetSystemMetrics(SM_CYSMICON);
    // A metric of zero is not a size to request: LoadImageW would be asked for
    // a zero-pixel icon. Fall back to the default-size load in that case.
    if cx <= 0 || cy <= 0 {
        return app_icon();
    }
    if let Ok(mod_) = GetModuleHandleW(None) {
        if let Ok(h) = LoadImageW(HINSTANCE(mod_.0), IDI_APP, IMAGE_ICON, cx, cy, LR_SHARED) {
            let icon = HICON(h.0);
            if !icon.is_invalid() {
                return icon;
            }
        }
    }
    app_icon()
}

const HOTKEY_TOGGLE: i32 = 1;
const HOTKEY_ESC: i32 = 2;
const IDM_DICTATE: usize = 1;
const IDM_EXIT: usize = 2;
const IDM_STARTUP: usize = 3;
const IDM_RESTART: usize = 4;
const IDM_OPEN_CONFIG: usize = 5;
const IDM_OPEN_KEYWORDS: usize = 6;
const IDM_UPDATE: usize = 7;
const TIMER_ORB: usize = 101;

/// The tray icon's `uID`. With `TRAY_GUID` this is the icon's identity: every
/// `Shell_NotifyIconW` call has to name the same pair.
const TRAY_ICON_ID: u32 = 1;

const CLASS_NAME: PCWSTR = w!("mnvoiceTrayClass");
const WINDOW_NAME: PCWSTR = w!("mnvoice");
const MUTEX_NAME: PCWSTR = w!("mnvoice-single-instance");
const TRAY_GUID: GUID = GUID {
    data1: 0x8f31c2d4,
    data2: 0x5a6b,
    data3: 0x4e19,
    data4: [0xb7, 0x02, 0x6c, 0x41, 0x9e, 0xd3, 0x55, 0x1a],
};

#[derive(Clone, Copy, PartialEq)]
enum State {
    Idle,
    Recording,
    Transcribing,
}

/// Human-readable state for logs and notifications.
fn state_name(s: State) -> &'static str {
    match s {
        State::Idle => "idle",
        State::Recording => "recording",
        State::Transcribing => "transcribing",
    }
}

impl State {
    /// The wire form, shared with the updater's worker thread.
    ///
    /// One encode, one decode. The encode used to be spelled out at two call
    /// sites - the static's initializer and `set_state`'s store, both as a bare
    /// `as u8` - while `from_code` re-spelled the table as
    /// `1 => Recording, 2 => Transcribing, _ => Idle`. Nothing kept them in
    /// agreement but declaration order, so inserting a variant - or reordering
    /// them - silently remapped the shared atomic. Keeping both directions here
    /// and nowhere else is what makes the table checkable.
    const fn as_code(self) -> u8 {
        self as u8
    }

    fn from_code(code: u8) -> State {
        match code {
            1 => State::Recording,
            2 => State::Transcribing,
            _ => State::Idle,
        }
    }
}

/// The session state in one authoritative place, readable from any thread.
///
/// The updater's worker thread reads this after its download finishes to decide
/// whether the binary may be swapped, so it must never have to ask the UI
/// thread's window for the answer: that lookup handed out a raw pointer into
/// UI-thread-owned memory that another thread then read and aliased, which is
/// only correct by luck and disappears entirely once the window is gone.
static SESSION_STATE: AtomicU8 = AtomicU8::new(State::Idle.as_code());

/// Set at the very start of startup, before anything could consume the
/// swap-aside image the previous process left behind, and read once the tray
/// icon exists: a leftover `.old` at startup is exactly how a just-installed
/// update announces itself, and that announcement is the only "done" the
/// user ever sees, because the process that did the installing is the one
/// that exits.
static JUST_UPDATED: AtomicBool = AtomicBool::new(false);

/// Record a state change in both the UI's own copy and the shared atomic.
fn set_state(app: &mut App, state: State) {
    app.state = state;
    SESSION_STATE.store(state.as_code(), Ordering::SeqCst);
}

/// Take the UI back to idle at the end of a session: the tray state, the orb
/// timer, the cancel key, the orb window itself, and the tooltip.
///
/// Both session endings - the worker's outcome arriving on `WM_APP_WORKER` and
/// the user pressing the cancel key - used to spell these five calls out
/// separately, in different orders (`cancel` set the state before killing the
/// timer, the outcome handler after), which was correct only because nothing
/// here reads the timer. One owner fixes the order and makes the invariant
/// visible: the state is idle BEFORE the tip is pushed, because `push_tip`
/// renders the state.
///
/// Deliberately does not touch `cancelled`/`stop` or write a log line: those
/// are what distinguishes a cancel from a completed session, and folding them
/// in would make the shared part too big to reuse.
fn end_session(app: &mut App) {
    set_state(app, State::Idle);
    let _ = unsafe { KillTimer(app.hwnd, TIMER_ORB) };
    let _ = unsafe { UnregisterHotKey(app.hwnd, HOTKEY_ESC) };
    if let Some(orb) = &mut app.orb {
        orb.hide();
    }
    unsafe { push_tip(app.hwnd, app) };
}

struct App {
    hwnd: HWND,
    state: State,
    /// Shared, not owned: `toggle()` reads this on every push-to-talk, and
    /// an owned `Config` meant deep-cloning the api key, the base URL and the
    /// whole keywords list once per dictation. It is written once, in
    /// WM_CREATE, and only ever read afterwards.
    config: Option<Arc<config::Config>>,
    /// This session's flags. FRESH ARCS PER SESSION, never reset in place: a
    /// stale worker from a cancelled session holds its own clones, so its
    /// cancelled flag stays set forever (its streaming flush can never type
    /// into a newer session) and its outcome lands in an orphaned slot the UI
    /// handler skips by construction. Resetting shared flags instead let a
    /// cancel-then-quick-restart type the old session's leftover words into
    /// the new one and let the stale worker tear the new session's UI down.
    stop: Arc<AtomicBool>,
    /// Set by the cancel key. Once set, the streaming reader types nothing
    /// further and the final flush is skipped, so a cancel really discards.
    cancelled: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<(bool, String)>>>,
    /// Monotonically increasing session generation id. Passed via WPARAM in
    /// WM_APP_WORKER so messages from cancelled/superseded sessions are
    /// rejected by the window procedure before touching state.
    session_id: usize,
    /// The toggle hotkey's display spelling, computed once at startup (the
    /// "none" disabled spelling, the configured one, or the default when the
    /// config did not load). Every tip names this same value: deriving the
    /// spelling from `config` at the call sites made a config that failed to
    /// load fall back to the default text while the key actually registered
    /// (or failed to) was a different one.
    hotkey_str: String,
    /// Whether the toggle hotkey is available as configured: true when it
    /// registered, and true for HOTKEY=none (disabled on purpose is the
    /// configured state - nothing is missing). False means registration
    /// FAILED, and the tip must keep saying so: the first idle tip used to
    /// overwrite the startup UNAVAILABLE warning with a cheerful "F9 to
    /// dictate" for a key that does nothing.
    hotkey_ok: bool,
    /// Whether the cancel key for the current session could be claimed. The
    /// counterpart of `hotkey_ok` on the cancel side, and held as a field for
    /// the same reason: a one-off `set_tray_tip` from the failed registration
    /// was overwritten by the very next state tip, so the session went on
    /// advertising a cancel key that was never live. True when the key
    /// registered, and true when there is no cancel key to register
    /// (CANCEL_KEY=none is the configured state, not a failed one).
    cancel_key_ok: bool,
    orb: Option<orb::Orb>,
    audio_engine: audio::AudioEngine,
}

static LOG_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn log(msg: &str) {
    let _guard = lock_poisoned(&LOG_LOCK);
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("mnvoice.log"))
    {
        let _ = writeln!(f, "[{}] {msg}", update::now_secs());
    }
}

/// Terminate any other running mnvoice.exe instances so they release the global
/// hotkey before this instance tries to claim it. Uses the Windows taskkill utility
/// which is present on every supported Windows version.
fn kill_running_instances() {
    let exe = std::env::current_exe().ok();
    let name = exe
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("mnvoice.exe")
        .to_string();
    let self_pid = unsafe { GetCurrentProcessId() };
    let reported = std::process::Command::new("C:\\Windows\\System32\\taskkill.exe")
        .args(["/F", "/IM", &name, "/FI", &format!("PID ne {self_pid}")])
        .creation_flags(CREATE_NO_WINDOW.0)
        .status()
        .map(|s| s.code());
    // Report what the scheduler actually said. `let _ = ...status()` dropped
    // the ExitStatus, so this line was written whether taskkill ran at all, was
    // filtered to zero victims, or failed - a restart log claiming a teardown
    // that never happened is worse than no log, because the next thing on
    // screen is a hotkey-registration failure with no cause above it.
    log(&restart_log_line(&name, reported));
}

/// What the restart teardown writes to the log, given what taskkill reported:
/// `Ok(Some(0))` is a run that finished without error, `Ok(Some(n))` a run that
/// exited with code `n`, `Ok(None)` one that ended with no code at all, and
/// `Err` one that could not be spawned.
///
/// Pure on purpose: the failure arms cannot be reached through the real command
/// without a victim the caller cannot terminate (an elevated or protected
/// process), and producing one takes an interactive UAC consent that stalls an
/// automated run indefinitely. Classifying a value pins the arms in a unit test
/// with no process spawned.
///
/// The zero arm deliberately does not claim a termination. Measured on this
/// machine: with `/FI "PID ne <self>"`, a run that killed a real victim and a
/// run where the filter matched nothing both exit 0 - the second prints
/// "INFO: No tasks running with the specified criteria." and the first prints
/// one "SUCCESS:" line per kill. So the exit code cannot tell the two apart,
/// and counting the SUCCESS lines is not a fix either: those strings come from
/// Windows message resources and are localized, so a non-English taskkill would
/// read as zero kills. Naming the code, and what it does and does not prove, is
/// the most this call can honestly say. (128 is the only code with a single
/// meaning here: no such image.)
fn restart_log_line(name: &str, reported: std::io::Result<Option<i32>>) -> String {
    match reported {
        Ok(Some(0)) => format!(
            "restart: taskkill exit 0 for other {name} instances (killed one, or none matched)"
        ),
        Ok(code) => format!(
            "restart: taskkill reported {} for other {name} instances",
            code.unwrap_or(-1)
        ),
        Err(e) => format!("restart: could not run taskkill ({e})"),
    }
}

/// Where the process starts, one step per line: finish a pending update
/// install, heal a restart, claim the single-instance mutex, load config,
/// then build the window, tray and hotkey and run the message loop. Each
/// step is a named helper below.
pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    if finish_pending_install(&args) {
        return;
    }
    if args.iter().any(|a| a == update::RESTART_ARG) {
        // Graceful self-heal: terminate any running instance, wait for it to
        // release the global hotkey, then continue starting fresh.
        kill_running_instances();
        thread::sleep(std::time::Duration::from_millis(700));
    }

    // Single instance: a process that finds the name taken exits here, before it
    // registers a window class, starts a capture engine or adds a tray icon.
    // The handle is what keeps the name claimed, so it outlives main.
    let _mutex = match claim_single_instance() {
        InstanceClaim::AlreadyTaken => return,
        InstanceClaim::Acquired(handle) => handle,
    };
    let config = load_config_or_log();
    // Shared from here on: the tray window keeps it for the life of the app
    // and every session reads it, so it is wrapped once instead of being
    // deep-copied per dictation.
    let config = config.map(Arc::new);
    let (hk_mod, hk_vk, hk_str) = hotkey_of(&config);
    let cancel_str = config
        .as_ref()
        .map(|c| c.cancel_key_str.clone())
        .unwrap_or_else(|| config::DEFAULT_CANCEL_STR.to_string());

    log(&format!(
        "mnvoice v{} started (pid {}, protocol {:?}, model {}, hotkey: {}, cancel: {}, keywords: {})",
        platform::version(),
        unsafe { GetCurrentProcessId() },
        config.as_ref().map(|c| c.protocol),
        config.as_ref().map(|c| c.model.as_str()).unwrap_or("none"),
        hk_str,
        cancel_str,
        config.as_ref().map(|c| c.keywords.len()).unwrap_or(0)
    ));

    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        if !register_window_class(hinstance) {
            return;
        }
        let audio_engine = audio::AudioEngine::start();
        // Best-effort housekeeping: reap what an update that never swapped in
        // left behind (the staged download and helper copies - the swap-aside
        // .old belongs to the just-updated handshake consumed just above) and
        // arm the periodic background check when AUTO_UPDATE=1.
        // Deliberately not read off the Config: auto_update_enabled() is called
        // directly. It stays readable when load() fails - a broken API key
        // aborts the parse - and that is exactly when a user is most likely to
        // be stuck on an outdated build, so the updater must not depend on a
        // Config having parsed successfully.
        let auto_update = config::auto_update_enabled();
        // Consume the just-updated handshake before anything else could
        // observe or remove the swap-aside image (the predicate deletes it as
        // it reads it, so this is also what makes the announcement at-most-once).
        JUST_UPDATED.store(update::left_old_image_behind(), Ordering::SeqCst);
        update::startup_cleanup(auto_update);
        // One-time move off the Run key onto a logon task. On its own
        // thread: it only decides whether the NEXT logon starts the app,
        // so the window, the hotkey and the tray never wait on its spawns.
        thread::spawn(|| {
            migrate_autostart();
            // The menu's checkbox reads the cache rather than probing inside
            // the modal loop, so the first open has an answer ready.
            refresh_autostart_cache();
        });

        // A load() failure is exactly the state the idle tip has to report,
        // and it is the reason dictation cannot start at all.
        let Some(hwnd) =
            create_tray_window(hinstance, config, hk_str.clone(), audio_engine)
        else {
            return;
        };
        // HOTKEY=none disables the toggle registration entirely; the tray
        // menu's Dictate item is then the only start and stop control (vk == 0
        // is the disabled sentinel). A disabled control is not a failed one,
        // so the tip may still say where dictation moved.
        let hotkey_ok = if hk_vk == 0 {
            log("hotkey disabled (HOTKEY=none); use the tray menu to dictate");
            true
        } else {
            register_hotkey_with_retry(hwnd, hk_mod, hk_vk, &hk_str)
        };
        if let Some(app) = app_ref(hwnd) {
            app.hotkey_ok = hotkey_ok;
            let configured = app.config.is_some();
            add_tray(hwnd, &idle_tip(&hk_str, hotkey_ok, configured));
        }

        // The installing process exits at relaunch, so it cannot report its
        // own success - this process is the success. The swap-aside image it
        // left behind is the handshake: say what happened now that there is a
        // tray icon to say it from.
        if JUST_UPDATED.load(Ordering::SeqCst) {
            let version = platform::version();
            log(&format!("previous install finished, running v{version}"));
            balloon("mnvoice updated", &format!("now running v{version}"));
        }

        run_message_loop();
    }
}

/// A second, short-lived copy of this exe finishes an install the first one
/// could not. It waits for that process to be gone, and only then, and only
/// when the swap left the exe path empty, does it move the staged image in.
/// Started before the single-instance mutex is taken, which the app itself
/// is holding for as long as it is the one installing. It runs from a copy
/// of this exe under an image name of its own, so the install it has to
/// repair comes in after the flag on the command line. Returns whether this
/// process was that finisher.
fn finish_pending_install(args: &[String]) -> bool {
    if let Some(pos) = args.iter().position(|a| a == update::FINISH_UPDATE_ARG) {
        let install = args.get(pos + 1).map(std::path::Path::new);
        // The digest the installer verified, passed to the helper so the staged
        // file it may have to finish installing is re-checked against it. See
        // `update::finish_install`.
        let digest = args.get(pos + 2).map(String::as_str);
        update::finish_install(install, digest);
        return true;
    }
    false
}

/// What claiming the single-instance name came to.
///
/// `Acquired` carries the handle, which the caller must keep alive for the
/// process's whole life: dropping it closes the handle, the OS releases the
/// name, and the guard stops guarding. `AlreadyTaken` is the one outcome the
/// caller must not start on.
enum InstanceClaim {
    Acquired(Option<HANDLE>),
    AlreadyTaken,
}

/// Advertise single-instance the Win32 way: a named mutex held for the
/// process's whole life (the OS releases it at exit; there is no closer to
/// call). Another instance owning the name is reported as `AlreadyTaken`,
/// which is the signal for this process to exit before it starts anything.
fn claim_single_instance() -> InstanceClaim {
    let handle = unsafe { CreateMutexW(None, true, MUTEX_NAME) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        log("second instance blocked, exiting");
        return InstanceClaim::AlreadyTaken;
    }
    // A failed CreateMutexW leaves no guard at all: the process runs without
    // single-instance protection, so two instances can race for the global
    // hotkey and the user sees a later "HOTKEY ... UNAVAILABLE" with no cause
    // above it. Nothing here can conjure a guard, but the log line is what
    // makes that later failure diagnosable, and every other startup failure in
    // this file already reports itself.
    if let Err(e) = &handle {
        log(&format!(
            "single-instance mutex unavailable ({e}), running unprotected"
        ));
    }
    InstanceClaim::Acquired(handle.ok())
}

/// Config or a logged reason. A broken config still starts the app: without
/// it there would be no tray menu to fix the file from.
fn load_config_or_log() -> Option<config::Config> {
    match config::load() {
        Ok(c) => Some(c),
        Err(e) => {
            log(&format!("config error: {e}"));
            // Still worth knowing whether updates are armed, because that is
            // decided from AUTO_UPDATE alone and a broken API key does not
            // revoke it. Otherwise the app runs but never updates itself and
            // nothing above this line says which way it went.
            log(&format!(
                "auto-update is {}",
                if config::auto_update_enabled() {
                    "armed"
                } else {
                    "off"
                }
            ));
            None
        }
    }
}

/// The hotkey pair and its display spelling, or the defaults when the
/// config did not load.
fn hotkey_of(config: &Option<Arc<config::Config>>) -> (HOT_KEY_MODIFIERS, u32, String) {
    config
        .as_ref()
        .map(|c| {
            (
                HOT_KEY_MODIFIERS(c.hotkey.0),
                c.hotkey.1,
                c.hotkey_str.clone(),
            )
        })
        .unwrap_or((
            HOT_KEY_MODIFIERS(config::DEFAULT_HOTKEY_MOD),
            config::DEFAULT_HOTKEY_VK,
            config::DEFAULT_HOTKEY_STR.to_string(),
        ))
}

/// Registers the window class once per process. A second registration is
/// not an error (the class survives between instances on some Windows
/// builds), anything else is.
fn register_window_class(hinstance: HINSTANCE) -> bool {
    unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: CLASS_NAME,
            hIcon: app_icon(),
            ..Default::default()
        };
        // A duplicate registration returns ERROR_CLASS_ALREADY_EXISTS, not
        // ERROR_ALREADY_EXISTS: window classes live in the process atom table,
        // and "this name is taken" is the atom table's own answer. Checking the
        // wrong code here meant a second registration of the same name logged a
        // failure that was not one. A relaunch is a fresh process with its own
        // atom table, so in practice the class is new each time - the tolerance
        // is for a future in-process second window.
        let registered = RegisterClassW(&wc) != 0;
        let taken = !registered && GetLastError() == ERROR_CLASS_ALREADY_EXISTS;
        if !registered && !taken {
            log(&format!("RegisterClassW failed ({})", GetLastError().0));
            return false;
        }
    }
    true
}

/// The hidden message window the tray, the hotkey and the worker all hang
/// off. `None` means the app cannot start.
fn create_tray_window(
    hinstance: HINSTANCE,
    config: Option<Arc<config::Config>>,
    hotkey_str: String,
    audio_engine: audio::AudioEngine,
) -> Option<HWND> {
    unsafe {
        let init = Box::into_raw(Box::new(AppInit {
            config,
            hotkey_str,
            instance: hinstance,
            audio_engine,
        }));
        match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            WINDOW_NAME,
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            Some(init as *const std::ffi::c_void),
        ) {
            Ok(h) => Some(h),
            Err(e) => {
                // The window proc never took ownership, so the box handed to
                // CreateWindowExW is still ours. It owns an Arc<config::Config>
                // and an AudioEngine, so leaving it unreclaimed leaks the
                // standby engine's resources on this path.
                drop(Box::from_raw(init));
                log(&format!("CreateWindowExW failed: {e}"));
                None
            }
        }
    }
}

/// Register the global hotkey. If another process (or a stale registration
/// from a previously killed instance) still owns it, retry for a few seconds
/// before giving up, then let the caller surface a visible tray warning
/// instead of silently running with a dead hotkey.
fn register_hotkey_with_retry(
    hwnd: HWND,
    hk_mod: HOT_KEY_MODIFIERS,
    hk_vk: u32,
    hk_str: &str,
) -> bool {
    for attempt in 0..10 {
        match unsafe { RegisterHotKey(hwnd, HOTKEY_TOGGLE, hk_mod, hk_vk) } {
            Ok(()) => {
                if attempt > 0 {
                    log(&format!(
                        "RegisterHotKey({hk_str}) succeeded on attempt {}",
                        attempt + 1
                    ));
                }
                return true;
            }
            Err(e) => {
                if attempt == 9 {
                    log(&format!(
                        "RegisterHotKey({hk_str}) FAILED after retries: {e} - another app or a stale mnvoice instance is holding this hotkey"
                    ));
                } else {
                    thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    }
    false
}

/// Pump messages until quit. Everything user-visible routes through here.
fn run_message_loop() {
    unsafe {
        let mut msg = MSG::default();
        // `GetMessageW` returns TRUE for a message, FALSE for WM_QUIT, and -1
        // for an error. `BOOL::as_bool` is `self.0 != 0`, so a bare
        // `while ...as_bool()` treats the error case as "keep pumping" and
        // re-dispatches the previous MSG in a tight loop - the exact
        // anti-pattern MSDN names on the GetMessage page. Testing the raw
        // value keeps both real answers and ends the pump on the third.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

struct AppInit {
    config: Option<Arc<config::Config>>,
    hotkey_str: String,
    instance: HINSTANCE,
    audio_engine: audio::AudioEngine,
}

/// The three per-session arcs, all brand new.
///
/// Two places start a session: the `App` built at `WM_CREATE`, and the Idle arm
/// of `toggle`. Both must write the same shape, because the stale-worker
/// argument in the field docs (`stop`/`cancelled`/`outcome`) depends on a new
/// session getting fresh arcs rather than a reset of the old ones - a stale
/// worker then writes into storage nothing reads anymore. Spelled twice, the pair
/// could drift and the first session would silently run on whatever the other one
/// wrote.
fn fresh_session_arcs() -> (
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<Mutex<Option<(bool, String)>>>,
) {
    (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(None)),
    )
}

/// The live `App`, or nothing if the window has none.
///
/// Returns `Option` rather than a `&'static mut` into a null pointer. Window
/// classes live in the process atom table, so a class registered once is
/// shared by every window of that class in the process, and `WM_CREATE` on a
/// second one would overwrite the first window's `GWLP_USERDATA` - which a
/// blind dereference would read as an `App` that belongs to the other window.
/// `WM_DESTROY` is the one path that clears the slot, so the value is absent
/// exactly when the window is going away. Nothing today creates a second tray
/// window, but the unchecked cast is what would make that a silent aliasing bug
/// instead of a compile error at the call site.
///
/// `unsafe` because it hands out a `&'static mut` whose validity is exactly the
/// reasoning above and nothing the compiler can see. Every sibling Win32 helper
/// in this file already says so in its signature (`app_icon`, `add_tray`,
/// `set_tray_tip`, `push_tip`, `show_menu`); a safe fn vouching for a reference
/// built from a pointer cast was the one place a caller could take that on trust.
/// Every call site here already sits inside an `unsafe` block, so this asks
/// nothing new of them.
unsafe fn app_ref(hwnd: HWND) -> Option<&'static mut App> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return None;
    }
    Some(&mut *ptr)
}

unsafe fn add_tray(hwnd: HWND, tip: &str) {
    let mut nid = tray_nid(hwnd, NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID);
    nid.uCallbackMessage = WM_APP_TRAY;
    // The tray is a small-icon surface: this is where the mark-only frames
    // exist to be used (see `tray_icon`).
    nid.hIcon = unsafe { tray_icon() };
    fill_wide(&mut nid.szTip, tip);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

/// The idle tray tip. It names the hotkey actually configured, not a
/// hardcoded one: a user with KEYBIND=F9 must not be told to press Alt+Space.
/// A disabled hotkey (HOTKEY=none) says where the control moved, and an
/// unregistered one (held by another app) keeps saying so - the first idle
/// write used to erase the startup warning and advertise a dead key.
fn idle_tip(hotkey: &str, hotkey_ok: bool, has_config: bool) -> String {
    // Without a key nothing can be transcribed, so that outranks every other
    // idle message: telling the user which key to press, when pressing it does
    // nothing, is the exact dead end this replaces.
    if !has_config {
        return "mnvoice - NO API KEY. Set API_KEY in mnvoice.env (see log).".to_string();
    }
    // "Disabled" is decided in one place, `hotkey_is_disabled`, rather than
    // by an exact string match here. The parser normalises to lowercase "none"
    // and the window setup tests the `hk_vk == 0` sentinel - three spellings of
    // one rule, where this third one silently degraded to "mnvoice - idle. none
    // to dictate." if the parser's normalisation ever changed.
    if hotkey_is_disabled(hotkey) {
        return "mnvoice - idle. Hotkey disabled - use the tray menu.".to_string();
    }
    match (hotkey, hotkey_ok) {
        (hk, true) => format!("mnvoice - idle. {hk} to dictate."),
        (hk, false) => format!("mnvoice - idle. HOTKEY {hk} UNAVAILABLE (in use by another app)"),
    }
}

/// Whether `hotkey` is the disabled spelling.
///
/// One definition for a rule the parser, the window setup and the tray tip all
/// need. The parser normalises to lowercase `"none"` when it builds the pair,
/// so a case-insensitive compare here agrees with it by construction rather
/// than by the two of them happening to spell it the same way today.
fn hotkey_is_disabled(hotkey: &str) -> bool {
    hotkey.eq_ignore_ascii_case("none")
}

/// The tray tooltip for a given session state. Single-sourced so left-click
/// polling and state transitions never disagree on tooltip text.
///
/// `cancel_warning` is the sentence naming a cancel key that could not be
/// claimed, and it is carried in every state, the way an unregistered dictate
/// key is: the condition lasts until a later session claims the key, so a tip
/// written once at the failed registration would be overwritten by the very
/// next state change and the session would go on advertising a key that does
/// nothing.
fn state_tip(
    state: State,
    hotkey_str: &str,
    hotkey_ok: bool,
    has_config: bool,
    cancel_warning: Option<&str>,
) -> String {
    let tip = match state {
        State::Idle => idle_tip(hotkey_str, hotkey_ok, has_config),
        State::Recording => "mnvoice - listening (auto-stops on silence)".to_string(),
        State::Transcribing => "mnvoice - transcribing...".to_string(),
    };
    match cancel_warning {
        Some(warning) => format!("{tip} - {warning}"),
        None => tip,
    }
}

unsafe fn set_tray_tip(hwnd: HWND, tip: &str) {
    let mut nid = tray_nid(hwnd, NIF_TIP | NIF_GUID);
    fill_wide(&mut nid.szTip, tip);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// The warning naming a cancel key this app could not claim, or `None` when
/// there is nothing to warn about: either the key registered, or there is no
/// cancel key to claim. Owned here because this is the only place the sentence
/// is composed - `toggle` records the failure and every tip reads it back from
/// that record, so the warning cannot be written to the tip once and then be
/// overwritten by the next state change.
fn cancel_warning(app: &App) -> Option<String> {
    match (app.cancel_key_ok, app.config.as_ref()) {
        (false, Some(cfg)) => Some(format!(
            "{} could not be claimed, so there is no cancel key",
            cfg.cancel_key_str
        )),
        _ => None,
    }
}

/// Push the tray tip for the state `app` is in right now.
///
/// `state_tip`'s doc says the *text* is single-sourced "so left-click polling
/// and state transitions never disagree on tooltip text" - and it is. The
/// *arguments* were not: this exact expression was spelled out at
/// five call sites (the tray left-click, the worker teardown, both toggle arms
/// and cancel), with every input a field of the same `App`, so a field the tip
/// should reflect had to be threaded through five places. `hotkey_ok` already
/// was, which is the drift this prevents rather than merely tidies.
///
/// Reads `app.state` rather than taking a state argument, so the tip always
/// describes the state the app is in - at the four sites that call `set_state`
/// first, that is the state they were hardcoding.
unsafe fn push_tip(hwnd: HWND, app: &App) {
    // Read from the record `toggle` writes, so no call site has to thread the
    // cancel-key state through and no transition can erase the warning.
    let warning = cancel_warning(app);
    set_tray_tip(
        hwnd,
        &state_tip(
            app.state,
            &app.hotkey_str,
            app.hotkey_ok,
            app.config.is_some(),
            warning.as_deref(),
        ),
    );
}

/// The tray icon's identity, shared by every `Shell_NotifyIconW` call: the
/// window, the id and the GUID are what make four different calls address
/// the same icon.
fn tray_nid(hwnd: HWND, flags: NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        // Named like every other id this file hands out. `uID` is half of the
        // icon's identity alongside `TRAY_GUID`: every `Shell_NotifyIconW`
        // call has to name the same pair, and a bare literal is the one thing
        // in that struct with no name to change together.
        uID: TRAY_ICON_ID,
        uFlags: flags,
        guidItem: TRAY_GUID,
        ..Default::default()
    }
}

/// Copies `text` into one of the fixed-size wide buffers the shell API
/// expects, truncating at the buffer's edge - the shell reads it
/// NUL-terminated, so clamping is the contract, not an afterthought.
fn fill_wide(dst: &mut [u16], text: &str) {
    let src = wide(text);
    // Reserve the last slot for the terminator. Copying dst.len() elements when
    // the source is longer leaves the buffer full of data with no NUL anywhere
    // in it, and the shell reads these fields NUL-terminated - so a truncated
    // copy would run off the end of szTip looking for the end of the string.
    // Nothing fed to these fields approaches their size today (szTip is 128,
    // szInfo 256), so this is a cliff removed rather than a bug fixed, but the
    // contract the function claims is now upheld on the truncation path too.
    //
    // A zero-length buffer is the one shape with no last slot to reserve: the
    // terminator write below would index past its end, so it is skipped rather
    // than clamped. No caller passes one, and the copy is a no-op either way.
    if dst.is_empty() {
        return;
    }
    let cap = dst.len() - 1;
    let n = src.len().min(cap);
    dst[..n].copy_from_slice(&src[..n]);
    dst[n] = 0;
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lparam.0 as *const CREATESTRUCTW);
                let init = Box::from_raw(cs.lpCreateParams as *mut AppInit);
                let color = init
                    .config
                    .as_ref()
                    .map(|c| c.orb_color)
                    .unwrap_or(crate::config::DEFAULT_ORB_COLOR);
                let fluid = init
                    .config
                    .as_ref()
                    .map(|c| c.orb_fluid_level)
                    .unwrap_or(crate::config::DEFAULT_ORB_FLUID_LEVEL);
                let orb = orb::Orb::new(init.instance, color, fluid)
                    .map_err(|e| {
                        log(&format!("orb init: {e}"));
                        e
                    })
                    .ok();
                let (stop, cancelled, outcome) = fresh_session_arcs();
                let app = Box::into_raw(Box::new(App {
                    hwnd,
                    state: State::Idle,
                    config: init.config,
                    stop,
                    cancelled,
                    outcome,
                    session_id: 0,
                    hotkey_str: init.hotkey_str,
                    // The registration result arrives after WM_CREATE (the
                    // hwnd did not exist yet); main sets the real value.
                    hotkey_ok: true,
                    // No session has started, so nothing has failed to claim.
                    cancel_key_ok: true,
                    orb,
                    audio_engine: init.audio_engine,
                }));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_ORB {
                    if let Some(app) = app_ref(hwnd) {
                        if let Some(orb) = &mut app.orb {
                            orb.tick();
                        }
                    }
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                if let Some(app) = app_ref(hwnd) {
                    match wparam.0 as i32 {
                        HOTKEY_TOGGLE => toggle(app),
                        // Esc cancels and hides the orb at once. It must not
                        // "finish" the phrase, which used to leave the orb
                        // sitting on screen.
                        HOTKEY_ESC => cancel(app),
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            WM_APP_TRAY => {
                let event = lparam.0 as u32;
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    show_menu(hwnd);
                } else if event == WM_LBUTTONUP {
                    if let Some(app) = app_ref(hwnd) {
                        push_tip(hwnd, app);
                    }
                }
                LRESULT(0)
            }
            WM_APP_WORKER => {
                let Some(app) = app_ref(hwnd) else {
                    return LRESULT(0);
                };
                let msg_session = wparam.0;
                if msg_session != app.session_id {
                    // Stale outcome from an earlier session; ignore completely.
                    return LRESULT(0);
                }
                let cancelled = app.cancelled.load(Ordering::SeqCst);
                let outcome = lock_poisoned(&app.outcome).take();
                if let Some((ok, message)) = outcome {
                    end_session(app);
                    // A cancelled session was already closed by cancel(); whatever
                    // the worker scraped together afterwards is deliberately dropped
                    // and must not be reported as a transcription. On REST nothing
                    // was typed either; on streaming, words committed before the
                    // cancel are already in the document, so the line says what
                    // cancel actually guarantees: the rest is discarded.
                    if cancelled {
                        log("session cancelled, discarding the rest");
                    } else if ok {
                        // One preview definition, shared with the transport's
                        // error path and the REST refusal message, so the bound
                        // cannot drift between the places a user reads it.
                        log(&format!("transcribed: {}", crate::platform::http::preview(&message)));
                    } else {
                        log(&format!("error: {message}"));
                    }
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = wparam.0;
                match id {
                    IDM_EXIT => {
                        // NIF_GUID is part of the icon's identity, not a
                        // modifier on it: the other three calls set it, and
                        // with it clear the shell addresses (hwnd, uID) -
                        // which is not this icon, because uID is ignored once
                        // a GUID identifies it. MSDN's own troubleshooting note
                        // for guidItem is that NIF_GUID must be set in EVERY
                        // Shell_NotifyIcon call, so the previous
                        // `Default::default()` (flags = 0) made the one
                        // explicit delete a no-op and the icon lingered until
                        // the shell reclaimed it at process exit.
                        let _ = Shell_NotifyIconW(NIM_DELETE, &tray_nid(hwnd, NIF_GUID));
                        // Release the global hotkey so the next launch can claim it.
                        // Without this, a killed or exited instance can leave Windows
                        // still believing the hotkey is owned, breaking the next start.
                        let _ = UnregisterHotKey(hwnd, HOTKEY_TOGGLE);
                        let _ = UnregisterHotKey(hwnd, HOTKEY_ESC);
                        if let Some(app) = app_ref(hwnd) {
                            set_state(app, State::Idle);
                        }
                        PostQuitMessage(0);
                    }
                    IDM_DICTATE => {
                        if let Some(app) = app_ref(hwnd) {
                            toggle(app);
                        }
                    }
                    IDM_STARTUP => {
                        // Toggle the logon task. The Run key is cleared as a side
                        // effect, and the state is re-read on next open, so the
                        // checkbox can never drift out of sync with reality.
                        //
                        // Runs on a worker thread, like check_for_updates_async
                        // and migrate_autostart: this body spawns schtasks and,
                        // whenever no task is registered, up to five reg.exe
                        // calls, each an unbounded Command::status()/.output()
                        // wait - std has no timeout. Doing that on the UI thread
                        // meant doing it inside TrackPopupMenu's modal loop,
                        // where nothing else is serviced: the tray window showed
                        // Not Responding, the balloon stopped updating, and a
                        // queued WM_APP_WORKER teardown was delayed by however
                        // long the registry took. On an AV-scanned or
                        // domain-locked machine that is a visible freeze from a
                        // single menu click.
                        thread::spawn(|| {
                            if let Err(e) = toggle_autostart() {
                                log(&format!("autostart toggle failed: {e}"));
                            }
                            // The menu's checkbox reads the cache, so the
                            // mutation has to republish it or the next open
                            // would still show the state from before the
                            // click.
                            refresh_autostart_cache();
                        });
                    }
                    IDM_RESTART => relaunch_for_restart(),
                    IDM_UPDATE => check_for_updates_async(false),
                    IDM_OPEN_CONFIG => {
                        open_companion_file(
                            "mnvoice.env",
                            "# mnvoice - see mnvoice.env.example for every key\nPROTOCOL=streaming\nAPI_KEY=\n",
                        );
                    }
                    IDM_OPEN_KEYWORDS => {
                        open_companion_file(
                            "keywords.txt",
                            "# one word per line, or comma-separated\n",
                        );
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = UnregisterHotKey(hwnd, HOTKEY_TOGGLE);
                let _ = UnregisterHotKey(hwnd, HOTKEY_ESC);
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
                if !ptr.is_null() {
                    drop(Box::from_raw(ptr));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn toggle(app: &mut App) {
    match app.state {
        State::Idle => {
            let Some(cfg) = app.config.clone() else {
                log("API key not set in mnvoice.env");
                return;
            };

            // Claim the cancel key BEFORE anything else becomes visible.
            // The orb appearing used to leave a gap where Esc was dead, which
            // read as "Esc does not work right after the orb shows".
            if cfg.cancel_key.1 != 0 {
                match unsafe {
                    RegisterHotKey(
                        app.hwnd,
                        HOTKEY_ESC,
                        HOT_KEY_MODIFIERS(cfg.cancel_key.0),
                        cfg.cancel_key.1,
                    )
                } {
                    Ok(()) => app.cancel_key_ok = true,
                    Err(e) => {
                        // Another process already owns the combination, which
                        // is the exact condition `register_hotkey_with_retry`
                        // exists for on the dictate key. The orb still appears
                        // below, and without a tip it advertises a cancel key
                        // that was never live - so record it on the app, the
                        // same way the dictate key's failure is recorded, and
                        // let `state_tip` keep saying it for as long as it is
                        // true. Writing the tip here instead was overwritten
                        // by the state push a few lines below.
                        log(&format!("RegisterHotKey({}) failed: {e}", cfg.cancel_key_str));
                        app.cancel_key_ok = false;
                    }
                }
            }

            // Fresh flags and a fresh outcome slot per session, never a reset:
            // a stale worker from the cancelled session writes to arcs nothing
            // reads anymore (its cancelled flag stays true, so its late flush
            // cannot type, and its outcome lands in an orphaned slot the UI
            // handler skips) and can no longer tear the new session down.
            app.session_id = app.session_id.wrapping_add(1);
            let session_id = app.session_id;
            let (stop, cancelled, outcome) = fresh_session_arcs();
            app.stop = stop.clone();
            app.cancelled = cancelled.clone();
            app.outcome = outcome.clone();
            let hwnd_bits = app.hwnd.0 as usize;
            let audio_engine = app.audio_engine.clone();

            // Worker immediately captures audio via pre-initialized standby engine & connects WebSocket
            let worker_cfg = cfg.clone();
            thread::spawn(move || {
                worker(
                    session_id,
                    stop,
                    cancelled,
                    worker_cfg,
                    outcome,
                    hwnd_bits,
                    audio_engine,
                )
            });
            set_state(app, State::Recording);

            // Summon the orb last, once cancel is already live.
            if let Some(orb) = &mut app.orb {
                orb.show(orb::OrbState::Recording);
            }
            let _ = unsafe { SetTimer(app.hwnd, TIMER_ORB, 33, None) };
            unsafe { push_tip(app.hwnd, app) };
            log("recording started");
        }
        State::Recording => {
            app.stop.store(true, Ordering::SeqCst);
            set_state(app, State::Transcribing);
            let _ = unsafe { UnregisterHotKey(app.hwnd, HOTKEY_ESC) };
            if let Some(orb) = &mut app.orb {
                orb.set_state(orb::OrbState::Transcribing);
            }
            unsafe { push_tip(app.hwnd, app) };
            log("recording stopped, transcribing");
        }
        // Pressing the dictate key again mid-transcription means "stop".
        // This arm used to do nothing at all, and because the tray item is greyed
        // rather than removed and the hotkey is registered regardless, the press
        // was swallowed with no log line, no tip and no state change - the orb
        // sat there transcribing and the user could not tell whether the press
        // had registered. The press cancels for both protocols: a Streaming
        // cancel stops the queued tail from being shipped, while a REST cancel
        // discards the whole captured dictation - `dictate_rest`'s entry guard
        // and its post-round-trip check drop it.
        State::Transcribing => cancel(app),
    }
}

/// Cancel the current session outright: stop capture, discard anything already
/// transcribed, and hide the orb in the same instant. This is what the cancel key
/// (Esc by default) must do.
fn cancel(app: &mut App) {
    if app.state == State::Idle {
        return;
    }
    // Tell the worker and the streaming reader to stop typing further words.
    app.cancelled.store(true, Ordering::SeqCst);
    app.stop.store(true, Ordering::SeqCst);
    end_session(app);
    log("recording cancelled");
}

/// The capture report classified in every shape it can arrive in.
///
/// A clean `Ok(())` with a short buffer is silence. `None` and `Err(RecvError)`
/// both mean the audio worker is gone: `None` is the request channel refusing
/// the hand-off (the only `Err` `capture_to_channel` can return), and
/// `Err(RecvError)` is the worker dropping its sender without ever reporting.
/// Both are a broken microphone, and reporting either as `NO_SPEECH` tells a
/// user whose Windows Audio service just stopped that they said nothing. The
/// REST and streaming arms share this classifier so the two cannot drift apart
/// again.
///
/// `None` is deliberately named as the worker rather than the audio engine: an
/// engine that fails to initialise still reports through the channel, as
/// `Ok(Err(why))`, so it arrives as `Some` here with its own message. Naming
/// the engine pointed a reader at the wrong subsystem for the one failure the
/// `None` arm actually reports.
fn capture_fault(done: Option<&std::sync::mpsc::Receiver<Result<(), String>>>) -> Option<String> {
    match done {
        None => Some("capture failed: the audio worker is not running".to_string()),
        Some(done) => match done.recv() {
            Ok(Err(e)) => Some(e),
            // The worker dropped its sender without sending, which for this
            // channel means it did not reach its own report. Unreachable in a
            // release build - `panic = "abort"` means a panicking worker takes
            // the process down rather than unwinding out of it - so this is
            // the defensive shape, not a case anyone has observed. Kept
            // because a debug build can panic and the arm costs nothing.
            Err(_) => Some("capture failed: the audio worker stopped".to_string()),
            Ok(Ok(())) => None,
        },
    }
}

fn worker(
    session_id: usize,
    stop: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    cfg: Arc<config::Config>,
    outcome: Arc<Mutex<Option<(bool, String)>>>,
    hwnd_bits: usize,
    audio_engine: audio::AudioEngine,
) {
    let hwnd = HWND(hwnd_bits as *mut std::ffi::c_void);
    let (tx, rx) = std::sync::mpsc::channel();
    let stop_audio = stop.clone();
    let max_seconds = cfg.max_seconds;

    // 1. Immediately activate capture via pre-initialized standby WASAPI engine (latency ~4ms!)
    // The outer Err means the request could not even be queued, i.e. the audio
    // worker thread is gone. Its message is dropped here, but its shape is not:
    // `capture_fault` classifies `None` as that failure. It is not the engine -
    // an engine that cannot initialise reports through the done channel and so
    // arrives as `Some`.
    let capture_done = audio_engine
        .capture_to_channel(
            stop_audio,
            max_seconds,
            cfg.vad_silence_ms,
            cfg.vad_rms_threshold,
            tx,
        )
        .ok();

    // 2. Concurrently run transcription (streaming WebSocket or REST fallback)
    let result = match cfg.protocol {
        config::Protocol::Streaming => {
            let mut result = match stream::run_stream(&cfg, &stop, &cancelled, rx) {
                Ok(text) => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        (false, NO_SPEECH.into())
                    } else {
                        (true, text)
                    }
                }
                Err(e) => {
                    log(&format!("streaming error: {e}"));
                    (false, e)
                }
            };
            // This is the arm that leaves the capture report unread, so it is
            // also the arm that classifies it; the REST arm consumes its own
            // inside the match above. Every shape other than a clean `Ok(())`
            // is a capture failure, and reporting it as NO_SPEECH would tell a
            // user whose audio service stopped that they said nothing.
            //
            // Stop capture first. `run_stream` returning means the session is
            // over, but capture does not know that: on the commonest failure
            // (no network, or a bad API key, so the socket never opens) it is
            // still running, and this `recv` waits for it. Dropping the audio
            // receiver inside `run_stream` usually ends it within a chunk, but
            // a device that is delivering nothing has no send to fail, so
            // without this the report - and the error it carries - would wait
            // for the device or for max_seconds. Setting it is free: the
            // session is finished either way, and a worker already stopping
            // sees no change.
            stop.store(true, Ordering::SeqCst);
            if let Some(e) = capture_fault(capture_done.as_ref()) {
                result = (false, e);
            }
            result
        }
        config::Protocol::Rest => {
            let mut samples = Vec::new();
            while let Ok(chunk) = rx.recv() {
                samples.extend_from_slice(&chunk);
            }
            // A capture failure is reported BEFORE the buffer is uploaded or
            // typed: the CLI returns on the capture error first, and typing a
            // transcript the caller is about to disown is the inconsistency
            // this arm used to have with it. A cancelled session is the same
            // case - dictate_rest refuses to type it, and there is no reason
            // to upload it either.
            //
            // Every shape of the report is classified by `capture_fault`, so a
            // broken microphone is never reported as NO_SPEECH.
            let capture_err = capture_fault(capture_done.as_ref());
            match capture_err {
                Some(e) => (false, e),
                None => match rest::dictate_rest(&cfg, &samples, &cancelled) {
                    Ok(text) if text.is_empty() => (false, NO_SPEECH.into()),
                    Ok(text) => (true, text),
                    Err(e) => (false, e),
                },
            }
        }
    };

    *lock_poisoned(&outcome) = Some(result);
    let _ = unsafe { PostMessageW(hwnd, WM_APP_WORKER, WPARAM(session_id), LPARAM(0)) };
}

/// The tray menu's single dictate/stop item for a given session state.
///
/// One item carries every direction: with HOTKEY=none this is the only way to
/// start a dictation, and it must never be a grayed-out stop item while idle.
/// The label and enablement follow the state so the single action stays
/// honest. The Transcribing arm of `toggle` cancels the session for both
/// protocols: a Streaming cancel stops the queued tail from being shipped, and
/// a REST cancel discards the whole captured dictation, which `dictate_rest`'s
/// entry guard and post-round-trip check drop. Naming that cancel in this item
/// is a deferred product decision, so for now the item shows a disabled status
/// label rather than an enabled "Dictate" that would silently do nothing - and
/// a REST press would discard work the user cannot get back. Pure so the
/// mapping is testable without a live window.
fn dictate_item(state: State) -> (&'static str, bool) {
    match state {
        State::Idle => ("Dictate", true),
        State::Recording => ("Stop && transcribe", true),
        State::Transcribing => ("Transcribing...", false),
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Some(app) = app_ref(hwnd) else {
        // The window is being torn down; nothing left to show a menu for.
        return;
    };
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    // Checkbox reflects live system state. Read from a cache rather than
    // probing here: `autostart_enabled` spawns schtasks and up to two reg.exe
    // processes, each an unbounded wait, and this runs inside
    // TrackPopupMenu's modal loop where nothing else is serviced - the same
    // freeze the IDM_STARTUP handler was moved off the UI thread to avoid. The
    // cache is filled at startup and refreshed after any mutation, so the
    // checkmark still cannot drift from reality in the steady state.
    let startup_flags = if autostart_cached() {
        MF_CHECKED
    } else {
        MENU_ITEM_FLAGS(0)
    };
    let _ = AppendMenuW(
        menu,
        MF_STRING | startup_flags,
        IDM_STARTUP,
        w!("Start with Windows"),
    );

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_UPDATE, w!("Check for updates"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN_CONFIG, w!("Open config"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN_KEYWORDS, w!("Open keywords"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_RESTART, w!("Restart"));

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let (dictate_label, dictate_clickable) = dictate_item(app.state);
    let dictate_label_wide = wide(dictate_label);
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        IDM_DICTATE,
        PCWSTR(dictate_label_wide.as_ptr()),
    );
    if !dictate_clickable {
        let _ = EnableMenuItem(menu, IDM_DICTATE as u32, MF_GRAYED);
    }
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, w!("Exit"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    // The popup is gone by the time TrackPopupMenu returns, so the menu it
    // owns is freed here rather than at process exit. Every other resource in
    // this crate that the shell hands out is released on the same path that
    // used it - Orb::drop destroys its DC, bitmap and window - and a USER32
    // menu is no different. One leaked handle per right-click, for the life of
    // the process, was the only leak of its kind here.
    let _ = DestroyMenu(menu);
}

const AUTOSTART_TASK: &str = r"\mnvoice";
const AUTOSTART_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const AUTOSTART_VALUE: &str = "mnvoice";
/// Where Task Manager records that the user turned a startup entry off, so a
/// Run value that Disable left behind is one Windows will not start.
const AUTOSTART_APPROVED_KEY: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

fn reg_cmd() -> std::process::Command {
    let mut c = std::process::Command::new("C:\\Windows\\System32\\reg.exe");
    c.creation_flags(CREATE_NO_WINDOW.0);
    c
}

fn schtasks_cmd() -> std::process::Command {
    let mut c = std::process::Command::new("C:\\Windows\\System32\\schtasks.exe");
    c.creation_flags(CREATE_NO_WINDOW.0);
    c
}

/// What makes mnvoice start at logon, as far as the machine says.
///
/// Present and absent are kept apart on purpose: a Run value that is merely
/// there is one Windows ignores once Task Manager has disabled it, so the two
/// say different things about what the checkmark may claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutostartState {
    /// The logon task is registered: the steady state after the migration,
    /// where the task's own answer settles everything.
    TaskPresent,
    /// Neither mechanism is known to start mnvoice at logon.
    ///
    /// Two ways in: the Run key is confirmed absent, and the query that would
    /// have answered could not be made (see `autostart_state`'s `None` arm).
    /// Both draw the same unchecked box, so the menu needs no third answer;
    /// what they must not share is a claim of knowledge, which is why this says
    /// "not known to" rather than "is not". What a click then does is NOT
    /// decided from this state - `toggle_autostart` re-reads live - so an
    /// unchecked box over an autostart that is really on deletes it rather than
    /// re-enabling it (see `AUTOSTART_CACHED` for why that window is small).
    NoRunValue,
    /// No logon task, and a Run value Windows still starts.
    RunValueEnabled,
    /// No logon task, and a Run value the user turned off in Task Manager.
    RunValueDisabled,
}

/// Whether the tray checkmark may claim autostart, spelled from the state
/// alone rather than from any one read.
///
/// A logon task speaks for itself. A Run value does not: Task Manager's
/// Disable leaves the value in place and starts nothing, so only a value the
/// user has not turned off counts.
fn autostart_starting(state: AutostartState) -> bool {
    matches!(
        state,
        AutostartState::TaskPresent | AutostartState::RunValueEnabled
    )
}

/// True when something starts mnvoice at logon, read one answer at a time.
///
/// The Run key is still consulted, so an install that has not been relaunched
/// since the migration still reports an accurate tray checkmark instead of
/// offering to enable something that is already on. Every read is a process
/// spawn, so callers that run on the UI thread must use `autostart_cached`
/// instead of this: the logon task settles the question on its own in the
/// steady state, and the Run value's Task Manager override is only read while
/// an old Run entry may still be there.
fn autostart_enabled() -> bool {
    let state = match task_state() {
        TaskState::Present => AutostartState::TaskPresent,
        // A disabled row starts nothing on its own, so it settles nothing: the
        // Run key is still the other mechanism that could be starting mnvoice
        // (an install that migrated and then had its task turned off), and the
        // answer has to come from there. This is the same enabled/disabled
        // distinction the Run-key path already models as
        // `RunValueEnabled`/`RunValueDisabled`; before it, a task the user
        // disabled in Task Scheduler drew the box checked, and the only click
        // a checked box offers DELETES the task - the inverse of "turn it on",
        // recoverable only by clicking twice.
        // `Unknown` keeps its pre-existing route through the Run key: an
        // unanswerable task question has always been answered by the other
        // mechanism rather than by a guess, and this change is about the
        // disabled row, not about re-deciding that.
        TaskState::Disabled | TaskState::Absent | TaskState::Unknown => {
            autostart_state(run_key_set(), run_key_disabled)
        }
    };
    autostart_starting(state)
}

/// What `autostart_enabled` last reported, for the menu's checkbox.
///
/// The menu is built inside `TrackPopupMenu`'s modal loop, where a process
/// spawn freezes everything: `autostart_enabled` runs schtasks and up to two
/// reg.exe commands with no timeout, which is the same unbounded wait the
/// IDM_STARTUP handler was moved onto a worker thread to escape. So the probe
/// runs off the UI thread (at startup and after each mutation) and the menu
/// reads this. `-1` means "not probed yet", and the checkbox is then drawn
/// unchecked.
///
/// Unchecked is the direction that cannot promise a removal of something that
/// is not there, but it is NOT harmless in the other way, and the earlier note
/// here got that wrong: `toggle_autostart` ignores what the menu showed and
/// recomputes `enable` from a live `autostart_enabled()`. So a stale unchecked
/// box over an autostart that really is on does not "enable it again
/// idempotently" - the click computes `enable = false` and deletes the task,
/// the inverse of what the user just clicked for. The mitigation is that the
/// cache is filled at startup and refreshed after every mutation, so the window
/// where the box can disagree with reality is the initial probe, not ordinary
/// use; it is not zero, and saying "idempotent" hid that.
static AUTOSTART_CACHED: AtomicI8 = AtomicI8::new(-1);

fn autostart_cached() -> bool {
    AUTOSTART_CACHED.load(Ordering::SeqCst) > 0
}

/// Re-probe the autostart state and publish it for the menu.
///
/// Called on a worker thread at startup and after every mutation, so the
/// checkbox follows reality without the menu ever paying for the probe.
fn refresh_autostart_cache() {
    let enabled = autostart_enabled();
    AUTOSTART_CACHED.store(i8::from(enabled), Ordering::SeqCst);
}

/// Classify what the Run value alone says, given a query that may not have been
/// answerable.
///
/// All three answers, not two. `None` is *unanswerable*, and letting it fall
/// through to "enabled" both draws a checkmark nothing verified and makes the
/// toggle compute `enable = false` - so a click would run `schtasks /delete` on
/// a task the user asked to create, the inverse of the request. The
/// `Option<bool>` was introduced for exactly this distinction, so the consumer
/// has to be as total as the producer.
///
/// Kept apart from `autostart_enabled` so all four cases can be asserted
/// without spawning `reg.exe`. The override query arrives as a closure and is
/// read only inside the `Some(true)` arm, because that is the only arm it
/// changes - so the spawn behind it is paid only while a Run value is actually
/// there.
fn autostart_state(present: Option<bool>, disabled: impl FnOnce() -> bool) -> AutostartState {
    match present {
        Some(false) => AutostartState::NoRunValue,
        Some(true) if disabled() => AutostartState::RunValueDisabled,
        Some(true) => AutostartState::RunValueEnabled,
        // Nothing could be read, so nothing is claimed. Folding that into
        // `NoRunValue` is a deliberate collapse, not an assertion that the key
        // is absent: the variant means "not known to start at logon", which is
        // true of both cases. It bounds what the CHECKMARK may claim, not what
        // the click does: `toggle_autostart` ignores this state and re-reads
        // live, so a fresh answer of "on" makes the click a delete (see
        // `AUTOSTART_CACHED` for the one window where box and reality differ).
        None => AutostartState::NoRunValue,
    }
}

/// Whether the Run value is present.
///
/// Three states, not two: `reg.exe` that cannot be spawned at all is
/// *unanswerable*, and collapsing that into "absent" is what let
/// `clear_run_key` report success for a delete that never ran. `task_state`
/// spells the same distinction out with an explicit `Unknown` variant; this
/// returns the same shape without needing an enum, because only the caller
/// below has to care.
fn run_key_set() -> Option<bool> {
    match reg_cmd()
        .args(["query", AUTOSTART_RUN_KEY, "/v", AUTOSTART_VALUE])
        .output()
    {
        Ok(out) => Some(out.status.success()),
        Err(_) => None,
    }
}

/// Remove the pre-migration Run key entry, reporting whether it is gone.
///
/// True only when a query that could be answered confirms the value is no
/// longer there: a delete that did not take against a policy or a security
/// product leaves it in place, and a query that cannot be run at all confirms
/// nothing. Neither may read as success.
fn clear_run_key() -> Option<bool> {
    let _ = reg_cmd()
        .args(["delete", AUTOSTART_RUN_KEY, "/v", AUTOSTART_VALUE, "/f"])
        .output();
    run_key_set().map(|present| !present)
}

/// Whether the user has turned the Run-key autostart off in Task Manager.
///
/// Disable does not delete the Run value, it only writes a blob under
/// StartupApproved\Run, so a value that is merely present says nothing about
/// whether Windows starts it. A value that is absent, or a query that could
/// not be answered, is read as no override, which is what Windows does too.
fn run_key_disabled() -> bool {
    let Ok(out) = reg_cmd()
        .args(["query", AUTOSTART_APPROVED_KEY, "/v", AUTOSTART_VALUE])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    // reg.exe prints the value, its type and then its bytes on one line, so
    // the tail after the type is the blob.
    let text = String::from_utf8_lossy(&out.stdout);
    let tail = text
        .lines()
        .find_map(|line| line.split_once("REG_BINARY"))
        .map_or("", |(_, tail)| tail);
    startup_approved_disabled(&parse_reg_binary_hex(tail))
}

/// Turn a `reg query` binary value's hex tail into the bytes it encodes.
///
/// reg.exe prints the bytes as hex, and keeping only the hex digits means the
/// grouping in that output does not have to be guessed at.
fn parse_reg_binary_hex(hex: &str) -> Vec<u8> {
    let digits: String = hex.chars().filter(char::is_ascii_hexdigit).collect();
    digits
        .as_bytes()
        .chunks(2)
        .filter_map(|pair| std::str::from_utf8(pair).ok())
        .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
        .collect()
}

/// True when a StartupApproved blob records that the user turned the entry off.
///
/// The flag is the first byte and the rest is the timestamp: 02 is an entry
/// Windows still starts, 03 and 06 are the flags Task Manager writes.
fn startup_approved_disabled(blob: &[u8]) -> bool {
    matches!(blob.first(), Some(0x03) | Some(0x06))
}

/// The schtasks arguments that ask whether the autostart task exists AND
/// whether it is enabled.
///
/// `/xml` rather than a table: the exit status alone answers only "is there a
/// task row", and a disabled row is not something Windows starts at logon, so
/// the exit code was being read as "starts at logon" for a task the user had
/// turned off in Task Scheduler. The XML is the locale-proof way to get the
/// second fact - element names (`<Enabled>`) are English on every Windows,
/// unlike a LIST/TABLE status column, whose text and field separator both
/// follow the machine (a semicolon separator on a default German system).
/// Measured here: an enabled task's XML omits `<Enabled>` entirely, and
/// `schtasks /change /tn <task> /disable` makes the same query emit
/// `<Enabled>false</Enabled>` while its exit code stays 0.
///
/// Deliberately no `/fo`: it has no meaning for `/xml`, and the rule that
/// kept it out (never parse locale-dependent text) is the reason `/xml` is
/// safe to parse at all.
fn schtasks_query_args() -> Vec<String> {
    ["query", "/tn", AUTOSTART_TASK, "/xml"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Decode the scheduler's task document into text.
///
/// The document declares `encoding="UTF-16"` and, measured on this machine,
/// arrives as plain ASCII bytes with no byte-order mark - the declaration
/// describes the Task Scheduler's internal XML encoding, not the bytes
/// `schtasks` writes to stdout. That gap is why this decodes rather than
/// assuming: on a build that honoured its own declaration, a lossy UTF-8 read
/// would interleave every character with a replacement mark, the `<Task`
/// guard would fail, and the enabled/disabled answer would be lost SILENTLY
/// (the checkmark would fall back to the Run key, which is the pre-change
/// behaviour, so nothing would look wrong). Both shapes are accepted, and an
/// absent mark means the ASCII path.
fn task_xml_text(bytes: &[u8]) -> String {
    const UTF16LE: &[u8] = &[0xFF, 0xFE];
    const UTF16BE: &[u8] = &[0xFE, 0xFF];
    let (units, swap) = if bytes.starts_with(UTF16LE) {
        (&bytes[2..], false)
    } else if bytes.starts_with(UTF16BE) {
        (&bytes[2..], true)
    } else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    let mut out = String::with_capacity(units.len() / 2);
    for pair in units.chunks(2) {
        let lo = pair[0];
        let hi = pair.get(1).copied().unwrap_or(0);
        let code = if swap {
            u16::from_be_bytes([lo, hi])
        } else {
            u16::from_le_bytes([lo, hi])
        };
        out.push(char::from_u32(code as u32).unwrap_or('\u{fffd}'));
    }
    out
}

/// Whether a task's XML document records it as switched off.
///
/// Absent means enabled: measured, an enabled task's document carries no
/// `<Enabled>` element at ANY level (the trigger's is omitted too when true),
/// and a disabled one carries exactly `<Enabled>false</Enabled>`. So the
/// default is true, and a document that says nothing is a task Windows will
/// start.
///
/// The scan is over the element's own text rather than whole lines, because
/// the scheduler pretty-prints today and no part of the format promises it
/// will: a line-based match would read a single-line document as enabled,
/// which is the silent direction this must not fail in. Matching any
/// `<Enabled>` that says false also treats a disabled logon TRIGGER as not
/// starting, which is the right answer to the question being asked - a task
/// whose trigger is off does not run at logon either.
///
/// Split out so the classification is testable without spawning the
/// scheduler, which is how the rest of the autostart rules are pinned.
fn task_xml_disabled(xml: &str) -> bool {
    const OPEN: &str = "<Enabled>";
    const CLOSE: &str = "</Enabled>";
    let mut rest = xml;
    while let Some(open) = rest.find(OPEN) {
        let after = &rest[open + OPEN.len()..];
        let Some(close) = after.find(CLOSE) else {
            return false;
        };
        if after[..close].trim().eq_ignore_ascii_case("false") {
            return true;
        }
        rest = &after[close + CLOSE.len()..];
    }
    false
}

/// What the scheduler says about the at-logon autostart task.
///
/// Absent and Unknown must never be merged: a task that is not there is the
/// state a disable asks for, while a query that could not be answered says
/// nothing at all and has to fail the toggle rather than look like success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskState {
    /// Registered and enabled: Windows starts it at logon.
    Present,
    /// Registered but switched off (Task Scheduler > Disable, or
    /// `schtasks /change /disable`). The row exists, so a query exits 0, but
    /// nothing runs at logon.
    Disabled,
    Absent,
    Unknown,
}

/// Ask the scheduler about the task.
///
/// The exit code answers existence and the document answers enabled-ness, and
/// both are needed: a disabled task exits 0 like an enabled one, so reading
/// the code alone reported "starts at logon" for a task the user had turned
/// off. Only a process that could not be spawned at all is unanswerable.
///
/// A successful query whose XML cannot be read at all (empty, or not a
/// document) is `Unknown` rather than `Present`: claiming a start the machine
/// did not confirm is the failure mode the `Unknown` variant exists to
/// prevent, and it is what makes the toggle refuse rather than guess.
fn task_state() -> TaskState {
    match schtasks_cmd().args(schtasks_query_args()).output() {
        Ok(out) if out.status.success() => {
            let xml = task_xml_text(&out.stdout);
            if xml.contains("<Task") {
                if task_xml_disabled(&xml) {
                    TaskState::Disabled
                } else {
                    TaskState::Present
                }
            } else {
                TaskState::Unknown
            }
        }
        Ok(_) => TaskState::Absent,
        Err(_) => TaskState::Unknown,
    }
}

/// True when a task row exists, whichever way it is switched.
///
/// The migration asks this question (a row of any kind means the task has
/// already been created, and re-creating it is not its job), while the
/// checkmark asks the enabled-ness question below. Keeping the two apart is
/// what stops a disabled task from being read as "starts at logon".
fn task_exists() -> bool {
    matches!(task_state(), TaskState::Present | TaskState::Disabled)
}

/// The schtasks arguments that create the autostart task.
///
/// ONLOGON is what makes this worth doing: the shell starts Run-key apps one
/// at a time and spread over minutes on a busy boot, while the Task Scheduler
/// runs logon-triggered tasks at logon itself. LIMITED keeps the task running
/// as the current user with no elevation prompt, which is also what keeps the
/// tray icon in the user's own session.
fn schtasks_create_args(exe: &str) -> Vec<String> {
    let quoted = quoted_exe(exe);
    [
        "/create",
        "/tn",
        AUTOSTART_TASK,
        "/tr",
        quoted.as_str(),
        "/sc",
        "onlogon",
        "/rl",
        "limited",
        "/f",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The exe as the task action's command line.
///
/// The scheduler stores the /tr string as the action's command line without
/// re-quoting it, and an unquoted path that contains a space does not launch:
/// it is split at the space and resolves to the wrong program. A path that
/// already carries its quotes is passed through, because wrapping it twice
/// would break it the same way.
fn quoted_exe(exe: &str) -> String {
    if exe.len() >= 2 && exe.starts_with('"') && exe.ends_with('"') {
        exe.to_string()
    } else {
        format!("\"{exe}\"")
    }
}

/// Serializes every autostart mutation in the process.
///
/// The one-time migration runs on its own thread at startup while the tray
/// can be toggling the same task and the same Run key at the same time, so a
/// mutation holds this across its reads, its command and its confirming
/// queries: each mutation then sees the state the previous one left. A
/// poisoned lock is taken anyway, because a panicked thread must not wedge
/// the tray forever.
static AUTOSTART_LOCK: Mutex<()> = Mutex::new(());

fn autostart_lock() -> MutexGuard<'static, ()> {
    lock_poisoned(&AUTOSTART_LOCK)
}

/// Move an older Run-key install onto the Task Scheduler, once, silently.
///
/// Called at startup so the fix happens by relaunching rather than by asking
/// the user to do anything. Both mechanisms firing for one boot is harmless -
/// the single-instance mutex makes the loser exit - and the task is left in
/// place, so the swap only ever runs in this direction.
fn migrate_autostart() {
    let _mutation = autostart_lock();
    if task_exists() || run_key_set() != Some(true) {
        return;
    }
    if run_key_disabled() {
        // Task Manager's Disable leaves the Run value in place and starts
        // nothing, so a logon task here would turn back on what the user
        // turned off; the entry stays exactly as Windows left it.
        log("autostart is disabled in Task Manager, leaving the Run key alone");
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = exe.display().to_string();
    let ok = schtasks_cmd()
        .args(schtasks_create_args(&exe))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        if clear_run_key() == Some(true) {
            log("autostart moved from the Run key to a logon task");
        } else {
            // The delete's outcome was not confirmed - the value may have
            // survived or the confirming read may have failed - so both
            // mechanisms can still be registered.
            log("autostart moved to a logon task, but the Run key could not be retired");
        }
    } else {
        // Leave the Run key alone: a machine whose task creation failed keeps
        // starting through it next boot rather than not starting at all.
        log("autostart migration to a logon task failed, keeping the Run key");
    }
}

/// Whether a disable attempt left the logon task gone.
///
/// The one place the delete's exit code and the scheduler's answer are combined,
/// kept as a function because the combination is the rule: the exit code is
/// authority when it is zero, and a non-zero one has to be classified rather
/// than trusted. `Unknown` counts as not-gone, so an unanswerable query fails
/// closed instead of reporting a disable nobody confirmed. The scheduler query
/// arrives as a closure and is read only when the delete's exit code was
/// non-zero, so a delete that worked never asks the scheduler anything.
fn disable_left_it_gone(exited_zero: bool, after: impl FnOnce() -> TaskState) -> bool {
    exited_zero || after() == TaskState::Absent
}

/// Which autostart step did not take, so the message can name it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AutostartFailure {
    /// The scheduler command itself (create or delete) did not take.
    Scheduler,
    /// The scheduler held the requested state, but the Run-key delete that
    /// follows it was not confirmed.
    Retire,
}

/// The message for an autostart step that did not take, classified by a fresh
/// Run-key read of what the machine still holds.
///
/// One function for both steps, because the shape is identical - a total match
/// over the same `(enable, Option<bool>)` pair, differing only in wording - and
/// a fifth answer for that read would otherwise have to be added in two places
/// or silently become non-total in one of them.
///
/// Total over the read: every arm names what was actually seen, and a query
/// that cannot be answered says "unconfirmed" instead of asserting a machine
/// state nothing observed. Claiming either "no Run key" or "the key survived"
/// would assert something the read cannot support.
fn autostart_failure_message(
    kind: AutostartFailure,
    enable: bool,
    run_key: Option<bool>,
) -> String {
    use AutostartFailure::{Retire, Scheduler};
    match (kind, enable, run_key) {
        // Present: the Run key is the autostart that survived, and naming it
        // is accurate.
        (Scheduler, true, Some(true)) => {
            "schtasks.exe could not create the logon task, keeping the Run key".into()
        }
        (Scheduler, false, Some(true)) => {
            "schtasks.exe could not remove the logon task, keeping the Run key".into()
        }
        (Retire, true, Some(true)) => {
            "the Run key could not be retired, so mnvoice starts twice at logon".into()
        }
        (Retire, false, Some(true)) => {
            "the Run key could not be retired, so mnvoice still starts at logon".into()
        }
        // Absent: confirmed by a query that ran. The retire case's caller
        // settles this before calling, so that arm exists to keep the match
        // total rather than because a caller can reach it.
        (Scheduler, true, Some(false)) => {
            "schtasks.exe could not create the logon task, and no Run key is left to fall back on"
                .into()
        }
        (Scheduler, false, Some(false)) => "the logon task could not be confirmed removed".into(),
        (Retire, _, Some(false)) => "the Run key could not be confirmed removed".into(),
        // Unanswerable: reg.exe could not be spawned, so the machine state is
        // unknown. The same distinction `task_state` spells out with its
        // `Unknown` variant.
        (_, _, None) => "the Run key could not be read, so the autostart state is unconfirmed".into(),
    }
}

/// Register or remove the at-logon autostart task. Reversible, no admin rights
/// needed, and the Run key is cleared in both directions - but only once the
/// scheduler is confirmed to hold the state that was asked for, so a toggle
/// that did not take can never destroy the autostart the user had.
///
/// The read of the current state and the write of the new one are one operation
/// under the autostart lock. Doing them apart - reading the state in the caller
/// and handing the result here - let a concurrent toggle, or the startup
/// migration, change the state between the two, so two clicks in quick
/// succession could both read the same value and both write the same target.
/// The lock's own contract is that a mutation holds it across its reads, so the
/// read belongs inside it.
///
/// The guard is passed in rather than acquired here so a caller can hold one
/// across its own read, which a second acquire inside could not do: the guard is
/// not reentrant.
fn toggle_autostart() -> Result<(), String> {
    let mutation = autostart_lock();
    let enable = !autostart_enabled();
    set_autostart_guarded(mutation, enable)
}

/// The delete half of the task's lifecycle arguments.
///
/// Third of three (`schtasks_query_args`, `schtasks_create_args`, this). The
/// query and create lists are helpers with tests because their flags are
/// behaviour-critical; the delete list was spelled inline, which made it the
/// only one of the three that could lose its `/f` or mistype the task name
/// without anything noticing. `/f` is load-bearing: without it the scheduler
/// prompts for confirmation, and a prompt nobody answers is a toggle that
/// never completes.
fn schtasks_delete_args() -> Vec<String> {
    ["delete", "/tn", AUTOSTART_TASK, "/f"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn set_autostart_guarded(mutation: MutexGuard<'static, ()>, enable: bool) -> Result<(), String> {
    let _mutation = mutation;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.display().to_string();
    let mut cmd = schtasks_cmd();
    if enable {
        cmd.args(schtasks_create_args(&exe));
    } else {
        cmd.args(schtasks_delete_args());
    }
    let status = cmd.status().map_err(|e| e.to_string())?;
    let in_place = if enable {
        status.success()
    } else {
        // The delete's own exit code is authority: a zero already means the
        // task is gone, so the scheduler is only asked when it says otherwise.
        // A task that was never there already leaves the requested state in
        // place; one that is still registered, or a question the scheduler
        // could not answer, is a toggle that did not take - and there the Run
        // key is still the only autostart the user has.
        disable_left_it_gone(status.success(), task_state)
    };
    if !in_place {
        // Whatever carries the autostart until the scheduler holds the one
        // that was asked for stays as it is, and the toggle reports what did
        // not take rather than naming a fallback that is not there. So the
        // enable branch checks before it claims the Run key: naming it
        // unconditionally reported a fallback that may itself have been
        // retired, which is the exact thing this branch exists to avoid.
        return Err(autostart_failure_message(
            AutostartFailure::Scheduler,
            enable,
            run_key_set(),
        ));
    }
    if clear_run_key() != Some(true) {
        // The delete's outcome was not confirmed on the first read. A fresh
        // read settles it: a key that is now confirmed gone means the retire
        // took - the first read was the unanswerable one - and the toggle
        // falls through to success. Only a key still present, or still
        // unanswerable, fails it, and the message must not claim the first
        // when the read returned the second, because that is a machine state
        // the code cannot see.
        let read = run_key_set();
        if read != Some(false) {
            return Err(autostart_failure_message(
                AutostartFailure::Retire,
                enable,
                read,
            ));
        }
    }
    log(if enable { "autostart enabled" } else { "autostart disabled" });
    Ok(())
}

/// Open a companion file beside the exe in Notepad, creating it from a stub if
/// the user has not made one yet, so nobody gets a "file not found" dialog.
fn open_companion_file(name: &str, stub: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let path = dir.join(name);
    if !path.exists() {
        let _ = std::fs::write(&path, stub);
    }
    let _ = std::process::Command::new("notepad.exe").arg(&path).spawn();
}

static UPDATE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Ask GitHub whether a newer release exists. `quiet` suppresses the
/// "up to date" balloon so the periodic background check stays silent.
///
/// Runs on a worker thread: the request can take seconds and the tray menu
/// must not freeze. Installation only ever happens when idle, because swapping
/// the exe mid-dictation would lose the transcript in flight.
pub(crate) fn check_for_updates_async(quiet: bool) {
    if UPDATE_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        if !quiet {
            balloon("Update in progress", "an update check is already running");
        }
        return;
    }
    thread::spawn(move || {
        struct InFlightGuard;
        impl Drop for InFlightGuard {
            fn drop(&mut self) {
                UPDATE_IN_FLIGHT.store(false, Ordering::SeqCst);
            }
        }
        let _guard = InFlightGuard;

        // The check can take seconds and installs take longer; from here on a
        // manual check narrates every stage, because a tray button that goes
        // silent for twenty seconds reads as broken, not as busy.
        if !quiet {
            balloon("Checking for updates", "reading the release feed on github.com");
        }
        let rel = match update::check_latest() {
            Ok(r) => r,
            Err(e) => {
                log(&format!("update check failed: {e}"));
                if !quiet {
                    balloon("Update check failed", &e);
                }
                return;
            }
        };
        let current = platform::version();
        if !update::is_newer(&rel.version, &current) {
            log("mnvoice is up to date");
            if !quiet {
                balloon("mnvoice is up to date", &format!("v{current} is the latest version"));
            }
            return;
        }

        // Never install while the user is speaking or a transcript is in flight.
        //
        // This is the pre-download check, and it is deliberately NOT the only
        // one: `install_and_relaunch` takes `session_active` and passes it to
        // `stage_and_swap` as its `busy` closure, which runs after the download
        // and the checksum verify, immediately before the two renames. That inner
        // check is the one that can see a session that began while the bytes
        // were in flight, because it is the one evaluated at the moment of the
        // swap - which is why `update::tests::the_idle_gate_is_read_after_the
        // _download_and_stops_the_swap` drives it there rather than here.
        //
        // A second copy of the same test used to sit below, before the download,
        // where nothing between it and this one can change state. The review
        // round flagged it as redundant and it was removed: two copies of one
        // test is how the two copies drift apart, and this one only catches the
        // case where a session is already running when the check happens.
        let state = session_state();
        if state != State::Idle {
            log(&format!(
                "update v{} available, deferred (currently {})",
                rel.version,
                state_name(state)
            ));
            if !quiet {
                balloon(
                    "Update available",
                    &format!(
                        "v{} is available. Not installed while mnvoice is busy - check again when idle.",
                        rel.version
                    ),
                );
            }
            return;
        }

        log(&format!("installing v{}", rel.version));
        if !quiet {
            // The download, the checksum verify and the swap happen inside the
            // call below; this is the window where the user would otherwise
            // stare at a silent tray.
            balloon(
                &format!("Found v{}", rel.version),
                "downloading and verifying its published checksum",
            );
        }
        if let Err(e) = update::install_and_relaunch(&rel, session_active) {
            log(&format!("update failed: {e}"));
            if !quiet {
                // The user asked for this check, so an install that cannot
                // complete is news they get to see, not a log line only.
                balloon("Update failed", &e);
            }
        }
    });
}

/// True while a dictation session is in flight. The install path checks this
/// both before downloading and again right before the binary is swapped, so a
/// transcript can never be stranded mid-swap.
fn session_active() -> bool {
    session_state() != State::Idle
}

/// Current session state. Read from the shared atomic rather than from the
/// window, so it stays correct while the window exists, while it is being
/// created, and after it is gone.
fn session_state() -> State {
    State::from_code(SESSION_STATE.load(Ordering::SeqCst))
}

/// Transient notification from the tray icon. Balloon timeouts are only
/// advisory, so the tip is dismissed on the next interaction either way.
fn balloon(title: &str, body: &str) {
    unsafe {
        // The one pair of names every window on this app shares: a rename of
        // either used to leave this call compiling and finding nothing, which
        // silently stops every update balloon. The class name is also the
        // documented external contract for driving the tray (see AGENTS.md).
        let Ok(hwnd) = FindWindowW(CLASS_NAME, WINDOW_NAME) else {
            return;
        };
        let mut nid = tray_nid(hwnd, NIF_INFO | NIF_GUID);
        fill_wide(&mut nid.szInfoTitle, title);
        fill_wide(&mut nid.szInfo, body);
        nid.dwInfoFlags = NIIF_INFO;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Relaunch this exe with the restart argument so a fresh instance takes over,
/// then exit.
fn relaunch_for_restart() {
    // `update::spawn_restarted` is the one spelling of the handoff - the
    // installer and the recovery helper use the same function, and this is the
    // third site that used to copy the command out by hand. The tray ignores
    // a failure here because it is quitting in the same breath; whatever the
    // spawn reported, this process has already decided to leave.
    if let Ok(exe) = std::env::current_exe() {
        let _ = update::spawn_restarted(&exe);
    }
    unsafe { PostQuitMessage(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shell reads szTip/szInfo/szInfoTitle NUL-terminated, so a copy that
    /// fills the whole buffer leaves no terminator anywhere in it and the read
    /// runs off the end of the field. Nothing fed to these fields approaches
    /// their size today (szTip 128, szInfo 256, szInfoTitle 64), so this is a
    /// cliff removed rather than a bug fixed - but the contract the function
    /// claims is now upheld on the truncation path too, and this pins it.
    #[test]
    fn a_truncated_wide_copy_still_ends_with_a_terminator() {
        // A destination eight slots wide and a source longer than that.
        let mut dst = [0xAAAAu16; 8];
        fill_wide(&mut dst, "abcdefghij");
        // The first seven slots carry the text, and the eighth is the NUL the
        // shell stops on - not the eighth character.
        assert_eq!(&dst[..7], &wide("abcdefg")[..7]);
        assert_eq!(dst[7], 0, "the last slot must be the terminator: {dst:?}");
        assert!(
            !dst.contains(&0xAAAA),
            "every slot must be written, not just the ones copied: {dst:?}"
        );

        // A text that fits still terminates inside the buffer.
        let mut dst = [0xAAAAu16; 8];
        fill_wide(&mut dst, "short");
        assert_eq!(&dst[..5], &wide("short")[..5]);
        assert_eq!(dst[5], 0, "the text's own terminator survives");

        // A text exactly as long as the usable width keeps its terminator in
        // the last slot, which is the boundary the reservation exists for.
        let mut dst = [0xAAAAu16; 8];
        fill_wide(&mut dst, "abcdefg");
        assert_eq!(&dst[..7], &wide("abcdefg")[..7]);
        assert_eq!(dst[7], 0, "a text of exactly cap-1 still terminates");

        // An empty buffer is not a panic: nothing is written at all.
        let mut dst: [u16; 0] = [];
        fill_wide(&mut dst, "anything");
    }

    /// The XML the scheduler writes is the only place the enabled flag lives,
    /// and its two shapes are measured on this machine rather than assumed:
    /// an enabled task's document omits `<Enabled>` entirely, and
    /// `schtasks /change /tn <task> /disable` makes the same query emit
    /// `<Enabled>false</Enabled>` with the exit code unchanged at 0. So
    /// "absent" must read as enabled and only an explicit false as disabled -
    /// reading absence as disabled would draw the box unchecked for every
    /// working autostart.
    #[test]
    fn a_disabled_task_row_is_not_read_as_an_autostart() {
        let enabled_doc = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy></Settings>
</Task>"#;
        let disabled_doc = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Settings>
    <Enabled>false</Enabled>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
  </Settings>
</Task>"#;
        assert!(
            !task_xml_disabled(enabled_doc),
            "no <Enabled> element means enabled"
        );
        assert!(
            task_xml_disabled(disabled_doc),
            "the explicit false is the disabled answer"
        );
        // A `<Enabled>true</Enabled>` anywhere is not the disabled answer, so a
        // document that carries a true (here at trigger level) and says nothing
        // at task level still reads as enabled. Measured on this machine, the
        // scheduler's own enabled document carries no <Enabled> element at all,
        // so the true case below is the belt rather than the observed shape.
        assert!(
            enabled_doc.contains("<Enabled>true</Enabled>"),
            "the fixture must actually carry the true spelling it claims to tolerate"
        );
        // Indentation and line endings are the scheduler's, not ours.
        assert!(task_xml_disabled(
            "<Settings>\r\n  <Enabled>false</Enabled>\r\n</Settings>"
        ));
        // An empty or unreadable document is not a disabled answer either; the
        // caller classifies a document with no <Task> as Unknown before ever
        // asking this question.
        assert!(!task_xml_disabled(""));
    }

    /// The decoder accepts both byte shapes the scheduler's own declaration
    /// leaves open: the ASCII bytes measured on this machine, and a real
    /// UTF-16 document (which the `encoding="UTF-16"` declaration promises and
    /// a future build could honour). Getting this wrong is silent, so both
    /// paths are asserted rather than one plus an assumption.
    #[test]
    fn the_task_document_is_decoded_whichever_way_it_arrives() {
        let ascii = "<Task><Settings><Enabled>false</Enabled></Settings></Task>";
        assert!(task_xml_disabled(&task_xml_text(ascii.as_bytes())));

        let mut utf16le = vec![0xFFu8, 0xFE];
        for u in "Task".encode_utf16() {
            utf16le.extend_from_slice(&u.to_le_bytes());
        }
        let mut doc = vec![0xFFu8, 0xFE];
        for u in ascii.encode_utf16() {
            doc.extend_from_slice(&u.to_le_bytes());
        }
        assert!(
            task_xml_disabled(&task_xml_text(&doc)),
            "a UTF-16LE document must give the same disabled answer"
        );
        let mut utf16be = vec![0xFEu8, 0xFF];
        for u in ascii.encode_utf16() {
            utf16be.extend_from_slice(&u.to_be_bytes());
        }
        assert!(
            task_xml_disabled(&task_xml_text(&utf16be)),
            "a UTF-16BE document must too"
        );
        // The `<Task` guard the caller uses has to survive the decode as well.
        assert!(task_xml_text(&doc).contains("<Task"));
        // A truncated final code unit is a replacement character, not a panic.
        assert!(!task_xml_text(&[0xFF, 0xFE, b'T']).contains("<Task"));
    }

    /// The tray must be able to get the SMALL frames of the .ico.
    ///
    /// This is the assertion behind `tray_icon`. It measures which
    /// frame the loader actually picked (GetIconInfo -> GetObjectW on its
    /// bitmap) rather than trusting the size argument, and it is what showed
    /// `LoadIconW` resolving to SM_CXICON (measured here: 32 px) while the
    /// tray slot is SM_CXSMICON (16 px at this session's scaling) - so the
    /// frames built for the tray surface were unreachable. The two sizes are
    /// asserted apart because a single metric would make the test pass either
    /// way.
    ///
    /// Skipped when the test binary carries no embedded icon resource, which
    /// is a build fact, not a behaviour failure: what is being tested is which
    /// frame the loader selects from a set, and there is no set to select from
    /// without it.
    #[test]
    fn the_tray_asks_for_the_small_icon_frames() {
        let small = unsafe { GetSystemMetrics(SM_CXSMICON) };
        let large = unsafe { GetSystemMetrics(SM_CXICON) };
        // A metric of 0 would be a broken session, not a broken choice; and if
        // the two metrics were equal there would be no size difference to
        // detect, so the measurement below would prove nothing either way.
        if small <= 0 || large <= 0 || small == large {
            return;
        }
        let picked = unsafe { icon_size_of(tray_icon()) };
        let Some((w, _h)) = picked else {
            // No resource in this binary: nothing to measure.
            return;
        };
        assert_eq!(w, small, "the tray must load the small frame it draws into");
        // The window class keeps the default-size load, and this is the proof
        // that the two requests land on different frames: if the metrics were
        // equal, the assertion above would prove nothing either way.
        if let Some((lw, _)) = unsafe { icon_size_of(app_icon()) } {
            assert_eq!(lw, large, "the default load must stay on the large frame");
        }
    }

    /// The pixel size of a loaded icon, measured from its colour bitmap.
    /// Returns None when the icon carries no usable bitmap.
    ///
    /// Imported locally because the product code never measures a bitmap: this
    /// exists only to read back what the icon loader chose.
    unsafe fn icon_size_of(icon: HICON) -> Option<(i32, i32)> {
        use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP, HGDIOBJ};
        // This binding takes the struct as an out-parameter, not a return.
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        // Colour first, then the mask: a monochrome icon carries no colour
        // bitmap, and the mask is then the only real size to read.
        let bm = if !info.hbmColor.is_invalid() {
            info.hbmColor
        } else if !info.hbmMask.is_invalid() {
            info.hbmMask
        } else {
            return None;
        };
        let mut bmp = BITMAP::default();
        let ok = GetObjectW(
            HGDIOBJ(bm.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut core::ffi::c_void),
        );
        // The bitmaps GetIconInfo hands back are copies owned by the caller.
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        (ok != 0).then(|| (bmp.bmWidth, bmp.bmHeight))
    }

    #[test]
    fn the_query_arguments_do_not_ask_for_a_format() {
        // The answer comes from the exit status, which means the same thing
        // on every Windows. A listing is only readable when its field
        // separator is a comma, and that separator is the machine's list
        // separator, so no format argument may ever appear here to parse.
        let args = schtasks_query_args();
        // The exact list IS the rule: a `/fo` or `/format` anywhere in it would
        // fail this equality. The test used to carry a second assertion that
        // scanned for those flags, and it could not fail once the equality
        // passed - two spellings of one check, where only the strict one can
        // disagree.
        //
        // `/xml` is now part of the contract: it is what lets the answer say
        // "disabled" as well as "present", and it is parseable on every locale
        // because element names are English (a table's status text is not).
        assert_eq!(
            args,
            vec![
                "query".to_string(),
                "/tn".to_string(),
                AUTOSTART_TASK.to_string(),
                "/xml".to_string()
            ],
            "the existence-and-enabledness question is an xml query, with no format argument"
        );
    }

    #[test]
    fn the_create_arguments_pin_the_latency_fix() {
        // The flags are the entire point of the change: a logon trigger (the
        // Run key is started late and one-at-a-time), run as the current user
        // without elevation (keeps the tray in the user's session), and /f so
        // re-enabling over an existing task is not an error. The action's
        // command line carries the path in quotes, because the scheduler
        // stores the /tr value verbatim and a spaced path that is not quoted
        // resolves to the wrong program at logon.
        let spaced = r"C:\Users\Some User\bin\mnvoice.exe";
        let args = schtasks_create_args(spaced);
        for flag in ["/create", "/sc", "onlogon", "/rl", "limited", "/f"] {
            assert!(
                args.iter().any(|a| a == flag),
                "expected {flag} in {args:?}"
            );
        }
        let action = args
            .iter()
            .position(|a| a == "/tr")
            .and_then(|i| args.get(i + 1))
            .expect("the action follows /tr");
        assert_eq!(
            action,
            &format!("\"{spaced}\""),
            "the action is the path wrapped in quotes"
        );
        let already = schtasks_create_args(&format!("\"{spaced}\""));
        assert_eq!(
            already
                .iter()
                .position(|a| a == "/tr")
                .and_then(|i| already.get(i + 1)),
            Some(&format!("\"{spaced}\"")),
            "an already-quoted path is not quoted twice"
        );
    }

    #[test]
    fn the_delete_arguments_are_the_task_and_a_forced_removal() {
        // The third of the three lifecycle arg lists, and until now the only
        // one with no test. `/f` matters: without it schtasks prompts for
        // confirmation, and a prompt nobody answers hangs the toggle.
        assert_eq!(
            schtasks_delete_args(),
            vec![
                "delete".to_string(),
                "/tn".to_string(),
                AUTOSTART_TASK.to_string(),
                "/f".to_string()
            ]
        );
    }

    #[test]
    fn the_binary_hex_tail_becomes_bytes() {
        // The shape reg.exe actually prints for a StartupApproved blob.
        assert_eq!(
            parse_reg_binary_hex("030000009AB326D7E424DC01"),
            vec![0x03, 0x00, 0x00, 0x00, 0x9A, 0xB3, 0x26, 0xD7, 0xE4, 0x24, 0xDC, 0x01]
        );
        assert_eq!(
            parse_reg_binary_hex("02 00 00 00 00 00 00 00 00 00 00 00"),
            vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            "spacing is not part of the answer"
        );
        assert!(parse_reg_binary_hex("").is_empty(), "no hex is no blob");
    }

    #[test]
    fn only_the_disabled_flag_makes_a_blob_a_user_override() {
        // 02 is an entry Windows still starts; 03 and 06 are the flags Task
        // Manager writes when the user turns one off.
        assert!(!startup_approved_disabled(&[0x02, 0x00, 0x00, 0x00]));
        assert!(startup_approved_disabled(&[0x03, 0x00, 0x00, 0x00]));
        assert!(startup_approved_disabled(&[0x06, 0x00, 0x00, 0x00]));
        assert!(!startup_approved_disabled(&[]), "no blob is no override");
    }

    /// An unanswerable Run-key read must not read as "enabled".
    ///
    /// The toggle computes `enable = !autostart_enabled()`, so a state that
    /// wrongly reads as enabled makes a click run `schtasks /delete` on a task
    /// the user asked to create - the inverse of the request, in the one state
    /// the three-way read exists to name.
    #[test]
    fn an_unanswerable_run_key_does_not_read_as_enabled() {
        assert_eq!(autostart_state(None, || false), AutostartState::NoRunValue);
        assert!(!autostart_starting(autostart_state(None, || false)));

        assert_eq!(
            autostart_state(Some(false), || false),
            AutostartState::NoRunValue
        );
        assert_eq!(
            autostart_state(Some(true), || true),
            AutostartState::RunValueDisabled
        );
        assert_eq!(
            autostart_state(Some(true), || false),
            AutostartState::RunValueEnabled
        );

        // Two of the four answers settle without reading the Task Manager
        // override at all, which is the whole reason the override arrives as a
        // closure: behind it is a `reg.exe` spawn on the UI thread, paid on
        // every tray-menu open. A read that is evaluated and then discarded
        // still costs that spawn, so the panicking closure is the only check
        // that says whether the staging holds - if the second read were an
        // eager argument again, this test would die rather than pass.
        assert_eq!(
            autostart_state(Some(false), || panic!(
                "the override is not read when the Run value is confirmed absent"
            )),
            AutostartState::NoRunValue
        );
        assert_eq!(
            autostart_state(None, || panic!(
                "the override is not read when nothing could be read at all"
            )),
            AutostartState::NoRunValue
        );
    }

    #[test]
    fn the_tray_checkmark_follows_what_actually_starts() {
        // The logon task answers the question on its own, and so does a Run
        // value Windows still starts; an entry the user turned off in Task
        // Manager starts nothing, and so does a machine with neither.
        assert!(autostart_starting(AutostartState::TaskPresent));
        assert!(autostart_starting(AutostartState::RunValueEnabled));
        assert!(!autostart_starting(AutostartState::RunValueDisabled));
        assert!(!autostart_starting(AutostartState::NoRunValue));
    }

    #[test]
    fn hotkey_of_uses_config_defaults_when_config_is_none() {
        let (mods, vk, s) = hotkey_of(&None);
        assert_eq!(mods.0, config::DEFAULT_HOTKEY_MOD);
        assert_eq!(vk, config::DEFAULT_HOTKEY_VK);
        assert_eq!(s, config::DEFAULT_HOTKEY_STR);
    }

    #[test]
    fn the_tray_dictate_item_matches_the_session_state() {
        // The single tray item must be honest about the one action it offers:
        // idle starts a dictation, recording stops it, and while a session is
        // finishing the Transcribing hotkey press cancels for both protocols,
        // but a REST cancel discards the whole captured dictation (rest's entry
        // guard and post-round-trip check drop it) - work the user cannot get
        // back - so the item is shown disabled rather than as an enabled
        // "Dictate" that would silently do nothing.
        assert_eq!(dictate_item(State::Idle), ("Dictate", true));
        assert_eq!(dictate_item(State::Recording), ("Stop && transcribe", true));
        assert_eq!(
            dictate_item(State::Transcribing),
            ("Transcribing...", false)
        );
    }

    /// Without an API key dictation cannot start at all, so the idle tip has to
    /// say that rather than name a hotkey that does nothing. The app still
    /// starts in this state (there must be a tray menu to fix the file from),
    /// which is exactly why it needs saying: before, pressing the hotkey logged
    /// one line and returned, and nothing on screen explained why.
    #[test]
    fn a_missing_api_key_is_reported_instead_of_the_hotkey() {
        for ok in [true, false] {
            let tip = idle_tip("Alt+Space", ok, false);
            assert!(
                tip.contains("NO API KEY"),
                "expected the missing key to be named, got {tip:?}"
            );
            assert!(
                !tip.contains("to dictate"),
                "must not advertise a hotkey that cannot work: {tip:?}"
            );
        }
        // And the healthy case is unchanged.
        assert_eq!(
            idle_tip("Alt+Space", true, true),
            "mnvoice - idle. Alt+Space to dictate."
        );
    }

    #[test]
    fn the_idle_tray_tip_names_the_configured_hotkey() {
        // A user with HOTKEY=F9 must not be told to press Alt+Space, and with
        // no config yet the documented default is what the tip names.
        assert_eq!(idle_tip("F9", true, true), "mnvoice - idle. F9 to dictate.");
        assert_eq!(
            idle_tip("Alt+Space", true, true),
            "mnvoice - idle. Alt+Space to dictate."
        );
        // A hotkey that never registered keeps saying so: the first idle
        // write used to erase the startup UNAVAILABLE warning. This holds for
        // the default spelling too, which is what a config that failed to
        // load leaves the app running on.
        assert_eq!(
            idle_tip("F9", false, true),
            "mnvoice - idle. HOTKEY F9 UNAVAILABLE (in use by another app)"
        );
        assert_eq!(
            idle_tip("Alt+Space", false, true),
            "mnvoice - idle. HOTKEY Alt+Space UNAVAILABLE (in use by another app)"
        );
        // HOTKEY=none moved the control to the tray; the tip says where, and
        // the deliberately disabled key is not reported as a failed one.
        assert_eq!(
            idle_tip("none", true, true),
            "mnvoice - idle. Hotkey disabled - use the tray menu."
        );
        assert_eq!(
            idle_tip("none", false, true),
            "mnvoice - idle. Hotkey disabled - use the tray menu."
        );
    }

    #[test]
    fn a_failed_disable_only_counts_when_the_task_is_really_gone() {
        // A task that was never there already leaves the requested state in
        // place, so a failed delete is not always a failed toggle.
        assert!(disable_left_it_gone(false, || TaskState::Absent));
        assert!(disable_left_it_gone(true, || TaskState::Absent));
        // One that is still registered, or a question the scheduler could not
        // answer, is a toggle that did not take - so an unanswerable query
        // fails closed rather than reporting a disable nobody confirmed.
        assert!(!disable_left_it_gone(false, || TaskState::Present));
        assert!(!disable_left_it_gone(false, || TaskState::Unknown));
        // A zero exit code is authority and is not overturned by the scheduler:
        // the closure is not even read, which the panicking closure pins - if
        // it were evaluated the test would die.
        assert!(disable_left_it_gone(true, || panic!(
            "the scheduler is not asked when the delete exited zero"
        )));
    }

    /// Every message `set_autostart_guarded` can return names what the Run-key
    /// read actually saw, and an unanswerable read is reported as unconfirmed
    /// rather than as an absent or surviving key.
    #[test]
    fn autostart_failure_messages_name_what_was_seen() {
        use AutostartFailure::{Retire, Scheduler};
        assert_eq!(
            autostart_failure_message(Scheduler, true, Some(true)),
            "schtasks.exe could not create the logon task, keeping the Run key"
        );
        assert_eq!(
            autostart_failure_message(Scheduler, false, Some(true)),
            "schtasks.exe could not remove the logon task, keeping the Run key"
        );
        assert_eq!(
            autostart_failure_message(Scheduler, true, Some(false)),
            "schtasks.exe could not create the logon task, and no Run key is left to fall back on"
        );
        assert_eq!(
            autostart_failure_message(Scheduler, false, Some(false)),
            "the logon task could not be confirmed removed"
        );
        assert_eq!(
            autostart_failure_message(Scheduler, true, None),
            "the Run key could not be read, so the autostart state is unconfirmed"
        );
        assert_eq!(
            autostart_failure_message(Scheduler, false, None),
            "the Run key could not be read, so the autostart state is unconfirmed"
        );
        assert_eq!(
            autostart_failure_message(Retire, true, Some(true)),
            "the Run key could not be retired, so mnvoice starts twice at logon"
        );
        assert_eq!(
            autostart_failure_message(Retire, false, Some(true)),
            "the Run key could not be retired, so mnvoice still starts at logon"
        );
        // A confirmed-absent fresh read settles the retire at the caller, so
        // these two pin the arm's totality, not a reachable caller state.
        assert_eq!(
            autostart_failure_message(Retire, true, Some(false)),
            "the Run key could not be confirmed removed"
        );
        assert_eq!(
            autostart_failure_message(Retire, false, Some(false)),
            "the Run key could not be confirmed removed"
        );
        assert_eq!(
            autostart_failure_message(Retire, true, None),
            "the Run key could not be read, so the autostart state is unconfirmed"
        );
        assert_eq!(
            autostart_failure_message(Retire, false, None),
            "the Run key could not be read, so the autostart state is unconfirmed"
        );
    }

    #[test]
    fn autostart_mutations_never_overlap() {
        // Two threads inside their critical section at once is exactly the
        // interleaving that silently reversed a user's disable: the tray's
        // mutation and the migration thread must keep each other out.
        static INSIDE: AtomicBool = AtomicBool::new(false);
        let overlapping = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let overlapping = Arc::clone(&overlapping);
            handles.push(std::thread::spawn(move || {
                let _mutation = autostart_lock();
                if INSIDE.swap(true, Ordering::SeqCst) {
                    overlapping.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
                INSIDE.store(false, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.join().expect("a probe thread");
        }
        assert!(
            !overlapping.load(Ordering::SeqCst),
            "two mutations were inside their critical section at once"
        );
    }

    #[test]
    fn a_poisoned_autostart_lock_is_still_taken() {
        // A panicked thread must not wedge the tray, so the next mutation
        // takes the poisoned lock instead of unwrapping a poison error.
        let joiner = std::thread::spawn(|| {
            let _mutation = autostart_lock();
            panic!("poison the lock on purpose");
        });
        assert!(joiner.join().is_err(), "the probe panicked as intended");
        let _mutation = autostart_lock();
    }

    /// The restart teardown's three arms, driven from the value the scheduler
    /// returns rather than from a real taskkill.
    ///
    /// This is the only way to reach the two failure arms: through the real
    /// command a failure needs a victim the caller cannot terminate, and
    /// producing one takes an interactive UAC consent that stalls an automated
    /// run. See `restart_log_line`.
    #[test]
    fn the_restart_report_names_what_the_scheduler_actually_said() {
        // A teardown that worked, and the ordinary first restart where /FI
        // filtered every victim out: measured, taskkill exits 0 for both, so
        // the line says which code it saw and that the code cannot distinguish
        // them. It must not claim a kill.
        assert_eq!(
            restart_log_line("mnvoice.exe", Ok(Some(0))),
            "restart: taskkill exit 0 for other mnvoice.exe instances \
             (killed one, or none matched)"
        );
        // A non-zero exit is reported as the number the scheduler gave. A
        // restart log claiming a teardown that never happened is worse than no
        // log: the next line is a hotkey-registration failure with no cause
        // above it.
        assert_eq!(
            restart_log_line("mnvoice.exe", Ok(Some(5))),
            "restart: taskkill reported 5 for other mnvoice.exe instances"
        );
        // A status carrying no code at all reads as -1 rather than claiming
        // success or panicking.
        assert_eq!(
            restart_log_line("mnvoice.exe", Ok(None)),
            "restart: taskkill reported -1 for other mnvoice.exe instances"
        );
        // A taskkill that could not be spawned is its own answer, and it names
        // the reason rather than a code.
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "taskkill.exe missing");
        assert_eq!(
            restart_log_line("mnvoice.exe", Err(err)),
            "restart: could not run taskkill (taskkill.exe missing)"
        );
    }

    #[test]
    fn the_tray_state_tip_reflects_session_state() {
        assert_eq!(
            state_tip(State::Idle, "Alt+Space", true, true, None),
            "mnvoice - idle. Alt+Space to dictate."
        );
        assert_eq!(
            state_tip(State::Recording, "Alt+Space", true, true, None),
            "mnvoice - listening (auto-stops on silence)"
        );
        assert_eq!(
            state_tip(State::Transcribing, "Alt+Space", true, true, None),
            "mnvoice - transcribing..."
        );
    }

    /// The cancel-key warning has to survive the tip that follows it. A state
    /// push happens on every transition, so a warning written once at the
    /// failed registration is gone within microseconds and the rest of the
    /// session advertises a key that does nothing.
    #[test]
    fn a_cancel_key_that_was_never_claimed_keeps_saying_so() {
        let warning = "Escape could not be claimed, so there is no cancel key";
        for state in [State::Idle, State::Recording, State::Transcribing] {
            let tip = state_tip(state, "Alt+Space", true, true, Some(warning));
            assert!(
                tip.contains(warning),
                "the warning must be present in {}: {tip:?}",
                state_name(state)
            );
        }
        // And it drops out the moment the key is claimed again.
        assert_eq!(
            state_tip(State::Recording, "Alt+Space", true, true, None),
            "mnvoice - listening (auto-stops on silence)"
        );
    }

    /// The capture report is classified in every shape it can arrive in.
    ///
    /// The REST arm and the streaming arm share this one classifier, so a
    /// broken microphone is never reported as NO_SPEECH on either - telling a
    /// user whose Windows Audio service just stopped that they said nothing is
    /// the misleading diagnostic the four init_wasapi messages exist to
    /// prevent. Before B2 the streaming arm read only `Ok(Err(_))`, so a
    /// worker that panicked before its own `send` was reported as silence
    /// there and as a capture fault on REST.
    #[test]
    fn the_capture_report_is_classified_in_every_shape() {
        // The audio worker could not be handed the request at all, which is
        // the only Err capture_to_channel can return - and is the worker being
        // gone, not the audio engine, which would have reported through the
        // channel with its own message.
        assert_eq!(
            capture_fault(None).as_deref(),
            Some("capture failed: the audio worker is not running")
        );

        // The worker reported its own failure: that message is the answer.
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let _ = tx.send(Err("no input audio device found".to_string()));
        assert_eq!(
            capture_fault(Some(&rx)).as_deref(),
            Some("no input audio device found")
        );

        // The worker dropped its sender without reporting, which is what a
        // panic before its own `send` looks like. That is a broken
        // microphone, not silence.
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        drop(tx);
        assert_eq!(
            capture_fault(Some(&rx)).as_deref(),
            Some("capture failed: the audio worker stopped")
        );

        // A clean report is the only shape that is not a fault.
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let _ = tx.send(Ok(()));
        assert_eq!(capture_fault(Some(&rx)), None);
    }

    /// The shared session state has exactly one encode and one decode, and
    /// they agree. Nothing but declaration order kept them in agreement
    /// before, so inserting a variant - or reordering them - silently remapped
    /// the atomic the updater's worker thread reads to decide whether the
    /// binary may be swapped.
    #[test]
    fn the_state_code_table_round_trips_every_variant() {
        let states = [State::Idle, State::Recording, State::Transcribing];
        let codes: Vec<u8> = states.iter().map(|s| s.as_code()).collect();
        for (state, code) in states.iter().zip(codes.iter()) {
            let decoded = State::from_code(*code);
            if decoded != *state {
                panic!("code {code} must decode back to the state that produced it");
            }
            if state_name(*state).is_empty() {
                panic!("every state still has a name");
            }
        }
        // Distinct codes, or two states would be indistinguishable on the wire.
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            states.len(),
            "codes must be distinct: {codes:?}"
        );
        // The static's initial value is the same encode, not a second spelling.
        assert_eq!(SESSION_STATE.load(Ordering::SeqCst), State::Idle.as_code());
    }
}
