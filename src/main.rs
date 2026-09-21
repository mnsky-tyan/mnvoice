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

use std::fs::OpenOptions;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
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
    Shell_NotifyIconW, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIF_GUID, NIF_ICON,
    NIF_MESSAGE, NIF_TIP, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_WORKER: u32 = WM_APP + 2;

const HOTKEY_TOGGLE: i32 = 1;
const HOTKEY_ESC: i32 = 2;
const IDM_STOP: usize = 1;
const IDM_EXIT: usize = 2;
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

struct App {
    hwnd: HWND,
    state: State,
    config: Option<config::Config>,
    stop: Arc<AtomicBool>,
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--install-startup") {
        if let Err(e) = install_startup(true) { log(&format!("install-startup failed: {e}")); }
        return;
    }
    if args.iter().any(|a| a == "--uninstall-startup") {
        if let Err(e) = install_startup(false) { log(&format!("uninstall-startup failed: {e}")); }
        return;
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
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_ALREADY_EXISTS.into() {
            log("RegisterClassW failed");
            return;
        }

        let audio_engine = audio::AudioEngine::start();
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

        if let Err(e) = RegisterHotKey(
            hwnd,
            HOTKEY_TOGGLE,
            HOT_KEY_MODIFIERS(hk_mod),
            hk_vk,
        ) {
            log(&format!("RegisterHotKey({hk_str}) failed: {e}"));
        }

        add_tray(hwnd, "mnvoice - idle");

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
        hIcon: unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default(),
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
                    HOTKEY_TOGGLE | HOTKEY_ESC => toggle(app),
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
                let outcome = app.outcome.lock().unwrap().take();
                if let Some((ok, message)) = outcome {
                    let _ = UnregisterHotKey(hwnd, HOTKEY_ESC);
                    let _ = KillTimer(hwnd, TIMER_ORB);
                    if let Some(orb) = &mut app.orb {
                        orb.hide();
                    }
                    app.state = State::Idle;
                    let _ = set_tray_tip(hwnd, "mnvoice - idle");
                    if ok {
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
                    let app = app_ref(hwnd);
                    app.state = State::Idle;
                    PostQuitMessage(0);
                } else if id == IDM_STOP {
                    let app = app_ref(hwnd);
                    if app.state == State::Recording {
                        toggle(app);
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
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

            // Summon the orb IMMEDIATELY with zero perceptible latency
            if let Some(orb) = &mut app.orb {
                orb.show(orb::OrbState::Recording);
            }
            let _ = unsafe { SetTimer(app.hwnd, TIMER_ORB, 33, None) };

            app.stop.store(false, Ordering::SeqCst);
            let stop = app.stop.clone();
            let outcome = app.outcome.clone();
            let hwnd_bits = app.hwnd.0 as usize;
            let audio_engine = app.audio_engine.clone();

            // Worker immediately captures audio via pre-initialized standby engine & connects WebSocket
            let worker_cfg = cfg.clone();
            thread::spawn(move || worker(stop, worker_cfg, outcome, hwnd_bits, audio_engine));
            app.state = State::Recording;

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
            let _ = unsafe { set_tray_tip(app.hwnd, "mnvoice - listening (auto-stops on silence)") };
            log("recording started");
        }
        State::Recording => {
            app.stop.store(true, Ordering::SeqCst);
            app.state = State::Transcribing;
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

fn worker(
    stop: Arc<AtomicBool>,
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
            match stream::run_stream(&cfg, &stop, rx) {
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
                    let text = text.trim().to_string();
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
    let _ = AppendMenuW(menu, MF_STRING, IDM_STOP, w!("Stop && transcribe"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, w!("Exit"));
    if !recording {
        let _ = EnableMenuItem(menu, IDM_STOP as u32, MF_GRAYED);
    }
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
}

fn install_startup(install: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("no exe directory")?;
    let link = std::env::var("APPDATA")
        .map(|a| {
            std::path::PathBuf::from(a)
                .join("Microsoft\\Windows\\Start Menu\\Programs\\Startup\\mnvoice.lnk")
        })
        .map_err(|_| "APPDATA not set")?;
    let script = if install {
        format!(
            "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.WorkingDirectory='{}';$s.Description='mnvoice dictation';$s.Save()",
            link.display(),
            exe.display(),
            dir.display()
        )
    } else {
        format!("Remove-Item -LiteralPath '{}' -Force -ErrorAction SilentlyContinue", link.display())
    };
    let status = std::process::Command::new(
        "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
    )
    .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
    .creation_flags(0x0800_0000)
    .status()
    .map_err(|e| e.to_string())?;
    if status.success() {
        log(if install { "startup shortcut installed" } else { "startup shortcut removed" });
        Ok(())
    } else {
        Err("powershell exited non-zero".into())
    }
}
