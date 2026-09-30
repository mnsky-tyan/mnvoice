# Project agent memory (mnvoice)

- Dictation vocabulary: keep `C:\Users\tyanw\bin\keywords.txt` (and this
  repo's `keywords.txt.example`) updated with the captain's common,
  idiosyncratic, or hard-to-pronounce terms (project names, tool names,
  jargon like `AGENTS.md`). Whenever a new term is introduced or encountered
  that speech recognition might misrecognize, record it there.

## Driving the phone over USB (adb)

- **Tunnel**: `adb devices` must show the device as `device` (authorized),
  then `adb reverse tcp:<port> tcp:<port>` makes the phone's
  `http://localhost:<port>` reach any WSL listener. Mirrored WSL networking
  shares `127.0.0.1`, so no portproxy or LAN IP is involved. The tunnel drops
  on USB replug - re-run the reverse command.
- **Tapping**: `uiautomator dump` gives EXACT element bounds for
  `adb shell input tap`. Screenshot-based coordinate estimates are wrong by
  100px+ because of Chrome's toolbar/WebView insets.
- **Text**: `adb shell input text` APPENDS to a focused field - it never
  clears it. The keyevent route (focus, `keyevent 122` MOVE_END, repeated
  `keyevent 67` DEL) is unreliable; safer to clear by reloading the page.
- Android BACK (`keyevent 4`) can blank a hash-routed SPA.
- **Verify** what the phone actually rendered: `adb exec-out screencap -p >
  f.png`.

## Building and verifying (from this machine)

- The only build/test command that works here, because cargo must run on the
  Windows side while the shell is WSL:

  WSLENV=CARGO_TARGET_DIR CARGO_TARGET_DIR=C:/Users/tyanw/AppData/Local/Temp/mnvoice-gate-target
  /mnt/c/Users/tyanw/.cargo/bin/cargo.exe test

  The target directory is redirected to real Windows disk on purpose: cargo
  incremental dies on the 9p mount with "could not create session directory
  lock file". The repo's own `target/` is not where builds land.
- Linux and macOS cannot be compiled or driven from this machine at all - no
  Linux cargo, no Apple SDK. Their verification is CI only: the release
  workflow builds and tests all three on pull_request.
- `update.rs`'s redirect test is the contract that makes asset downloads work
  (GitHub serves releases from a CDN after a 302). The Unix backends pin the
  same contract with their own loopback server through the same trait.
- Release tags are per platform: `vX.Y.Z-win`, `vX.Y.Z-linux`, `vX.Y.Z-macos`
  - one release each. A bare `vX.Y.Z` tag from before the split still updates
  Windows. The updater resolves its own platform's entry from the feed; a
  Windows install must never derive a URL from another platform's tag.

## Gating this repository through no-mistakes

- The pipeline agent model lives in `~/.no-mistakes/config.yaml`
  (`agent_config.pi.model`), currently `stepfun-intl-2/step-5-preview`; a run
  can pin it explicitly with `axi run --model <provider/model>`. Verify a new
  model with a one-line `pi --model <id> -p ...` before starting a run, because
  a rejected pin wastes a whole review round.
- Gating work that is already merged: reset origin/main to the pre-change
  commit, keep the old tip on a backup branch, cut the run branch at the old
  tip, run the pipeline, merge the PR. Tags and releases never move. A pipeline
  with an empty diff against its base skips its own steps, so the branch must
  actually contain the history.
- The test step drives the Linux product for real (podman container, Xvfb,
  mock provider, X key monitor), so it takes about 1.5 h; budget `--wait`
  cycles or status polling accordingly. A Windows-GUI scenario is honestly
  `untested` - approving the step with `--reason` is the correct outcome, and
  the analyzer rejects `pass` with `live=false`.
- Build scripts (`build.rs`) are never unit-tested by `cargo test`; put
  build-time rules in the crate where tests can reach them, or point the spec
  at the tests that do exercise the same rule.
- The document step also runs lint housekeeping (`auto_fix.lint: 3`) on the
  files the change touches, so the pushed head can differ from the reviewed
  head: CI runs on the pushed head, local `cargo test` should be re-run after
  the run finishes.
