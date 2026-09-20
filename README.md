# mnvoice

Push-to-talk dictation for Windows. Press **Alt+Space** to start recording,
**Esc** to stop; the transcript is pasted into whatever window has focus.

Transcription runs on Groq's servers (`whisper-large-v3-turbo`, an
OpenAI-compatible endpoint). Nothing is loaded locally - no model weights, no
inference runtime - so the app stays at a few MB of RAM.

```
tiny.exe
  ├── global push-to-talk hotkey (Alt+Space)
  ├── microphone capture (WASAPI, 16 kHz mono)
  ├── Groq API call (WinHTTP, native TLS)
  └── paste returned text (clipboard + Ctrl+V)
```

## Setup

1. Build (needs the MSVC Rust toolchain and VS 2022 build tools):

   ```
   cargo build --release
   ```

   The binary is `target/release/mnvoice.exe` (~275 KB, no runtime
   dependencies).

2. Create `mnvoice.env` next to the exe (or set the variables in the
   environment):

   ```
   GROQ_API_KEY=gsk_your_key_here
   ```

   Get a key at https://console.groq.com/keys. The free tier covers
   whisper-large-v3-turbo with generous limits (2,000 requests/day,
   8 hours of audio/day).

3. Run `mnvoice.exe`. It lives in the system tray:
   - **Alt+Space** - start recording (tray tooltip shows "listening")
   - **Esc** - stop, transcribe, paste at the cursor
   - **Alt+Space** again while recording - same as Esc
   - Left-click the tray icon - status, right-click - menu (Stop &
     transcribe / Exit)

   Optional: run with `--install-startup` to launch on login
   (`--uninstall-startup` removes it again).

## Configuration

`mnvoice.env` beside the exe, or environment variables (env wins):

| Variable             | Default                    | Meaning                          |
| -------------------- | -------------------------- | -------------------------------- |
| `GROQ_API_KEY`       | -                          | required                          |
| `GROQ_MODEL`         | `whisper-large-v3-turbo`   | Groq model id                     |
| `GROQ_LANGUAGE`      | `en`                       | language hint sent to the API     |
| `GROQ_BASE_URL`      | `https://api.groq.com`     | override for testing/proxies      |
| `GROQ_MAX_SECONDS`   | `120`                      | auto-stop after this much audio   |
| `GROQ_TRAILING_SPACE`| `1`                        | append a space after the paste    |

## Notes

- Audio is sent to Groq; it is not processed locally. Recordings are 16 kHz
  mono 16-bit WAV (about 32 KB per 10 seconds).
- Measured footprint: ~11 MB working set / ~2 MB private at idle.
- Logs (events only, no audio): `%TEMP%\mnvoice.log`.
- The paste replaces the clipboard contents. It uses Ctrl+V, so it works in
  any normal editor, browser, or terminal.
- Windows blocks global hotkeys and synthetic input into *elevated* windows
  when the app runs unelevated - run mnvoice as administrator if you need to
  dictate into admin apps.

## Troubleshooting

- **"microphone is muted in Windows"** - unmute the input device in
  Settings > System > Sound (or the laptop's mic-mute key).
- **"microphone captured only silence"** - the right endpoint may not be
  selected, or Windows privacy settings block microphone access for desktop
  apps.
- **"GROQ_API_KEY is not set"** - create `mnvoice.env` beside the exe.
- **Nothing pastes** - make sure a text field has focus when the recording
  stops; the text goes to the focused window at that moment.
