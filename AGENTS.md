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
   - A green no-mistakes lint/test step describes the head it ran against, which may
     precede later fix commits. After a gate run finishes, re-run
     `cargo clippy --locked --all-targets` and both test suites on the final head before
     merging. This caught 2 clippy warnings in `src/platform/input.rs` that the lint step
     reported as clean because the CI fixer rewrote the file afterwards.
   - Fixtures in tests that serve MORE body than the client reads must not close the socket
     with bytes still queued, or park indefinitely in an unbounded write. On Linux that
     surfaces as `Connection reset by peer (os error 104)`, on macOS as `timed out reading
     response` - it is a flaky-test signature, not a production bug. Write in bounded chunks
     with a write timeout and drain until the client closes.
   - `FindWindowW("mnvoiceTrayClass", $null)` returns zero even while the window exists.
     Enumerate with `EnumWindows`, match `GetClassNameW` against `mnvoiceTrayClass` and
     `GetWindowThreadProcessId` against the target PID, then `PostMessageW` the returned
     hwnd. Verified again at the v0.1.24 update.
   - `gh release download -D <dir>` can exit 0 with no file written when invoked from WSL
     toward a Windows path. To verify a published asset, read its checksum file via
     `gh api repos/:owner/:repo/releases/assets/<id> -H "Accept: application/octet-stream"`
     and pipe the asset itself through the same endpoint into `sha256sum`; that compares
     published bytes against published checksums without touching the filesystem.
   - A green no-mistakes step describes the head it ran against, not the head you
     merge. After a gate finishes, re-run `cargo clippy --locked --all-targets` and
     both test suites on the final head before merging. This has now caught real
     defects twice (2 clippy warnings in `src/platform/input.rs` after the CI fixer
     rewrote the file; a test consolidation that silently removed an unreachable
     base URL and would have made a real network call).
   - When consolidating test fixtures onto a shared constructor, re-check that each
     test's deliberately extreme value (an unreachable URL, a hostile key) is still
     set explicitly. Consolidation removed one and the test kept passing while
     becoming non-hermetic - a green test is not evidence that it still fails the
     way it was written to.
   - **A stale cargo target dir can report a test count that does not exist in
     the source.** A gate auto-fix round added tests, the pipeline rebased the
     branch, and `cargo test` then ran the old binary: 141 "passing" tests whose
     names `grep -rn` could not find anywhere in `src/`. Detected by comparing
     against a pristine `git worktree` of the same commit (136). After any
     pipeline auto-fix round, `touch src/*.rs src/platform/*.rs` before trusting
     a count, or build in a fresh worktree. A count that disagrees with
     `grep -c '#\[test\]'` is the tell.
   - **A gate can validate a head that is not the head you merge.** The run's
     `head` stayed at the pre-rebase commit while follow-up commits landed on
     top, so the pipeline's own steps never ran on the final commit. Confirm the
     passing CI run's `head_sha` equals `git rev-parse HEAD` before merging
     (`gh api repos/:owner/:repo/actions/runs/<id> --jq .head_sha`), and verify
     the final head yourself.
