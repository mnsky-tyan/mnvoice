// Direct text input into the focused window via Win32 SendInput (KEYEVENTF_UNICODE).
// Keystrokes are sent directly into the active window at the cursor without
// touching or clobbering the system clipboard.
// Also provides fallback clipboard paste_text.

use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

#[allow(dead_code)]
const CF_UNICODETEXT: u32 = 13;
#[allow(dead_code)]
const GMEM_MOVEABLE_FLAGS: GLOBAL_ALLOC_FLAGS = GLOBAL_ALLOC_FLAGS(0x0002);

/// Type text directly into the focused window using Unicode keyboard events.
/// Does not touch or overwrite the system clipboard.
pub fn type_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let mut inputs = Vec::with_capacity(utf16.len() * 2);

    for &ch in &utf16 {
        // Press
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: ch,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
        // Release
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: ch,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }

    unsafe {
        let sent = SendInput(&mut inputs, std::mem::size_of::<INPUT>() as i32);
        if sent != inputs.len() as u32 {
            return Err("SendInput keystroke failed".into());
        }
    }
    Ok(())
}

/// Fallback paste via clipboard + Ctrl+V.
#[allow(dead_code)]
pub fn paste_text(text: &str, trailing_space: bool) -> Result<(), String> {
    let mut text = text.to_string();
    if trailing_space {
        text.push(' ');
    }
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let hglobal = GlobalAlloc(GMEM_MOVEABLE_FLAGS, utf16.len() * 2)
            .map_err(|e| format!("allocate clipboard buffer ({e})"))?;
        let dst = GlobalLock(hglobal);
        if dst.is_null() {
            let _ = GlobalFree(hglobal);
            return Err("lock clipboard buffer".into());
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst as *mut u16, utf16.len());
        let _ = GlobalUnlock(hglobal);

        let mut opened = false;
        for _ in 0..25 {
            if OpenClipboard(HWND(std::ptr::null_mut())).is_ok() {
                opened = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        if !opened {
            let _ = GlobalFree(hglobal);
            return Err("clipboard is busy".into());
        }
        let _ = EmptyClipboard();
        let set = SetClipboardData(CF_UNICODETEXT, HANDLE(hglobal.0));
        let _ = CloseClipboard();
        if set.is_err() {
            let _ = GlobalFree(hglobal);
            return Err("set clipboard data".into());
        }
    }

    thread::sleep(Duration::from_millis(30));
    unsafe {
        let mut inputs = [
            key_input(VK_LCONTROL, false),
            key_input(VK_V, false),
            key_input(VK_V, true),
            key_input(VK_LCONTROL, true),
        ];
        let sent = SendInput(&mut inputs, std::mem::size_of::<INPUT>() as i32);
        if sent != inputs.len() as u32 {
            return Err("send Ctrl+V failed".into());
        }
    }
    Ok(())
}

#[allow(dead_code)]
unsafe fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
