# Security & Privacy Policy

`mnvoice` is designed from the ground up to respect user privacy, minimize system footprint, and provide full transparency over audio transmission and credential handling.

## Audio Privacy & Data Transmission

1. **No Local Audio Storage**:
   - Recorded audio - WASAPI on Windows, ALSA on Linux, CoreAudio on macOS - is held strictly in volatile system memory (`RAM`) buffers while recording.
   - Zero audio data or recordings are ever written to local disk, temporary files, or cache directories.
   - Audio buffers are immediately dropped and deallocated when transmission finishes or when recording is cancelled.

2. **Direct Point-to-Point TLS Encryption**:
   - Audio packets are streamed exclusively to the speech recognition endpoint explicitly specified in `mnvoice.env` (`BASE_URL`) or via environment variables.
   - All network connections enforce TLS 1.2 / TLS 1.3: native Windows HTTP (`WinHTTP` with `WINHTTP_FLAG_SECURE`) on Windows, `rustls` with bundled roots on Linux, `Security.framework` on macOS.
   - mnvoice does not communicate with any telemetry servers, third-party analytics, crash reporting daemons, or auxiliary endpoints.

3. **Update Traffic** (Windows, `AUTO_UPDATE` or a manual "Check for updates"):
   - The only other destination is GitHub itself: the public `releases.atom` feed names the newest release, the downloaded asset and its published `SHA256SUMS` file come from that release's own assets. Release bytes are verified against the published checksum before they are swapped in, and nothing but that check ever writes to disk.

4. **Log Files & Diagnostics**:
   - Diagnostic logging (`%TEMP%\mnvoice.log`, Windows tray build) records only application lifecycle events (start, stop, errors) and the final transcribed text snippet. The Linux and macOS binaries print the same lifecycle lines to the terminal and write no log file.
   - Raw audio samples, audio waveforms, API keys, and authorization tokens are **never** logged to disk.

## Credential & API Key Management

1. **Local-Only Storage**:
   - API keys and tokens are loaded strictly from your local `mnvoice.env` file (placed beside the executable) or environment variables.
   - Credentials are held only in process memory for the duration of the network request.
   - No credentials are ever sent to any destination other than the `Authorization` header of the configured speech-to-text service.

2. **Version Control Safety**:
   - The repository's `.gitignore` explicitly excludes all `.env` files (`*.env`, which covers `mnvoice.env`).
   - Only example templates (`mnvoice.env.example`, `keywords.txt.example`) are tracked in source control.

## Keystroke Synthesis & System Clipboard

1. **Zero Clipboard Overwrites**:
   - Text is injected directly at the active cursor position as synthesized keystrokes: `SendInput` with `KEYEVENTF_UNICODE` on Windows, XTest on X11/XWayland, CoreGraphics events on macOS.
   - mnvoice **never** inspects, reads, clears, or clobbers your system clipboard history.

2. **No Global Keyboard Snooping**:
   - mnvoice does not install low-level keyboard hooks (`WH_KEYBOARD_LL`).
   - On Windows it registers only the specific global activation shortcut (`Alt+Space` and `Esc`) via the official `RegisterHotKey` API; the Linux and macOS builds register no hotkey at all.
   - The application cannot see or log any other keys typed by the user.

## Reporting a Security Concern

If you discover a potential vulnerability or security issue, please open an issue on the GitHub repository or submit a private security advisory via GitHub.
