// Windows hides the console; the attribute is ignored by the other platforms.
#![windows_subsystem = "windows"]

// mnvoice - push-to-talk dictation.
//
// Windows: the full tray application - Alt+Space starts recording, speech is
// streamed and typed directly into the focused window, the orb indicates
// status. That lives in windows_app.
//
// Linux and macOS: the same engine with a terminal-driven control surface -
// Enter to start a dictation, silence or Enter to stop, words typed into the
// focused window and echoed to stdout. The orb and tray are Windows-native
// surfaces and have no cross-platform equivalent yet; the capture, transport
// and typing paths are the seam modules, shared with Windows.

#[cfg(windows)]
mod audio;
mod config;
mod locks;
#[cfg(windows)]
mod orb;
mod orb_render;
#[cfg(windows)]
mod paste;
mod platform;
mod rest;
mod stream;
#[cfg(test)]
mod test_hooks;
#[cfg(windows)]
mod update;
#[cfg(windows)]
mod windows_app;

#[cfg(windows)]
fn main() {
    windows_app::main();
}

#[cfg(target_os = "linux")]
fn main() {
    // The desktop app is the default control surface on Wayland/X11: hotkey,
    // orb and typing through the compositor. The terminal build stays
    // reachable with --terminal for sessions without a compositor (SSH, a
    // headless box).
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--terminal") {
        if let Err(e) = platform::cli::run() {
            eprintln!("mnvoice: {e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = platform::linux_app::main() {
        eprintln!("mnvoice: {e}");
        std::process::exit(1);
    }
}

// macOS has no desktop surface yet in this release; the terminal build is its
// control surface, as the README's Linux and macOS section documents.
#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = platform::cli::run() {
        eprintln!("mnvoice: {e}");
        std::process::exit(1);
    }
}
