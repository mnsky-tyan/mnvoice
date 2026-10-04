// Platform seams.
//
// mnvoice began as a Windows-only, Win32-native program: WASAPI for capture,
// SendInput for typing, WinHTTP for transport, a layered Win32 window for the
// orb, and renaming-the-running-image for self-update. Those choices are why
// capture starts in single-digit milliseconds and dictation never touches the
// clipboard, so they are worth keeping on Windows.
//
// This module is the boundary. Only three things get a trait, and only because
// each has genuinely different capabilities per platform rather than a different
// spelling of the same thing:
//
//   - `http`: swapping the transport library could change timing behaviour, so
//     it is pinned behind a boundary with tests that assert the redirect
//     contract the updater depends on.
//   - `audio`: the standby-engine latency property has to survive the port.
//   - `input`: Windows SendInput, X11 XTest and macOS CGEventTap differ in what
//     they can do at all (macOS needs a permission grant; Wayland needs a
//     portal), not just in how they are called.
//
// Everything else - the tray, global hotkeys, the single-instance guard, the
// autostart entry, the orb's window plumbing - stays a per-platform module
// behind `#[cfg]`. A trait for "show a tray icon" would have exactly one
// implementation per platform and no shared logic, which is abstraction for its
// own sake.

pub mod audio;
#[cfg(not(windows))]
pub mod cli;
pub mod http;
pub mod input;

#[cfg(windows)]
pub mod windows_impl;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod unix_audio;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod unix_http;

#[cfg(target_os = "linux")]
pub mod linux_impl;

#[cfg(target_os = "macos")]
pub mod macos_impl;

/// The version a tag names, with the leading `v` and the platform suffix
/// removed: "v0.1.15-win" -> "0.1.15". This is the consuming side's
/// definition of what a tag's version is; the publishing side keeps the same
/// rule in .github/actions/verify-release-tag. The binary derives its own
/// baked-in version from it (see `version` below), so the updater's comparison
/// can never disagree with the build about where a version ends and a platform
/// suffix begins. Lives in the platform seam because `update` is Windows-only
/// but the CLI prints the version on every platform.
pub fn version_of_tag(tag: &str) -> String {
    let bare = tag.strip_prefix('v').unwrap_or(tag);
    bare.split('-').next().unwrap_or(bare).to_string()
}

/// The version this build reports: the release tag baked by build.rs
/// (`MNVOICE_TAG`, falling back to the crate version for local builds), run
/// through the same parser the updater applies to feed tags. Windows and the
/// other platforms read the same value, so a release's three assets all
/// identify identically, and the build can never disagree with the updater
/// about where a version ends and a platform suffix begins.
pub fn version() -> String {
    version_of_tag(env!("MNVOICE_TAG"))
}

/// Which release asset the Windows updater downloads. The name is stable and
/// platform-specific so an existing install never has to be told twice, and it
/// is the exact name this release has always used, so the updater already
/// running on people's machines keeps resolving correctly. The Unix assets are
/// named by the release workflow, which is where they are defined.
#[cfg(windows)]
pub const fn asset_name() -> &'static str {
    "mnvoice.exe"
}
