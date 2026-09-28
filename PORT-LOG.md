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
