// Direct text input into the focused window via Win32 SendInput (KEYEVENTF_UNICODE).
// Keystrokes are sent directly into the active window at the cursor without
// touching or clobbering the system clipboard.

use std::thread;
use std::time::Duration;

use windows::Win32::UI::Input::KeyboardAndMouse::*;

/// Type text directly into the focused window using Unicode keyboard events.
/// Does not touch or overwrite the system clipboard.
pub fn type_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    let utf16: Vec<u16> = text.encode_utf16().collect();

    unsafe {
        for &ch in &utf16 {
            let mut inputs = [
                INPUT {
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
                },
                INPUT {
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
                },
            ];
            SendInput(&mut inputs, std::mem::size_of::<INPUT>() as i32);
            // Small delay between characters so the target window processes each
            // keystroke before the next arrives. Without this, apps that buffer
            // input (browsers, terminals) can auto-repeat or drop events when
            // flooded with a large batch all at once.
            thread::sleep(Duration::from_millis(2));
        }
    }
    Ok(())
}
