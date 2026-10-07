# mnvoice - Ultra-low-latency Push-to-Talk Dictation

Native Rust dictation agent for Windows, Linux, and macOS.
Continuous background tray application with native audio capture, silence detection,
and direct keyboard event injection.

## Critical Test & Running Instance Rules

1. **NEVER KILL THE RUNNING `mnvoice.exe` PROCESS**:
   - The user runs a live `mnvoice.exe` instance continuously on this machine (`C:\Users\tyanw\bin\mnvoice.exe`).
   - It holds a single-instance named mutex (`mnvoice-single-instance`, session-local; see `MUTEX_NAME` in `src/windows_app.rs`).
   - Automated tests and test-step agents MUST NEVER kill, terminate, or replace any running `mnvoice.exe` process.
   - Do NOT touch or overwrite `C:\Users\tyanw\bin\mnvoice.exe` during tests or reviews (only during explicit release/update deployment).

2. **Toolchain & Testing Scope**:
   - The authoritative test suite is run via Windows cargo reached through the WSL mount:
     `WSLENV=CARGO_TARGET_DIR CARGO_TARGET_DIR=C:/Users/tyanw/AppData/Local/Temp/mnvoice-gate-target /mnt/c/Users/tyanw/.cargo/bin/cargo.exe test --locked`
   - Target directory MUST sit on the Windows disk because WSL 9p mounts do not support Windows file locking.
   - Pure unit tests exercise code paths safely without acquiring the single-instance mutex or touching audio hardware.
   - Do NOT create scratch probe executables, live window harnesses, or kill background processes.

3. **Release & Tag Discipline**:
   - Version bumps must update BOTH `Cargo.toml` AND `Cargo.lock` together (`cargo update -p mnvoice --offline` or `cargo build`).
   - Tags are published per-platform: `vX.Y.Z-win`, `vX.Y.Z-linux`, `vX.Y.Z-macos`.
