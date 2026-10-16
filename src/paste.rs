// Direct text input into the focused window via Win32 SendInput (KEYEVENTF_UNICODE).
// Keystrokes are sent directly into the active window at the cursor without
// touching or clobbering the system clipboard.

use std::thread;

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
            // One INPUT built per role, two events per character. The key-up
            // entry used to be spelled out in full beside the key-down one -
            // identical in every field except `dwFlags`, so the layout was
            // written twice per iteration and a change to wVk or time had to
            // land in both copies to keep the pair honest.
            let down = INPUT {
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
            };
            // `INPUT` is `Copy`/`Clone` (windows-rs derives both); this copies
            // the key-down event and flips the one field that differs. The
            // field write happens inside this function's `unsafe` block: the
            // anonymous union member is only valid to touch because `down` was
            // built through the same `ki` variant.
            let mut up = down.clone();
            up.Anonymous.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
            let inputs = [down, up];
            let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
            if sent != inputs.len() as u32 {
                return Err(format!(
                    "SendInput injected {sent} of {} events for U+{ch:04X}",
                    inputs.len()
                ));
            }
            // Small delay between characters so the target window processes each
            // keystroke before the next arrives. Without this, apps that buffer
            // input (browsers, terminals) can auto-repeat or drop events when
            // flooded with a large batch all at once.
            thread::sleep(crate::platform::input::KEY_GAP);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_returns_ok_without_injecting() {
        assert_eq!(type_text(""), Ok(()));
    }
}
