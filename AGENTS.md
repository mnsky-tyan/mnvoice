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
   - After all three workflows succeed, publish release notes with
     `gh release edit <tag> --notes-file <file>`, then verify 6 assets + checksums.

4. **Triggering the In-App Update (verified 2026-10-08, v0.1.22 -> v0.1.23)**:
   - The running app checks for updates via a tray command, not an HTTP endpoint: post
     `WM_COMMAND (0x0111)` with `WPARAM = IDM_UPDATE (7)` and `LPARAM = 0` to the window of
     class `mnvoiceTrayClass` (enumerate with `FindWindowW`/`EnumWindows` and match by PID;
     the orb window is `mnvoiceOrbClass`). The app then downloads, sha256-verifies, swaps the
     exe, and restarts itself at the same path with a new PID.
   - Windows PowerShell trap: `$PID` is a built-in read-only variable. Naming a script variable
     `$pid` fails to assign and silently leaves the shell's own PID in it; use `$procId`.
   - Verify success by re-reading `Get-Process mnvoice` for the new version, and confirm the
     installed exe's `sha256sum` matches the published `mnvoice.exe` checksum.
   - WSL `/tmp` is not visible to `powershell.exe -File`; copy helpers under
     `C:\Users\tyanw\AppData\Local\Temp\` (reachable as `/mnt/c/Users/tyanw/AppData/Local/Temp/`)
     and delete them afterwards.
