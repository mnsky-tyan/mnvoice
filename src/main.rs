#![windows_subsystem = "windows"]

// mnvoice - push-to-talk dictation for Windows.
// Alt+Space starts recording, speech is streamed and typed directly into the focused window,
// auto-stops when silence is detected (or Esc stops).
// A compact translucent glass orb with flowing fluid inside indicates status at the screen bottom.

mod audio;
mod config;
mod orb;
mod paste;
mod rest;
mod stream;
mod update;

use std::fs::OpenOptions;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_NOREPEAT,
};
use std::os::windows::process::CommandExt;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIF_GUID, NIF_ICON,
    NIF_INFO, NIF_MESSAGE, NIF_TIP, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_WORKER: u32 = WM_APP + 2;

/// Resource id of the icon embedded by build.rs via assets/mnvoice.ico.
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
const IDM_STOP: usize = 1;
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

/// Record a state change in both the UI's own copy and the shared atomic.
fn set_state(app: &mut App, state: State) {
    app.state = state;
    SESSION_STATE.store(state as u8, Ordering::SeqCst);
}

struct App {
    hwnd: HWND,
    state: State,
    config: Option<config::Config>,
    stop: Arc<AtomicBool>,
    /// Set by the cancel key. Once set, the streaming reader types nothing
    /// further and the final flush is skipped, so a cancel really discards.
    cancelled: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<(bool, String)>>>,
    orb: Option<orb::Orb>,
    audio_engine: audio::AudioEngine,
}

static LOG_LOCK: Mutex<()> = Mutex::new(());

fn log(msg: &str) {
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

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
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
    let _ = std::process::Command::new("C:\\Windows\\System32\\taskkill.exe")
        .args(["/F", "/IM", &name, "/FI", &format!("PID ne {self_pid}")])
        .creation_flags(0x0800_0000)
        .status();
    log(&format!("restart: terminated other {name} instances"));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == update::FINISH_UPDATE_ARG) {
        // A second, short-lived copy of this exe finishes an install the first one
        // could not. It waits for that process to be gone, and only then, and only
        // when the swap left the exe path empty, does it move the staged image in.
        // Started before the single-instance mutex is taken, which the app itself
        // is holding for as long as it is the one installing.
        update::finish_install();
        return;
    }
    if args.iter().any(|a| a == "--restart") {
        // Graceful self-heal: terminate any running instance, wait for it to
        // release the global hotkey, then continue starting fresh.
        kill_running_instances();
        thread::sleep(std::time::Duration::from_millis(700));
    }

    let _mutex = unsafe { CreateMutexW(None, true, MUTEX_NAME) }.ok();
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        log("second instance blocked, exiting");
        return;
    }

    let config = match config::load() {
        Ok(c) => Some(c),
        Err(e) => {
            log(&format!("config error: {e}"));
            None
        }
    };
    let kw_count = config.as_ref().map(|c| c.keywords.len()).unwrap_or(0);
    let (hk_mod, hk_vk, hk_str) = config
        .as_ref()
        .map(|c| (c.hotkey.0, c.hotkey.1, c.hotkey_str.clone()))
        .unwrap_or((MOD_ALT.0 | MOD_NOREPEAT.0, 0x20, "Alt+Space".to_string()));
    let cancel_str = config
        .as_ref()
        .map(|c| c.cancel_key_str.clone())
        .unwrap_or_else(|| "Escape".to_string());

    log(&format!(
        "mnvoice started (pid {}, protocol {:?}, model {}, hotkey: {}, cancel: {}, keywords: {})",
        unsafe { GetCurrentProcessId() },
        config.as_ref().map(|c| c.protocol),
        config.as_ref().map(|c| c.model.as_str()).unwrap_or("none"),
        hk_str,
        cancel_str,
        kw_count
    ));

    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let icon = app_icon();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: CLASS_NAME,
            hIcon: icon,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_ALREADY_EXISTS.into() {
            log("RegisterClassW failed");
            return;
        }

        let audio_engine = audio::AudioEngine::start();
        // Best-effort housekeeping: drop the leftover .old from a previous
        // update and arm the periodic background check when AUTO_UPDATE=1.
        update::startup_cleanup(config.as_ref().map(|c| c.auto_update).unwrap_or(false));
        let init = Box::into_raw(Box::new(AppInit { config, instance: hinstance, audio_engine }));
        let hwnd = match CreateWindowExW(
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
            Ok(h) => h,
            Err(e) => {
                log(&format!("CreateWindowExW failed: {e}"));
                return;
            }
        };

        // Register the global hotkey. If another process (or a stale registration
        // from a previously killed instance) still owns it, retry for a few seconds
        // before giving up, then surface a visible tray warning instead of silently
        // running with a dead hotkey.
        let mut hotkey_ok = false;
        for attempt in 0..10 {
            match RegisterHotKey(hwnd, HOTKEY_TOGGLE, HOT_KEY_MODIFIERS(hk_mod), hk_vk) {
                Ok(()) => {
                    hotkey_ok = true;
                    if attempt > 0 {
                        log(&format!("RegisterHotKey({hk_str}) succeeded on attempt {}", attempt + 1));
                    }
                    break;
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

        if hotkey_ok {
            add_tray(hwnd, &format!("mnvoice - idle ({hk_str})"));
        } else {
            add_tray(hwnd, &format!("mnvoice - HOTKEY {hk_str} UNAVAILABLE (in use by another app)"));
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

struct AppInit {
    config: Option<config::Config>,
    instance: HINSTANCE,
    audio_engine: audio::AudioEngine,
}

fn app_ref(hwnd: HWND) -> &'static mut App {
    unsafe { &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App) }
}

unsafe fn add_tray(hwnd: HWND, tip: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID,
        uCallbackMessage: WM_APP_TRAY,
        hIcon: unsafe { app_icon() },
        guidItem: TRAY_GUID,
        ..Default::default()
    };
    let tip_w = wide(tip);
    let n = tip_w.len().min(nid.szTip.len());
    nid.szTip[..n].copy_from_slice(&tip_w[..n]);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn set_tray_tip(hwnd: HWND, tip: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_TIP | NIF_GUID,
        guidItem: TRAY_GUID,
        ..Default::default()
    };
    let tip_w = wide(tip);
    let n = tip_w.len().min(nid.szTip.len());
    nid.szTip[..n].copy_from_slice(&tip_w[..n]);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lparam.0 as *const CREATESTRUCTW);
                let init = Box::from_raw(cs.lpCreateParams as *mut AppInit);
                let color = init.config.as_ref().map(|c| c.orb_color).unwrap_or((1.0, 0.18, 0.58));
                let fluid = init.config.as_ref().map(|c| c.orb_fluid_level).unwrap_or(0.75);
                let orb = orb::Orb::new(init.instance, color, fluid).map_err(|e| {
                    log(&format!("orb init: {e}"));
                    e
                }).ok();
                let app = Box::into_raw(Box::new(App {
                    hwnd,
                    state: State::Idle,
                    config: init.config,
                    stop: Arc::new(AtomicBool::new(false)),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    outcome: Arc::new(Mutex::new(None)),
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
            audio::WM_APP_RECORDING_READY => {
                // Mic hardware is confirmed capturing. Show the orb now!
                let app = app_ref(hwnd);
                if app.state == State::Recording {
                    if let Some(orb) = &mut app.orb {
                        orb.show(orb::OrbState::Recording);
                    }
                    let _ = SetTimer(app.hwnd, TIMER_ORB, 33, None);
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
                    let tip = match app.state {
                        State::Idle => "mnvoice - idle. Alt+Space to dictate.",
                        State::Recording => "mnvoice - listening... (auto-stops on silence)",
                        State::Transcribing => "mnvoice - transcribing...",
                    };
                    let _ = set_tray_tip(hwnd, tip);
                }
                LRESULT(0)
            }
            WM_APP_WORKER => {
                let app = app_ref(hwnd);
                let cancelled = app.cancelled.load(Ordering::SeqCst);
                let outcome = app.outcome.lock().unwrap().take();
                if let Some((ok, message)) = outcome {
                    let _ = UnregisterHotKey(hwnd, HOTKEY_ESC);
                    let _ = KillTimer(hwnd, TIMER_ORB);
                    if let Some(orb) = &mut app.orb {
                        orb.hide();
                    }
                    set_state(app, State::Idle);
                    let _ = set_tray_tip(hwnd, "mnvoice - idle");
                    // A cancelled session was already closed by cancel(); whatever
                    // the worker scraped together afterwards is deliberately dropped
                    // and must not be reported as a transcription.
                    if cancelled {
                        log("session cancelled, nothing typed");
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
                let id = wparam.0 as usize;
                if id == IDM_EXIT {
                    let _ = Shell_NotifyIconW(
                        NIM_DELETE,
                        &NOTIFYICONDATAW {
                            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                            hWnd: hwnd,
                            uID: 1,
                            guidItem: TRAY_GUID,
                            ..Default::default()
                        },
                    );
                    // Release the global hotkey so the next launch can claim it.
                    // Without this, a killed or exited instance can leave Windows
                    // still believing the hotkey is owned, breaking the next start.
                    let _ = UnregisterHotKey(hwnd, HOTKEY_TOGGLE);
                    let _ = UnregisterHotKey(hwnd, HOTKEY_ESC);
                    let app = app_ref(hwnd);
                    set_state(app, State::Idle);
                    PostQuitMessage(0);
                } else if id == IDM_STOP {
                    let app = app_ref(hwnd);
                    if app.state == State::Recording {
                        toggle(app);
                    }
                } else if id == IDM_STARTUP {
                    // Toggle the registry entry. Re-checked on next open, so the
                    // checkbox can never drift out of sync with reality.
                    let enable = !autostart_enabled();
                    if let Err(e) = set_autostart(enable) {
                        log(&format!("autostart toggle failed: {e}"));
                    }
                } else if id == IDM_RESTART {
                    relaunch_for_restart();
                } else if id == IDM_UPDATE {
                    check_for_updates_async(false);
                } else if id == IDM_OPEN_CONFIG {
                    open_companion_file(
                        "mnvoice.env",
                        "# mnvoice - see mnvoice.env.example for every key\nPROTOCOL=streaming\nAPI_KEY=\n",
                    );
                } else if id == IDM_OPEN_KEYWORDS {
                    open_companion_file(
                        "keywords.txt",
                        "# one word per line, or comma-separated\n",
                    );
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

            app.stop.store(false, Ordering::SeqCst);
            app.cancelled.store(false, Ordering::SeqCst);
            let stop = app.stop.clone();
            let cancelled = app.cancelled.clone();
            let outcome = app.outcome.clone();
            let hwnd_bits = app.hwnd.0 as usize;
            let audio_engine = app.audio_engine.clone();

            // Worker immediately captures audio via pre-initialized standby engine & connects WebSocket
            let worker_cfg = cfg.clone();
            thread::spawn(move || worker(stop, cancelled, worker_cfg, outcome, hwnd_bits, audio_engine));
            set_state(app, State::Recording);

            // Summon the orb last, once cancel is already live.
            if let Some(orb) = &mut app.orb {
                orb.show(orb::OrbState::Recording);
            }
            let _ = unsafe { SetTimer(app.hwnd, TIMER_ORB, 33, None) };
            let _ = unsafe { set_tray_tip(app.hwnd, "mnvoice - listening (auto-stops on silence)") };
            log("recording started");
        }
        State::Recording => {
            app.stop.store(true, Ordering::SeqCst);
            set_state(app, State::Transcribing);
            let _ = unsafe { UnregisterHotKey(app.hwnd, HOTKEY_ESC) };
            if let Some(orb) = &mut app.orb {
                orb.set_state(orb::OrbState::Transcribing);
            }
            let _ = unsafe { set_tray_tip(app.hwnd, "mnvoice - transcribing...") };
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
    let _ = unsafe { set_tray_tip(app.hwnd, "mnvoice - idle") };
    log("recording cancelled");
}

fn worker(
    stop: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    cfg: config::Config,
    outcome: Arc<Mutex<Option<(bool, String)>>>,
    hwnd_bits: usize,
    audio_engine: audio::AudioEngine,
) {
    let hwnd = HWND(hwnd_bits as *mut std::ffi::c_void);
    let (tx, rx) = std::sync::mpsc::channel();
    let stop_audio = stop.clone();
    let max_seconds = cfg.max_seconds;

    // 1. Immediately activate capture via pre-initialized standby WASAPI engine (latency ~4ms!)
    let capture_done_rx = audio_engine.capture_to_channel(
        stop_audio,
        max_seconds,
        cfg.vad_silence_ms,
        cfg.vad_rms_threshold,
        tx,
    );

    // 2. Concurrently run transcription (streaming WebSocket or REST fallback)
    let result = match cfg.protocol {
        config::Protocol::Streaming => {
            match stream::run_stream(&cfg, &stop, &cancelled, rx) {
                Ok(text) => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        (false, "No speech detected".into())
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
            let wav = audio::wav_bytes(&samples);
            match rest::transcribe(&cfg, &wav) {
                Ok(text) => {
                    // No provider here exposes a native filler_words parameter, so
                    // disfluencies are removed locally before anything is typed.
                    let text = if cfg.strip_fillers {
                        rest::strip_disfluencies(text.trim())
                    } else {
                        text.trim().to_string()
                    };
                    if text.is_empty() {
                        (false, "No speech detected".into())
                    } else {
                        let _ = paste::type_text(&text);
                        if cfg.trailing_space {
                            let _ = paste::type_text(" ");
                        }
                        (true, text)
                    }
                }
                Err(e) => (false, e),
            }
        }
    };

    if let Ok(rx) = capture_done_rx {
        let _ = rx.recv();
    }
    *outcome.lock().unwrap() = Some(result);
    let _ = unsafe { PostMessageW(hwnd, WM_APP_WORKER, WPARAM(0), LPARAM(0)) };
}

unsafe fn show_menu(hwnd: HWND) {
    let app = app_ref(hwnd);
    let recording = app.state == State::Recording;
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
    let _ = AppendMenuW(menu, MF_STRING, IDM_STOP, w!("Stop && transcribe"));
    if !recording {
        let _ = EnableMenuItem(menu, IDM_STOP as u32, MF_GRAYED);
    }
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, w!("Exit"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
}

const AUTOSTART_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const AUTOSTART_VALUE: &str = "mnvoice";

/// reg.exe with CREATE_NO_WINDOW, so toggling autostart never flashes a console.
fn reg_cmd() -> std::process::Command {
    let mut c = std::process::Command::new("C:\\Windows\\System32\\reg.exe");
    c.creation_flags(0x0800_0000);
    c
}

/// True when a mnvoice autostart entry exists for the current user.
fn autostart_enabled() -> bool {
    let Ok(out) = reg_cmd()
        .args(["query", AUTOSTART_RUN_KEY, "/v", AUTOSTART_VALUE])
        .output()
    else {
        return false;
    };
    out.status.success()
}

/// Register or remove the current-user autostart entry. Reversible, no admin
/// rights needed, and visible in Task Manager's Startup tab.
fn set_autostart(enable: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = reg_cmd();
    if enable {
        cmd.args([
            "add",
            AUTOSTART_RUN_KEY,
            "/v",
            AUTOSTART_VALUE,
            "/t",
            "REG_SZ",
            "/d",
            exe.display().to_string().as_str(),
            "/f",
        ]);
    } else {
        cmd.args(["delete", AUTOSTART_RUN_KEY, "/v", AUTOSTART_VALUE, "/f"]);
    }
    let status = cmd.status().map_err(|e| e.to_string())?;
    if status.success() {
        log(if enable { "autostart enabled" } else { "autostart disabled" });
        Ok(())
    } else {
        Err("reg.exe exited non-zero".into())
    }
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

/// Ask GitHub whether a newer release exists. `quiet` suppresses the
/// "up to date" balloon so the periodic background check stays silent.
///
/// Runs on a worker thread: the request can take seconds and the tray menu
/// must not freeze. Installation only ever happens when idle, because swapping
/// the exe mid-dictation would lose the transcript in flight.
fn check_for_updates_async(quiet: bool) {
    thread::spawn(move || {
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
        let current = update::current_version().to_string();
        if !update::is_newer(&rel.version, &current) {
            log("mnvoice is up to date");
            if !quiet {
                balloon("mnvoice is up to date", &format!("v{current} is the latest version"));
            }
            return;
        }

        // Never install while the user is speaking or a transcript is in flight.
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
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_INFO | NIF_GUID,
            guidItem: TRAY_GUID,
            ..Default::default()
        };
        let t = wide(title);
        let n = t.len().min(nid.szInfoTitle.len());
        nid.szInfoTitle[..n].copy_from_slice(&t[..n]);
        let b = wide(body);
        let n = b.len().min(nid.szInfo.len());
        nid.szInfo[..n].copy_from_slice(&b[..n]);
        nid.dwInfoFlags = NIIF_INFO;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Relaunch this exe with --restart so a fresh instance takes over, then exit.
fn relaunch_for_restart() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(&exe).arg("--restart").spawn();
    }
    unsafe { PostQuitMessage(0) };
}
