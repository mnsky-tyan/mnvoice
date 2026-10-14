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
   - **A bound installed on one handle may never reach the one that matters.**
     The Windows WebSocket send bound was set on the session handle immediately
     after `Self::open`, but `open` calls `WinHttpOpenRequest` internally and a
     request handle inherits the session's timeouts *when it is created*, so a
     value set afterwards may never apply. Ask for it at request creation by
     widening the tuple the opener takes. More importantly, the platform may
     not honour it at all: `WinHttpWebSocketSend` ignores the send timeout
     entirely (measured - blocked past 60 s at 2 s, past 120 s at 100 ms, and
     with `WINHTTP_OPTION_SEND_TIMEOUT` also set), so the only real bound on a
     stalled peer is the Unix backend's `WRITE_TIMEOUT`. Verify a timeout
     against a peer that stops reading before claiming it protects anything,
     and keep a control run (same frames, peer reads) to prove the send path
     itself is healthy.
   - **A gate finding can be right about the fact and wrong about the history.**
     A reviewer claimed `restore_old_image` "reverses two declined decisions";
     the decisions were in fact directed fixes, one of them by the reviewer
     itself in an earlier round. Check the recorded round history before
     conceding a scope argument, and verify the behavioural consequence before
     approving either way - here, that the restore consumes `mnvoice.exe.old`,
     so `left_old_image_behind` cannot announce a failed install as a
     successful update.
   - **A refused install must leave *something* runnable.** The base helper
     deleted the staged file when the digest check refused, but that path is
     only reachable once `swap_interrupted` has proved the exe is absent - so
     the refusal left no exe at all. Restore the previous image instead. In
     general: on the failure path of a swap, ask what the user has left to run.
   - **`sync --recover` needs a clean worktree.** Uncommitted local changes
     duplicated the pipeline head and blocked recovery with
     `safety: blocked_recover_dirty`. `git stash` first, recover, then pop.

   - **Two spellings of one derived constant will disagree exactly where the
     derivation can fail.** The capture frame stride was `nBlockAlign.max(1)`
     at the sizing site and `nBlockAlign > 0 ? nBlockAlign : channels *
     sample_bytes` inside `convert_mix`; they differed only at
     `nBlockAlign == 0`, where the sizing site called 1 byte a frame and the
     decoder called it 8 for a stereo float32 format. A packet sized the old
     way failed the length check outright, or decoded one frame for every
     eight the device had captured. Both sites now call one
     `frame_bytes(format)` helper, and the helper takes its widths from the
     format rather than from arguments, so no caller can hand it a pair that
     contradicts the format it is sizing or decoding. The rule: when a value
     is derived, derive it in one function and pass the function's result, not
     the inputs, to the places that need it.
   - **A comment that names a defect class invites the reviewer to check that
     class, so it must be true at that line.** Repeated rounds of this pass
     were spent on prose claiming protection the code does not establish: a
     guard "catching" a mid-session format change (both operands come from the
     startup copy of the format, so it cannot), a "tail" that is "real, not
     theoretical" (the carry was provably empty by induction), and "neither
     clamp" one sentence after stating the channel count is not clamped. State
     what the code establishes and name what it does not, including that a
     separate change would be needed to cover the rest.
   - **Verify a reviewer's claim about the history before acting on it, and
     keep the falsification on record.** A finding re-filed three times as
     `intent-conflict-windows-carry` insisted the intent REQUIRED carrying a
     partial trailing frame on Windows; the intent's wording predated the same
     reviewer's own proof that WASAPI's `GetBuffer` reports a whole number of
     frames, which made the carry unreachable. Disposition: deliberate, with
     the reason recorded in the code at the site. A fix built on a premise the
     code contradicts is reverted, and the revert is recorded, not dropped.
   - **When WSL interop to Windows dies, every `/mnt/c/...` launch fails with
     `UtilAcceptVsock:271: accept4 failed 110` and the exit code can still be
     0.** That last part is the trap: a wrapper sees success and an empty
     string. `cargo.exe`, `cmd.exe` and `tasklist.exe` all went down together
     while `powershell.exe` intermittently worked, which is what made it look
     like a per-tool bug. It is a machine-level outage, so a gate's test step
     cannot route around it - the run fails at `test` with the branch intact.
     Diagnose with a trivial `cmd.exe /c echo` twice (one success does not
     mean recovery), treat "ERROR line plus no stdout" as failure, and hold
     Windows-bound work rather than letting an agent thrash. Recovery is the
     user's Windows-side call: restarting WSL kills every WSL process,
     including the agent session and herdr's panes.
   - **NEVER run a freshly built `mnvoice.exe` with `--restart` while the live
     instance is running.** The flag's whole job is to kill other mnvoice
     processes and free the hotkey, so it terminates the user's running app -
     and because the built binary then becomes the one that launches, the live
     session is left running an unreleased build from the target dir instead of
     `C:\Users\tyanw\bin\mnvoice.exe`. Verified 2026-10-14: a probe of a temp
     release build killed the live instance (PID 17272) and replaced it with the
     temp binary; recovery was stopping that PID and relaunching the installed
     exe. To smoke-test a build, start it from a directory with no env file and
     no other instance expected, or test the behaviour through `cargo test` -
     never through the flag that exists to kill things.
