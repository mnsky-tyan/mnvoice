# mnvoice - Ultra-low-latency Push-to-Talk Dictation

Native Rust dictation agent for Windows, Linux, and macOS.
Continuous background tray application with native audio capture, silence detection,
and direct keyboard event injection.

## Critical Test & Running Instance Rules

1. **NEVER KILL THE RUNNING `mnvoice.exe` PROCESS**:
   - The user runs a live `mnvoice.exe` instance continuously on this machine (the user's installed `mnvoice.exe`).
   - It holds a single-instance named mutex (`mnvoice-single-instance`, session-local; see `MUTEX_NAME` in `src/windows_app.rs`).
   - Automated tests and test-step agents MUST NEVER kill, terminate, or replace any running `mnvoice.exe` process.
   - Do NOT touch or overwrite the user's installed `mnvoice.exe` during tests or reviews (only during explicit release/update deployment).

2. **Toolchain & Testing Scope**:
   - The authoritative test suite is TWO, not one, and they answer different
     questions. Locally `~/.cargo/bin/cargo test --locked` builds and tests the
     whole crate natively (151 tests, ~6 s), is what an agent must run before
     every commit, and is the gate's own test command (`.no-mistakes.yaml`
     `commands.test`). CI (`release.yml`, dispatched on `main` and on every PR,
     building windows/linux/macos on real runners) is the gate for a change and
     must be green for the exact head that is claimed.
   - The Windows cargo reached through the WSL mount used to be the gate's
     test command; it is dead and kept as history. The durable evidence:
     `/proc/sys/fs/binfmt_misc` contains only `python3.14` - there is no
     WSLInterop entry, so no PE binary can be exec'd from this Linux (the
     exec fails with ENOEXEC, "Exec format error", when the drive is
     mounted) - and its target dir had to sit on the Windows disk because
     WSL 9p mounts support no Windows file locking. A gate whose test
     command cannot run improvises a harness, so the command now points at
     the native suite. Run the local suite yourself, and let CI be the
     Windows proof; do not report a local Windows test.
   - Pure unit tests exercise code paths safely without acquiring the single-instance mutex or touching audio hardware.
   - Do NOT create scratch probe executables, live window harnesses, or kill background processes.
   - **A destructive probe must name a target that cannot exist.** On
     2026-10-17 a probe of taskkill's exit codes ran
     `taskkill /F /IM mnvoice.exe /FI "PID ne 999999"` against the LIVE
     instance: the image name is the product's own, and a filter that matches
     every PID still selects the running app, so PID 5700 was terminated. The
     rule is the `/IM` argument, not the `/FI` filter - use a name that has no
     process behind it (`mnvoice-probe-does-not-exist.exe`), or copy a
     harmless system binary to a private name and target that. Recovery from
     this mistake is a user action, not an agent one: the exe on disk is
     untouched, but only the user can restart their own tray app.

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
     a Windows temp directory (reachable from WSL through the mount)
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
   - **The same trap fires with a SMALL body, and on a different error string.**
     A loopback fixture that wrote a 15-byte JSON reply and returned (dropping
     the stream) passed Linux and Windows and failed macOS CI with
     `reading response body failed (Invalid argument (os error 22))`. The
     window is short, not absent: the client can still be in its read when the
     server closes. Drain until the client closes regardless of body size, and
     do not treat "the body is tiny" or "it passed locally" as evidence - the
     third OS in the matrix is where it shows.
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

   - **A `GlobalShortcuts` portal binding is grabbed by kglobalaccel for the
     process's whole life; closing the session does NOT release the grab.**
     Measured 2026-10-20 on Kubuntu/KDE: after the cancel session was closed
     the Esc grab stayed live three minutes and only went away when the
     process exited. `org.kde.KGlobalAccel.unregister(component, shortcut)`
     on the session bus clears a stale grab without a restart. The Linux
     cancel key therefore defaults to a chord (`Alt+Escape`, unbound on KDE
     by default) - a bare key bound here is dead in every other app until
     mnvoice quits. The rule is enforced at the bind site, not in the default:
     `linux_portal::cancel_binding` declines a modifier-free cancel key and
     logs what to write instead, so a copied `CANCEL_KEY=Escape` cannot
     re-steal it. A bare *toggle* is warned about and bound anyway
     (`bare_toggle_warning`): refusing cancel still leaves two ways to stop a
     recording, refusing the toggle leaves no way in at all.
     Windows is not in this class of problem - it registers cancel at the start
     of a recording and unregisters it at the stop (`windows_app::toggle`), so
     `Escape` remains the default there and the platforms' defaults are
     deliberately different (`cfg(windows)` in `config.rs`). The earlier
     arm/disarm design (bind cancel per recording, close at the stop) is
     reverted in 93d2b80; do not reintroduce it on Linux.

   - **A cancel event must be polled while the capture is live.** The portal's
     cancel signal was collected only after the transcriber returned, so
     Alt+Esc during a recording did nothing at all: measured 2026-10-20, the
     session ran on until the hotkey was pressed again or the silence window
     expired. `watch_recording` now polls the channel and raises BOTH flags,
     which is the Windows `cancel()` seam verbatim.

   - **On Wayland, `input::type_text` (XTest) reaches native windows and
     nothing else - the portal typer is the only path that works there.**
     Measured 2026-10-20: `xwininfo -root -tree` on `:0` listed only Chocobar,
     KWin, Fcitx and xsettingsd - no kitty, no Brave, both native Wayland.
     So `stream::commit_words` typing live through the global injector is
     invisible in a native Wayland app, and the end-of-session
     `type_transcript` through `WaylandTyper` is the only keystroke the user
     ever sees. That looks like a double-typing bug and is NOT one: removing
     the callback's keystrokes makes streaming type nothing at all in kitty
     and Brave. It really does double-type into an XWayland window, and the
     fix is to route the streaming loop's own commits through the portal
     typer, not to drop the final pass. Do not "fix the double typing" again
     without measuring which injector reaches the user's actual windows.

   - **A cancel must not wait out the provider's final-transcription window.**
     The streaming loop waited up to 1500 ms for Deepgram's last frame, and
     the only thing that could end that wait early was the reader thread -
     which on a cancel is parked inside `read()`, waiting for a frame a
     cancelled session never gets. So a cancel paid the full 1500 ms before
     the socket closed, and that wait sat between the user's Alt+Esc and the
     orb coming down. Measured 2026-10-21 with the negative control: 1.759 s
     before the fix, 0.25 s after. The wait now also ends on `cancelled`,
     which is set only by an explicit cancel (never by a normal stop), so the
     ordinary end still waits for the final transcript. The regression test is
     `a_cancel_does_not_wait_out_the_final_transcription_window`.

   - **A layer-shell buffer is the compositor's until it releases it.**
     The orb swapped between two shm buffers on every frame and the
     `wl_buffer.release` event was discarded, so the render wrote into
     pixels the compositor could still be reading - the "tear" the
     double-buffer comment promised to avoid. `wl_buffer.release` is now
     tracked per buffer (`Buffer::in_use`) and the render only paints into a
     buffer the compositor has handed back; when both are still out the frame
     is skipped and the last committed frame stays on screen. The pair is
     destroyed together with the surface - `Hide` and the compositor's
     `Closed` event both drop the buffers, and each `Buffer`'s `Drop` munmaps
     its mapping - and the paint loop recreates the pair lazily, one fresh
     memfd+mmap pair per dictation, the same lifecycle as the surface. Safety
     does not come from the surface destroy (`wl_surface.destroy` only
     deletes the surface and invalidates its object ID): the destroyed
     buffer's storage is never reused, which is exactly the case
     `wl_surface.attach` permits for destroying a `wl_buffer` before
     `wl_buffer.release` arrives; the buffer/pool destroy requests follow the
     surface destroy on the wire as belt-and-braces. `in_use` only tracks
     reuse within one dictation.

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
   - **The 2026-10-17 interop recovery did not last (re-measured 2026-10-21).**
     It was per-launch-shape - direct `.exe` invocation over the mount worked
     again (a `cargo.exe` under the Windows user profile, `tasklist.exe`,
     `taskkill.exe`) while `cmd.exe /c echo` still returned `Permission
     denied` - so the diagnostic above must not be read as "the whole mount is
     down": test the shape you actually need, and do not use a dead `cmd.exe`
     to declare `cargo.exe` dead. The recovery did not last:
     `/proc/sys/fs/binfmt_misc` now contains only `python3.14` - no
     WSLInterop entry - so no Windows executable runs from WSL today and CI
     is the Windows proof once more (rule 2). A local Windows test is not
     reported.
   - **NEVER run a freshly built `mnvoice.exe` with `--restart` while the live
     instance is running.** The flag's whole job is to kill other mnvoice
     processes and free the hotkey, so it terminates the user's running app -
     and because the built binary then becomes the one that launches, the live
     session is left running an unreleased build from the target dir instead of
     the user's installed `mnvoice.exe`. Verified 2026-10-14: a probe of a temp
     release build killed the live instance (PID 17272) and replaced it with the
     temp binary; recovery was stopping that PID and relaunching the installed
     exe. To smoke-test a build, start it from a directory with no env file and
     no other instance expected, or test the behaviour through `cargo test` -
     never through the flag that exists to kill things.
   - **A WebSocket message may arrive in fragments; the Windows backend must
     reassemble them.** RFC 6455 lets a sender split one message across frames
     (first frame with FIN clear and the message opcode, then continuations
     with opcode 0), and the consumer of the transport `read` parses every
     buffer as one complete JSON document. tungstenite reassembles on Unix;
     WinHTTP surfaces fragments via `peBufferType` and the loop must keep
     reading until the final fragment, mapping the close type separately.
     Verified with a live loopback peer test that discriminates (it fails with
     the pre-fix read on Windows, passes with the fix).
   - **A tick that consumed nothing must still charge the silence windows.**
     The Unix engine's empty-mono path and the Windows engine's packet==0 path
     both count nothing toward the no-speech and after-voice limits, so a
     stalled device ran until the longer window expired. Both now call
     `advance_stalled(elapsed_ms, vad_silence_ms)`, which charges both windows
     on a tick that returned no audio.

   - **`VAD_RMS_THRESHOLD=400` is below this laptop's ambient noise floor, so
     a silent room never closes the 3s window and a dictation never stops by
     itself.** Measured 2026-10-20 on the user's own capture path (48 kHz
     stereo mixed to mono, resampled to 16 kHz, 20 ms chunks, `rms_of`): 30 s
     of silence gives p50=358, p99=571, max=684, and 28% of chunks count as
     speech, so the longest quiet run was 500 ms against a 3000 ms requirement.
     The user's `~/.local/bin/mnvoice.env` now sets `VAD_RMS_THRESHOLD=800`
     (clears the ceiling with margin, still far below speech); a silent
     recording then auto-stops in ~6 s instead of never. This is a per-machine
     tuning, NOT a default change - a quiet room is fine at 400, and the
     shipped `mnvoice.env.example` must stay platform- and hardware-neutral.
     When "the orb never goes away" is reported again, measure the noise floor
     through the engine's own metric before touching the capture code.
   - **`AUDCLNT_BUFFERFLAGS_SILENT` packets hold stale buffer contents, not
     silence.** The flag means the driver did not write the buffer; reading it
     feeds a glitch into the transcript. Substituting zeros (a shared const
     slice, no per-packet allocation) makes the behaviour independent of the
     driver.
   - **The tray menu must not spawn processes.** `TrackPopupMenu` runs on the UI
     thread; spawning `schtasks`/`reg.exe` inside it blocks the menu. The
     autostart state is now cached (`AUTOSTART_CACHED`) and refreshed on the
     worker threads, and the menu only reads the cache.
   - **tungstenite's `handshake` feature is not a TLS feature.** It is the HTTP
     upgrade machinery, needed on both Unix and Windows. It is now a
     `cfg(windows)` dev-dependency so the Windows test build can compute
     `Sec-WebSocket-Accept` with `derive_accept_key` instead of ~95 lines of
     hand-rolled SHA-1/base64. The release binary is unchanged (dev-only).
   - **One icon file serves both platforms; on Linux it is baked in at build
     time.** `assets/mnvoice.ico` is the only brand asset: winres embeds it in
     the exe, and the Linux build script decodes its PNG frames into `OUT_DIR`
     so `platform::linux_icon` can serve the tray's `IconPixmap` and write the
     launcher PNG the desktop entry names with `Icon=mnvoice`. Before this, the
     Linux tray painted the orb from `orb_render` and the entry had no `Icon=`
     at all, so the brand mark appeared nowhere on a Linux desktop - the orb is
     a live status indicator, not the product icon. Pixels travel through
     `OUT_DIR` files, not `cargo:rustc-env`: a 256KB env var overflowed rustc's
     command line ("Argument list too long", measured). A checkout whose `.ico`
     is missing warns and falls back to the orb instead of failing the build.
   - **The exe/tray icon is generated artwork, not a hand-editable asset.**
     `assets/mnvoice.ico` is built from the mn-brand master
     `~/mnsky/mn-brand/icons/mnvoice.svg` with librsvg (the brand renderer,
     `mn-brand/svg_render.py`), never from a screenshot or an upscale. The two
     source SVGs next to it record exactly what the ICO contains:
     `mnvoice-icon.svg` is the master minus the full-square page background
     (transparent corners, so the card floats), and `mnvoice-mark.svg` is that
     plus the wordmark paths dropped and the remaining artwork scaled 1.15x
     about the mark's own centre. Sizes 16-48 use the mark-only file, 64-256
     the full card: at tray sizes the wordmark collapses into mush and the mark
     stroke is sub-pixel, so a responsive set is the only version that stays
     legible. 1.15x is the measured maximum before the mark clips the card edge
     (1.25x puts ink outside the rounded rect). Do not "fix" the small sizes by
     thickening or redrawing the signature - the brand forbids it.
     Regenerate with the recipe in the commit that introduced them, and verify
     a rebuild by extracting the RT_ICON PNG frames from the exe and comparing
     pixels against the source renders (`rc.exe` re-encodes, so byte equality
     against the .ico is not expected; pixel equality is).

## Repo wiki

```
mnvoice/
├── AGENTS.md              this file: test rules, release discipline, repo wiki
├── Cargo.toml             deps: windows (Win32), cpal+enigo (Unix), ashpd+wayland (Linux desktop)
├── build.rs               winres: bakes the version resource + icon into mnvoice.exe
├── src/
│   ├── main.rs            entry: Windows -> windows_app; Linux -> linux_app (or --terminal); macOS -> cli
│   ├── windows_app.rs     the Windows tray app: window, tray, hotkey, message loop (2,982 lines)
│   ├── audio.rs           WASAPI capture engine + standby pre-init (Windows)
│   ├── orb.rs             the orb's Windows surface: Win32 layered window
│   ├── orb_render.rs      the platform-neutral orb render: 48px fluid, premultiplied ARGB
│   ├── config.rs          mnvoice.env parse: providers, hotkeys, orb, aliases (single owner)
│   ├── stream.rs          the streaming transport: Deepgram WebSocket, word commits
│   ├── rest.rs            the REST transport: OpenAI-compatible batch transcription
│   ├── update.rs          the self-updater: download, sha256 verify, swap, relaunch (Windows)
│   ├── paste.rs           Windows Unicode typing pairs
│   ├── locks.rs           poison-recovery helpers
│   ├── test_hooks.rs      test-only counters for stream tests
│   └── platform/
│       ├── mod.rs         the seam boundary: tag parsing, version, per-platform modules
│       ├── audio.rs       the Audio trait
│       ├── http.rs        the Http trait (redirect contract the updater depends on)
│       ├── input.rs       the Injector trait + the KEY_GAP pacing contract
│       ├── cli.rs         the terminal control surface (Enter to toggle) + dictate_with_controls
│       ├── unix_audio.rs  cpal capture (Linux, macOS)
│       ├── unix_http.rs   ureq + tungstenite transport (Linux, macOS)
│       ├── linux_app.rs   the Linux desktop app: flock, autostart, worker-thread loop (hotkey + tray events), PHASE static, relaunch
│       ├── linux_portal.rs ashpd clients: GlobalShortcuts hotkey (with the bare-key guard), RemoteDesktop typing
│       ├── linux_tray.rs  the tray (ksni StatusNotifierItem): rows per phase -> AppEvent, icon from linux_icon (orb fallback)
│       ├── linux_icon.rs  the brand mark on Linux: ICO frames baked by build.rs, tray pixmap + launcher PNG
│       ├── linux_orb.rs   the orb's Linux surface: zwlr_layer_shell_v1 overlay, one surface per dictation, buffers destroyed with it, scope=on-screen-display (window type, not a label), buffer released before reuse
│       ├── linux_impl.rs  the X11 XTest injector (fallback without a portal grant)
│       ├── macos_impl.rs  the macOS injector (CoreGraphics)
│       └── windows_impl.rs the SendInput injector
├── .github/               release workflows per platform (win/linux/macos tags)
└── docs/                  notes from each pass
```

Entry points: `src/main.rs` (platform dispatch), `windows_app::main()` (Windows tray), `linux_app::main()` (Linux desktop), `platform::cli::run()` (terminal, all platforms).
