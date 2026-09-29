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

mod config;
#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod orb;
#[cfg(windows)]
mod paste;
mod platform;
mod rest;
mod stream;
#[cfg(windows)]
mod update;
#[cfg(windows)]
mod windows_app;

#[cfg(windows)]
fn main() {
    windows_app::main();
}

#[cfg(not(windows))]
fn main() {
    if let Err(e) = platform::cli::run() {
        eprintln!("mnvoice: {e}");
        std::process::exit(1);
    }
}
