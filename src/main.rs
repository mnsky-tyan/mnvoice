#![windows_subsystem = "windows"]

// mnvoice - push-to-talk dictation for Windows.
// Alt+Space starts recording, speech is streamed in real-time to the screen,
// auto-stops when silence is detected (or Esc stops), and text is pasted into
// the focused window. Transcription runs via Deepgram Nova-3 or Groq.

mod audio;
mod config;
mod groq;
mod orb;
mod paste;
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
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_NOREPEAT, VK_ESCAPE, VK_SPACE,
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
    log(&format!(
        "mnvoice started (pid {}, provider {:?}, model {})",
        unsafe { GetCurrentProcessId() },
        config.as_ref().map(|c| c.provider),
        config.as_ref().map(|c| c.model.as_str()).unwrap_or("none")
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

        let init = Box::into_raw(Box::new(AppInit { config, instance: hinstance }));
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
            MOD_ALT | MOD_NOREPEAT,
            VK_SPACE.0 as u32,
        ) {
            log(&format!("RegisterHotKey(Alt+Space) failed: {e}"));
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
                let orb = orb::Orb::new(init.instance).map_err(|e| {
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
            stream::WM_APP_STREAM_TOKEN => {
                let ptr = lparam.0 as *mut String;
                if !ptr.is_null() {
                    let text = *Box::from_raw(ptr);
                    let app = app_ref(hwnd);
                    if let Some(orb) = &mut app.orb {
                        orb.set_text(&text);
                    }
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
            if let Err(e) = audio::preflight() {
                log(&format!("preflight failed: {e}"));
                return;
            }
            app.stop.store(false, Ordering::SeqCst);
            let stop = app.stop.clone();
            let outcome = app.outcome.clone();
            let hwnd_bits = app.hwnd.0 as usize;
            thread::spawn(move || worker(stop, cfg, outcome, hwnd_bits));
            app.state = State::Recording;
            if let Err(e) = unsafe { RegisterHotKey(app.hwnd, HOTKEY_ESC, MOD_NOREPEAT, VK_ESCAPE.0 as u32) } {
                log(&format!("RegisterHotKey(Esc) failed: {e}"));
            }
            if let Some(orb) = &mut app.orb {
                orb.show(orb::OrbState::Recording);
            }
            let _ = unsafe { SetTimer(app.hwnd, TIMER_ORB, 33, None) };
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
) {
    let result = run_once(&stop, &cfg, hwnd_bits);
    *outcome.lock().unwrap() = Some(result);
    let hwnd = HWND(hwnd_bits as *mut std::ffi::c_void);
    let _ = unsafe { PostMessageW(hwnd, WM_APP_WORKER, WPARAM(0), LPARAM(0)) };
}

fn run_once(stop: &Arc<AtomicBool>, cfg: &config::Config, hwnd_bits: usize) -> (bool, String) {
    // If Deepgram is the provider, use real-time streaming tokens on screen
    if cfg.provider == config::Provider::Deepgram {
        match stream::run_stream(cfg, stop, hwnd_bits) {
            Ok(text) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    return (false, "No speech detected".into());
                }
                if let Err(e) = paste::paste_text(&text, cfg.trailing_space) {
                    log(&format!("paste error: {e}"));
                    return (false, format!("Paste failed: {e}"));
                }
                return (true, text);
            }
            Err(e) => {
                log(&format!("streaming error ({e}), falling back to batch"));
            }
        }
    }

    // Fallback batch mode
    let samples = match audio::capture(stop, cfg.max_seconds) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("capture error: {e}"));
            return (false, format!("Recording failed: {e}"));
        }
    };
    let secs = samples.len() as f32 / audio::SAMPLE_RATE as f32;
    log(&format!("captured {secs:.1}s of audio"));
    if samples.is_empty() {
        return (false, "No audio captured".into());
    }
    let wav = audio::wav_bytes(&samples);
    match groq::transcribe(cfg, &wav) {
        Ok(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                return (false, "No speech detected".into());
            }
            if let Err(e) = paste::paste_text(&text, cfg.trailing_space) {
                log(&format!("paste error: {e}"));
                return (false, format!("Paste failed: {e}"));
            }
            (true, text)
        }
        Err(e) => {
            log(&format!("transcription error: {e}"));
            (false, e)
        }
    }
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
