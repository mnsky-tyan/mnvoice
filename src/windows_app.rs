// mnvoice - push-to-talk dictation for Windows.
// Alt+Space starts recording, speech is streamed and typed directly into the focused window,
// auto-stops when silence is detected (or Esc stops).
// A compact translucent glass orb with flowing fluid inside indicates status at the screen bottom.

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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
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

pub const NO_SPEECH: &str = "No speech detected";

/// Resource id of the icon embedded by build.rs via assets/mnvoice.ico.
/// MAKEINTRESOURCEW(1) in the Windows API is `1 as *const u16`.
#[allow(clippy::manual_dangling_ptr)]
const IDI_APP: PCWSTR = PCWSTR(1 as *const u16);

/// Loads the embedded mnvoice icon. Falls back to the OS generic icon if the
/// resource is somehow missing, so the tray icon is never blank.
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
static SESSION_STATE: AtomicU8 = AtomicU8::new(State::Idle as u8);

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
    SESSION_STATE.store(state as u8, Ordering::SeqCst);
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
    orb: Option<orb::Orb>,
    audio_engine: audio::AudioEngine,
}

static LOG_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn log(msg: &str) {
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("mnvoice.log"))
    {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {msg}");
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
        .creation_flags(0x0800_0000)
        .status()
        .map(|s| s.code());
    // Report what the scheduler actually said. `let _ = ...status()` dropped
    // the ExitStatus, so this line was written whether taskkill ran at all, was
    // filtered to zero victims, or failed - a restart log claiming a teardown
    // that never happened is worse than no log, because the next thing on
    // screen is a hotkey-registration failure with no cause above it.
    log(&restart_log_line(&name, reported));
}

/// What the restart teardown writes to the log, given what the scheduler
/// actually reported: `Ok(Some(0))` is a taskkill that ran and succeeded,
/// `Ok(Some(n))` one that ran and reported `n`, `Ok(None)` one that ended with
/// no code at all, and `Err` one that could not be spawned.
///
/// Pure on purpose. The two failure arms cannot be reached through the real
/// command without a taskkill that genuinely fails, and with
/// `/FI "PID ne <pid>"` present a no-victim restart exits 0 - so the only
/// benign way to a failure is a victim the caller cannot terminate, an
/// elevated or protected process. Producing one takes an interactive UAC
/// consent, which stalls an automated run indefinitely and leaves a prompt on
/// the secure desktop that a non-elevated process cannot dismiss. Classifying
/// a value means the arms are pinned by a unit test with no process spawned.
fn restart_log_line(name: &str, reported: std::io::Result<Option<i32>>) -> String {
    match reported {
        Ok(Some(0)) => format!("restart: terminated other {name} instances"),
        // taskkill exits non-zero both for "no process matched" (the ordinary
        // first-restart case, filtered out by /FI) and for a real failure.
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
        thread::spawn(migrate_autostart);

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
        app_ref(hwnd).hotkey_ok = hotkey_ok;
        add_tray(hwnd, &idle_tip(&hk_str, hotkey_ok, app_ref(hwnd).config.is_some()));

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
        update::finish_install(args.get(pos + 1).map(std::path::Path::new));
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
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_ALREADY_EXISTS {
            log("RegisterClassW failed");
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
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
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

fn app_ref(hwnd: HWND) -> &'static mut App {
    unsafe { &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App) }
}

unsafe fn add_tray(hwnd: HWND, tip: &str) {
    let mut nid = tray_nid(hwnd, NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID);
    nid.uCallbackMessage = WM_APP_TRAY;
    nid.hIcon = unsafe { app_icon() };
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
    match (hotkey, hotkey_ok) {
        ("none", _) => "mnvoice - idle. Hotkey disabled - use the tray menu.".to_string(),
        (hk, true) => format!("mnvoice - idle. {hk} to dictate."),
        (hk, false) => format!("mnvoice - idle. HOTKEY {hk} UNAVAILABLE (in use by another app)"),
    }
}

/// The tray tooltip for a given session state. Single-sourced so left-click
/// polling and state transitions never disagree on tooltip text.
fn state_tip(state: State, hotkey_str: &str, hotkey_ok: bool, has_config: bool) -> String {
    match state {
        State::Idle => idle_tip(hotkey_str, hotkey_ok, has_config),
        State::Recording => "mnvoice - listening (auto-stops on silence)".to_string(),
        State::Transcribing => "mnvoice - transcribing...".to_string(),
    }
}

unsafe fn set_tray_tip(hwnd: HWND, tip: &str) {
    let mut nid = tray_nid(hwnd, NIF_TIP | NIF_GUID);
    fill_wide(&mut nid.szTip, tip);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// The tray icon's identity, shared by every `Shell_NotifyIconW` call: the
/// window, the id and the GUID are what make four different calls address
/// the same icon.
fn tray_nid(hwnd: HWND, flags: NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
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
                let app = Box::into_raw(Box::new(App {
                    hwnd,
                    state: State::Idle,
                    config: init.config,
                    stop: Arc::new(AtomicBool::new(false)),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    outcome: Arc::new(Mutex::new(None)),
                    session_id: 0,
                    hotkey_str: init.hotkey_str,
                    // The registration result arrives after WM_CREATE (the
                    // hwnd did not exist yet); main sets the real value.
                    hotkey_ok: true,
                    orb,
                    audio_engine: init.audio_engine,
                }));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_ORB {
                    let app = app_ref(hwnd);
                    if let Some(orb) = &mut app.orb {
                        orb.tick();
                    }
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                let app = app_ref(hwnd);
                match wparam.0 as i32 {
                    HOTKEY_TOGGLE => toggle(app),
                    // Esc cancels and hides the orb at once. It must not "finish"
                    // the phrase, which used to leave the orb sitting on screen.
                    HOTKEY_ESC => cancel(app),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_TRAY => {
                let event = lparam.0 as u32;
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    show_menu(hwnd);
                } else if event == WM_LBUTTONUP {
                    let app = app_ref(hwnd);
                    set_tray_tip(
                        hwnd,
                        &state_tip(app.state, &app.hotkey_str, app.hotkey_ok, app.config.is_some()),
                    );
                }
                LRESULT(0)
            }
            WM_APP_WORKER => {
                let app = app_ref(hwnd);
                let msg_session = wparam.0;
                if msg_session != app.session_id {
                    // Stale outcome from an earlier session; ignore completely.
                    return LRESULT(0);
                }
                let cancelled = app.cancelled.load(Ordering::SeqCst);
                let outcome = app.outcome.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some((ok, message)) = outcome {
                    let _ = UnregisterHotKey(hwnd, HOTKEY_ESC);
                    let _ = KillTimer(hwnd, TIMER_ORB);
                    if let Some(orb) = &mut app.orb {
                        orb.hide();
                    }
                    set_state(app, State::Idle);
                    set_tray_tip(
                        hwnd,
                        &state_tip(State::Idle, &app.hotkey_str, app.hotkey_ok, app.config.is_some()),
                    );
                    // A cancelled session was already closed by cancel(); whatever
                    // the worker scraped together afterwards is deliberately dropped
                    // and must not be reported as a transcription. On REST nothing
                    // was typed either; on streaming, words committed before the
                    // cancel are already in the document, so the line says what
                    // cancel actually guarantees: the rest is discarded.
                    if cancelled {
                        log("session cancelled, discarding the rest");
                    } else if ok {
                        let preview: String = message.chars().take(200).collect();
                        log(&format!("transcribed: {preview}"));
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
                        let _ = Shell_NotifyIconW(NIM_DELETE, &tray_nid(hwnd, Default::default()));
                        // Release the global hotkey so the next launch can claim it.
                        // Without this, a killed or exited instance can leave Windows
                        // still believing the hotkey is owned, breaking the next start.
                        let _ = UnregisterHotKey(hwnd, HOTKEY_TOGGLE);
                        let _ = UnregisterHotKey(hwnd, HOTKEY_ESC);
                        let app = app_ref(hwnd);
                        set_state(app, State::Idle);
                        PostQuitMessage(0);
                    }
                    IDM_DICTATE => {
                        let app = app_ref(hwnd);
                        toggle(app);
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
                            let enable = !autostart_enabled();
                            if let Err(e) = set_autostart(enable) {
                                log(&format!("autostart toggle failed: {e}"));
                            }
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
                if let Err(e) = unsafe {
                    RegisterHotKey(
                        app.hwnd,
                        HOTKEY_ESC,
                        HOT_KEY_MODIFIERS(cfg.cancel_key.0),
                        cfg.cancel_key.1,
                    )
                } {
                    log(&format!("RegisterHotKey({}) failed: {e}", cfg.cancel_key_str));
                }
            }

            // Fresh flags and a fresh outcome slot per session, never a reset:
            // a stale worker from the cancelled session writes to arcs nothing
            // reads anymore (its cancelled flag stays true, so its late flush
            // cannot type, and its outcome lands in an orphaned slot the UI
            // handler skips) and can no longer tear the new session down.
            app.session_id = app.session_id.wrapping_add(1);
            let session_id = app.session_id;
            app.stop = Arc::new(AtomicBool::new(false));
            app.cancelled = Arc::new(AtomicBool::new(false));
            app.outcome = Arc::new(Mutex::new(None));
            let stop = app.stop.clone();
            let cancelled = app.cancelled.clone();
            let outcome = app.outcome.clone();
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
            unsafe {
                set_tray_tip(
                    app.hwnd,
                    &state_tip(State::Recording, &app.hotkey_str, app.hotkey_ok, app.config.is_some()),
                )
            };
            log("recording started");
        }
        State::Recording => {
            app.stop.store(true, Ordering::SeqCst);
            set_state(app, State::Transcribing);
            let _ = unsafe { UnregisterHotKey(app.hwnd, HOTKEY_ESC) };
            if let Some(orb) = &mut app.orb {
                orb.set_state(orb::OrbState::Transcribing);
            }
            unsafe {
                set_tray_tip(
                    app.hwnd,
                    &state_tip(State::Transcribing, &app.hotkey_str, app.hotkey_ok, app.config.is_some()),
                )
            };
            log("recording stopped, transcribing");
        }
        State::Transcribing => {}
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
    set_state(app, State::Idle);
    let _ = unsafe { KillTimer(app.hwnd, TIMER_ORB) };
    let _ = unsafe { UnregisterHotKey(app.hwnd, HOTKEY_ESC) };
    if let Some(orb) = &mut app.orb {
        orb.hide();
    }
    unsafe {
        set_tray_tip(
            app.hwnd,
            &state_tip(State::Idle, &app.hotkey_str, app.hotkey_ok, app.config.is_some()),
        )
    };
    log("recording cancelled");
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
    // The outer Err means the request could not even be queued (the engine is
    // gone); the original code ignored that case and so does this - it is not
    // a capture failure the user can act on.
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
    let mut result = match cfg.protocol {
        config::Protocol::Streaming => {
            match stream::run_stream(&cfg, &stop, &cancelled, rx) {
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
            }
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
            // Three shapes reach here and they are not the same outcome. A
            // clean `Ok(())` with a short buffer really is no speech. But
            // `capture_done == None` means `capture_to_channel` returned
            // `Err` (the engine was gone before the thread started) and
            // `Err(RecvError)` means the audio worker panicked before its
            // `send` - both of those are a broken microphone, and reporting
            // them as NO_SPEECH tells a user whose Windows Audio service just
            // stopped that they said nothing, which is exactly the misleading
            // diagnostic the four specific init_wasapi messages exist to
            // prevent. Only the third shape is silence.
            let capture_err = match &capture_done {
                None => Some("capture failed: the audio engine is unavailable".to_string()),
                Some(done) => match done.recv() {
                    Ok(Err(e)) => Some(e),
                    // The worker dropped its sender without sending, which for
                    // this channel means it did not reach its own report.
                    Err(_) => Some("capture failed: the audio worker stopped".to_string()),
                    Ok(Ok(())) => None,
                },
            };
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

    // Streaming leaves the capture report unread until here; the REST arm above
    // has already consumed it (a second recv sees a closed channel and no-ops).
    if let Some(rx) = capture_done {
        if let Ok(Err(e)) = rx.recv() {
            result = (false, e);
        }
    }
    *outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    let _ = unsafe { PostMessageW(hwnd, WM_APP_WORKER, WPARAM(session_id), LPARAM(0)) };
}

/// The tray menu's single dictate/stop item for a given session state.
///
/// One item carries every direction: with HOTKEY=none this is the only way to
/// start a dictation, and it must never be a grayed-out stop item while idle.
/// The label and enablement follow the state so the single action stays
/// honest: Transcribing has no click action at all (toggle's arm is a
/// deliberate no-op), so it shows a disabled status label rather than an
/// enabled "Dictate" that would silently do nothing. Pure so the mapping is
/// testable without a live window.
fn dictate_item(state: State) -> (&'static str, bool) {
    match state {
        State::Idle => ("Dictate", true),
        State::Recording => ("Stop && transcribe", true),
        State::Transcribing => ("Transcribing...", false),
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let app = app_ref(hwnd);
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    // Checkbox reflects live system state, so it is evaluated on every open.
    let startup_flags = if autostart_enabled() { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
    let _ = AppendMenuW(menu, MF_STRING | startup_flags, IDM_STARTUP, w!("Start with Windows"));

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
    c.creation_flags(0x0800_0000);
    c
}

fn schtasks_cmd() -> std::process::Command {
    let mut c = std::process::Command::new("C:\\Windows\\System32\\schtasks.exe");
    c.creation_flags(0x0800_0000);
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
    /// Neither mechanism is registered, so nothing starts at logon.
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
/// spawn on the UI thread, so the staging matters: the logon task settles the
/// question on its own in the steady state, and the Run value's Task Manager
/// override is only read while an old Run entry may still be there.
fn autostart_enabled() -> bool {
    let state = if task_exists() {
        AutostartState::TaskPresent
    } else if !run_key_set() {
        AutostartState::NoRunValue
    } else if run_key_disabled() {
        AutostartState::RunValueDisabled
    } else {
        AutostartState::RunValueEnabled
    };
    autostart_starting(state)
}

fn run_key_set() -> bool {
    reg_cmd()
        .args(["query", AUTOSTART_RUN_KEY, "/v", AUTOSTART_VALUE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Remove the pre-migration Run key entry, reporting whether it is gone.
///
/// True when the value is not there any more, which is what `reg query`
/// confirms afterwards: a delete that did not take against a policy or a
/// security product leaves it in place, and nothing may read that as success.
fn clear_run_key() -> bool {
    let _ = reg_cmd()
        .args(["delete", AUTOSTART_RUN_KEY, "/v", AUTOSTART_VALUE, "/f"])
        .output();
    !run_key_set()
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

/// The schtasks arguments that ask whether the autostart task exists.
///
/// Deliberately no `/fo`: the exit status answers the question on every
/// Windows, while a listing's field separator is the machine's own list
/// separator (a semicolon on a default German system), so anything that
/// parsed that text was answering a question about the locale instead.
fn schtasks_query_args() -> Vec<String> {
    ["query", "/tn", AUTOSTART_TASK]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// What the scheduler says about the at-logon autostart task.
///
/// Absent and Unknown must never be merged: a task that is not there is the
/// state a disable asks for, while a query that could not be answered says
/// nothing at all and has to fail the toggle rather than look like success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskState {
    Present,
    Absent,
    Unknown,
}

/// Ask the scheduler about the task.
///
/// Only a process that could not be spawned at all is unanswerable: schtasks
/// exits zero for a task that is there and non-zero when it is not, and that
/// exit code is the only thing there is to ask.
fn task_state() -> TaskState {
    match schtasks_cmd().args(schtasks_query_args()).output() {
        Ok(out) if out.status.success() => TaskState::Present,
        Ok(_) => TaskState::Absent,
        Err(_) => TaskState::Unknown,
    }
}

fn task_exists() -> bool {
    task_state() == TaskState::Present
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
    AUTOSTART_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Move an older Run-key install onto the Task Scheduler, once, silently.
///
/// Called at startup so the fix happens by relaunching rather than by asking
/// the user to do anything. Both mechanisms firing for one boot is harmless -
/// the single-instance mutex makes the loser exit - and the task is left in
/// place, so the swap only ever runs in this direction.
pub fn migrate_autostart() {
    let _mutation = autostart_lock();
    if task_exists() || !run_key_set() {
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
        if clear_run_key() {
            log("autostart moved from the Run key to a logon task");
        } else {
            // The logon task holds the autostart, but the Run value survived
            // the delete, so both mechanisms are still registered.
            log("autostart moved to a logon task, but the Run key could not be retired");
        }
    } else {
        // Leave the Run key alone: a machine whose task creation failed keeps
        // starting through it next boot rather than not starting at all.
        log("autostart migration to a logon task failed, keeping the Run key");
    }
}

/// Whether turning autostart off left the logon task gone.
///
/// A delete that exited zero did its job. One that failed has to be
/// classified rather than trusted: a task that was never there already leaves
/// the requested state in place, while one that is still registered, or a
/// question the scheduler could not answer, is a toggle that did not take -
/// and there the Run key is still the only autostart the user has.
fn delete_left_task_gone(delete_ok: bool, after: TaskState) -> bool {
    delete_ok || after == TaskState::Absent
}

/// Register or remove the at-logon autostart task. Reversible, no admin rights
/// needed, and the Run key is cleared in both directions - but only once the
/// scheduler is confirmed to hold the state that was asked for, so a toggle
/// that did not take can never destroy the autostart the user had.
fn set_autostart(enable: bool) -> Result<(), String> {
    let _mutation = autostart_lock();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.display().to_string();
    let mut cmd = schtasks_cmd();
    if enable {
        cmd.args(schtasks_create_args(&exe));
    } else {
        cmd.args(["delete", "/tn", AUTOSTART_TASK, "/f"]);
    }
    let status = cmd.status().map_err(|e| e.to_string())?;
    let in_place = if enable {
        status.success()
    } else {
        // The delete's own exit code is authority: a zero already means the
        // task is gone, so the scheduler is only asked when it says otherwise.
        // The helper below takes `delete_ok` only to be callable from a test
        // that wants to exercise the failed-delete branch; the live call site
        // is inside this `else`, where a zero already short-circuited, so the
        // flag is always false here.
        status.success() || delete_left_task_gone(false, task_state())
    };
    if !in_place {
        // Whatever carries the autostart until the scheduler holds the one
        // that was asked for stays as it is, and the toggle reports what did
        // not take rather than naming a fallback that is not there.
        return Err(if enable {
            "schtasks.exe could not create the logon task, keeping the Run key".into()
        } else if run_key_set() {
            "schtasks.exe could not remove the logon task, keeping the Run key".into()
        } else {
            "the logon task could not be confirmed removed".into()
        });
    }
    if !clear_run_key() {
        // The Run value survived, so the app still starts at logon however
        // the task reads, and the log must not claim a move that half failed.
        return Err(if enable {
            "the Run key could not be retired, so mnvoice starts twice at logon".into()
        } else {
            "the Run key could not be retired, so mnvoice still starts at logon".into()
        });
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
        let Ok(hwnd) = FindWindowW(w!("mnvoiceTrayClass"), w!("mnvoice")) else {
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
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(&exe)
            .arg(update::RESTART_ARG)
            .spawn();
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

    #[test]
    fn the_query_arguments_do_not_ask_for_a_format() {
        // The answer comes from the exit status, which means the same thing
        // on every Windows. A listing is only readable when its field
        // separator is a comma, and that separator is the machine's list
        // separator, so no format argument may ever appear here to parse.
        let args = schtasks_query_args();
        assert_eq!(
            args,
            vec![
                "query".to_string(),
                "/tn".to_string(),
                AUTOSTART_TASK.to_string()
            ],
            "the existence question is a plain query"
        );
        assert!(
            !args.iter().any(|a| a
                .trim_start_matches('/')
                .eq_ignore_ascii_case("fo")
                || a.trim_start_matches('/').eq_ignore_ascii_case("format")),
            "expected no format argument in {args:?}"
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
        // finishing there is no click action (toggle's Transcribing arm is a
        // no-op), so it is shown disabled rather than as an enabled "Dictate"
        // that silently does nothing.
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
        // The delete's own exit code is authority, so a follow-up question
        // that could not be answered changes nothing.
        assert!(delete_left_task_gone(true, TaskState::Unknown));
        assert!(delete_left_task_gone(true, TaskState::Absent));
        // A delete that failed says nothing on its own: a task that was never
        // there already leaves the requested state in place.
        assert!(delete_left_task_gone(false, TaskState::Absent));
        assert!(!delete_left_task_gone(false, TaskState::Present));
        assert!(!delete_left_task_gone(false, TaskState::Unknown));
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
        // filtered every victim out: taskkill exits 0 for both.
        assert_eq!(
            restart_log_line("mnvoice.exe", Ok(Some(0))),
            "restart: terminated other mnvoice.exe instances"
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
            state_tip(State::Idle, "Alt+Space", true, true),
            "mnvoice - idle. Alt+Space to dictate."
        );
        assert_eq!(
            state_tip(State::Recording, "Alt+Space", true, true),
            "mnvoice - listening (auto-stops on silence)"
        );
        assert_eq!(
            state_tip(State::Transcribing, "Alt+Space", true, true),
            "mnvoice - transcribing..."
        );
    }
}
