// Constructors for the backend of whichever platform is being built.
//
// Shared code asks for "the audio engine" and "the injector" and gets the
// platform's answer without a single cfg of its own.

#[cfg(target_os = "linux")]
pub use crate::platform::linux_impl::{audio, default_injector};

#[cfg(target_os = "macos")]
pub use crate::platform::macos_impl::{audio, default_injector};