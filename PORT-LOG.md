# Port log - Linux and macOS (v0.1.15)

Reverse-chronological. Every claim links to a check that was run.

## Stage: seam routing (main was routed first, before this branch)

- `update.rs` http_get, `rest.rs` transcribe, `stream.rs` handshake+read loop
  all go through `platform::http::{Transport, WebSocket}` now.
- Check: `git grep "Win32::Networking::WinHttp" -- ':!src/platform'` -> empty.
- Windows tests stayed green throughout (47 -> 48 by the end).

## Stage: Audio and Injector traits

- `platform::audio::Audio` - persistent-engine shape, not record-per-session;
  the WASAPI standby trick survives behind it on Windows.
- `platform::input::Injector` + `input::global()` - the streaming loop has no
  platform knowledge; it types through the indirection.
- Windows impls: `AudioEngine` implements Audio, `SendInputInjector` static.
- Check: cargo test on Windows -> 47 passed, 0 warnings.

## Stage: the split (commit 3061482)

- `main.rs` is now the dispatcher; the 884-line Win32 app moved verbatim to
  `windows_app.rs`; non-Windows gets `platform::cli`, a terminal-driven
  product over the same seam modules.
- build.rs: version env on all platforms, resource section Windows-only.
- Check: cargo test on Windows -> 47 passed, 0 warnings; CI round 1 failed on
  Linux/macOS exactly where expected (no transport existed yet).

## Stage: Unix backends (commits 0cd5b0c..HEAD)

- `unix_audio` (cpal): stream is `!Send`, so the whole session lives in one
  thread; realtime callback only appends to a buffer, a 20 ms ticker mixes to
  mono and resamples to 16 kHz (fractional step + linear interpolation, the
  conversion `audio.rs` applies to the device's mix format) + VAD + feeds the
  transcriber. Device-open per session is the stated cost of no cpal standby
  equivalent.
- `unix_http`: ureq + tungstenite. GET follows redirects (pinned by the same
  loopback 302 test the Windows updater has, run through the trait), status
  codes ride as data, read's timeout ignored like WinHTTP's (the loop treats
  Err as end-of-stream), every call carries a wall-clock bound the Windows
  WinHTTP backend sets and std/ureq do not (60 s REST, 45 s download, 10 s
  connect, 30 s handshake), TLS by feature flags: rustls+ring+bundled roots on
  Linux, native-tls/Security.framework on macOS. One `UnixTransport` serves
  both Unix backends: the TLS split is already decided by Cargo.toml's
  per-target features - which declare no network dependency for both targets,
  because Cargo unions the features of every matching entry - plus the setup
  each side needs (`ensure_tls_ready` installs rustls' crypto provider on
  Linux, `agent_builder` installs the native-tls connector on macOS, since
  ureq's `native-tls` feature supplies only the adapter and never the
  default), so only the audio device and the injector genuinely differ per
  platform.
- `linux_impl`/`macos_impl`: cpal audio, enigo injection (x11rb backend - no
  libxdo system dependency).
- CI iterations to green: 6 rounds, each fixing exactly what rustc on the real
  platform said (duplicate mod decls, tungstenite Bytes/Utf8Bytes, cpal
  traits in scope, non-exhaustive Message match, moved chunk, ureq unsized
  reader, header lifetimes, enigo feature name).
- Check: `gh pr checks 2` -> build pass, linux pass, macos pass (run
  36350984195). Linux runs the redirect test; macOS 3m22s total.

## Deferred, on purpose

- Wayland-native input (portal): X11/XWayland ships; a native Wayland window receives
  nothing and nothing on this path can detect that, so the limit is stated in
  the README rather than papered over.
- Orb, tray, hotkeys, single-instance, autostart, self-update on Unix: all
  Windows-native surfaces; the updater especially (running-image swap) is
  per-platform work. README documents all of it.
- Proxy support: WinHTTP resolves a system proxy (`WINHTTP_ACCESS_TYPE_DEFAULT_PROXY`),
  while the Unix transport dials the provider directly, so `HTTP_PROXY` /
  `HTTPS_PROXY` / `ALL_PROXY` are ignored on both the REST path (ureq's
  `proxy-from-env` feature is off) and the streaming path (`connect_with_timeout`
  hands a raw TCP stream to tungstenite, which has no CONNECT tunnel). A
  CONNECT tunnel for the socket would be new machinery the port does not need
  yet, so the limit is stated in the README instead.

## Release shape: three separate releases

The first gate run validated the port itself; the release shape was then
changed from "one tag, three assets" to three separate releases, one per
platform, on the captain's instruction - a single release mixing binaries is
not what a user of one platform should be handed.

- Tags `v0.1.15-win` / `v0.1.15-linux` / `v0.1.15-macos`, one release each,
  carrying only that platform's artifacts. The Windows release keeps exactly
  the file set v0.1.14 shipped (exe + zip + SHA256SUMS).
- The atom feed lists up to three releases per version now, so "newest entry"
  no longer identifies this platform's release. `feed_tags` +
  `newest_tag_for_this_platform` resolve the newest entry of this platform:
  the `win` suffix, or a bare tag from before the split (Windows only, since
  every pre-split release was Windows). Without that a Windows install derives
  a URL from the Linux or macOS release's tag and gets a 404.
- release.yml: each job publishes the release for the tag it was triggered by;
  a pull_request still builds and tests all three jobs, which is where the
  other platforms are verified because nothing on the captain's machine can
  compile them.

## Release: v0.1.15-win / -linux / -macos (2026-09-29)

Three releases published, one per platform, each carrying only its own
artifacts (Windows keeps the v0.1.14 file set: exe + zip + SHA256SUMS).

- CI green on the tagged commits: one job per tag, plus the full three-job
  matrix on the PR head (windows/linux/macos all success).
- Published hashes verified by downloading the asset and comparing it to
  SHA256SUMS: exe b9f138f9..., zip a7e48c89....
- The captain's install was moved from v0.1.14 to v0.1.15 by hand, because a
  pre-v0.1.15 updater derives its URL from a bare tag and there is no bare
  v0.1.15 release any more: it would 404 forever and never leave v0.1.14.
  The swap was the same rename the updater uses (running image aside as
  mnvoice.exe.old, new image takes the name); the running process keeps
  v0.1.14 until relaunch, and its next startup deletes the .old. From this
  version the updater resolves its own platform's release from the feed, so
  the hop happens exactly once per machine.
