# mnvoice

Ultra-low-latency, hands-free push-to-talk dictation for Windows.  
Press a hotkey, speak naturally - words type directly into whatever window you are using, in real time, with zero clipboard interference.

Built in native Rust using pure Win32, WASAPI, and WinHTTP. No Electron, no Python, no async runtimes.

```
mnvoice.exe (~417 KB)
  ├── Global hotkey (configurable, default Alt+Space to start / stop)
  ├── Standby pre-initialized audio capture (WASAPI, 16 kHz mono, ~15 ms to first audio)
  ├── Dual-layer VAD (local RMS energy + server endpointing, auto-stops on silence)
  ├── Provider-agnostic STT (real-time WebSocket streaming or standard REST)
  ├── Monotonic live word-by-word typing (SendInput KEYEVENTF_UNICODE, zero clipboard touch)
  ├── Custom vocabulary / keyterm prompting (keywords.txt or KEYWORDS= env)
  ├── Filler-word stripping (provider-native on streaming, local filter on REST)
  └── System tray control (start with Windows, check for updates, config, keywords, restart)
```

## Features

- **~15 ms activation** - persistent standby WASAPI engine pre-initializes at launch; Alt+Space starts capture in ~4 ms, first audio in ~15 ms.
- **Zero clipboard pollution** - words are injected directly at the cursor via `SendInput` with `KEYEVENTF_UNICODE`.
- **Real-time word streaming** - 40 ms audio slices over native WinHTTP WebSockets; words appear as you speak.
- **Hands-free auto-stop** - dual VAD (local RMS + server endpointing) detects ~2.2 s of silence and finalizes automatically.
- **Fully configurable** - hotkey, cancel key, orb color, fluid level, filler words, STT provider, model, language, and vocabulary. All in one plain text file, none of it required.
- **No windows, no taskbar** - lives in the system tray. Right-click for start-with-Windows, check for updates, config, keywords, and restart.
- **Privacy focused** - zero audio written to disk, point-to-point TLS, zero telemetry. See [SECURITY.md](SECURITY.md).
- **Tiny footprint** - ~417 KB binary, ~14 MB working set, ~2.4 MB private RAM, 0% idle CPU.

---

## Quick Start

### 1. Download

Grab the latest `mnvoice-windows-x64.zip` from the [Releases](../../releases/latest) page.  
Extract it - you get three files:

```
mnvoice.exe
mnvoice.env.example
keywords.txt.example
```

### 2. Configure

Rename `mnvoice.env.example` to `mnvoice.env` (keep it next to `mnvoice.exe`) and fill in your API key:

```ini
PROTOCOL=streaming
API_KEY=your_api_key_here
MODEL=nova-3
LANGUAGE=en
```

Get a free API key from [Deepgram](https://console.deepgram.com/) (streaming, ~$200 free credit, recommended) or [Groq](https://console.groq.com/) (REST, free tier). See the Configuration Reference below for every key.

Everything else is optional - delete `mnvoice.env` entirely and mnvoice runs on its defaults, which strip filler words.

### 3. Run

Double-click `mnvoice.exe`, or from a terminal:

```cmd
mnvoice.exe
```

There is no window and no taskbar entry. A small icon appears in the **system tray** (bottom-right, near the clock) and stays there in the background.

Press **Alt+Space** in any app to start dictating. A pink orb appears at the bottom of your screen while you speak, then text is typed straight into your focused window.

### What you get

```
system tray icon  - right-click for autostart, check for updates, config, keywords, restart, exit
orb               - appears only while recording, then disappears
no windows        - nothing in the taskbar, nothing to close
no terminal       - no command needed after install
```

---

## Start with Windows

One click, no commands. Right-click the tray icon and check **Start with Windows**:

- **Checked** - mnvoice is added to the current user's startup list
- **Unchecked** - it is removed again

The entry is visible and reversible in Windows Task Manager under **Startup apps**, and it needs no admin rights.

If you prefer a terminal, the same change is one command (swap in your own path):

```cmd
reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v mnvoice /t REG_SZ /d "C:\Users\you\mnvoice.exe" /f
```

To remove it:

```cmd
reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v mnvoice /f
```

---

## The tray menu

```
system tray icon
    -> Start with Windows   (checkbox)
    -> Check for updates    (downloads a newer release, installs it when idle)
    -> Open config          (opens mnvoice.env in Notepad)
    -> Open keywords        (opens keywords.txt in Notepad)
    -> Restart              (frees the hotkey and starts fresh)
    -> Stop & transcribe
    -> Exit
```

---

## Command line

One flag is yours to use, and it is for recovery, not normal use:

| Command | Purpose |
|---|---|
| `mnvoice.exe` | Run normally |
| `mnvoice.exe --restart` | Kill stale instances, free the hotkey, start fresh |
| `mnvoice.exe --finish-update <dir>` | Internal recovery flag the updater itself uses: a short-lived helper copy finishes an interrupted swap. Never run it by hand. |

`--restart` is what to reach for if **Alt+Space silently stops working** - almost always another app (Gemini, PowerToys, AutoHotkey) has grabbed the same hotkey and left it held. Starting with `--restart` lets mnvoice claim it again.

---

### Start it with your shell (advanced)

Most users want the checkbox above. These are for people who also want mnvoice to launch only when a shell is open:

PowerShell profile (`$PROFILE`):

```powershell
if (-not (Get-Process mnvoice -ErrorAction SilentlyContinue)) {
    Start-Process -WindowStyle Hidden "C:\path\to\mnvoice.exe"
}
```

WSL `~/.bashrc` or `~/.zshrc`:

```bash
if ! powershell.exe -NoProfile -Command \
    "if (Get-Process mnvoice -EA SilentlyContinue) { exit 0 } else { exit 1 }" \
    > /dev/null 2>&1; then
    powershell.exe -NoProfile -WindowStyle Hidden \
        -Command "Start-Process 'C:\path\to\mnvoice.exe'" > /dev/null 2>&1 &
fi
```

Task Scheduler (works even if Explorer is restarting):

```cmd
schtasks /create /tn "mnvoice" /tr "C:\path\to\mnvoice.exe" /sc onlogon /rl limited /f
```

---

## Customization

All settings go in `mnvoice.env` next to the executable. Full reference below.

### Keybindings

```ini
# Start / stop dictation
HOTKEY=Alt+Space

# Cancel recording and discard transcript
CANCEL_KEY=Escape
```

Supported modifiers: `Alt`, `Ctrl`, `Shift`, `Win`  
Supported keys: `Space`, `Escape`, `Tab`, `Enter`, `F1`-`F24`, `A`-`Z`, `0`-`9`, `Home`, `End`, `PageUp`, `PageDown`, `Insert`, `Delete`, `BackQuote`

Examples:
```ini
HOTKEY=Ctrl+Shift+D
HOTKEY=F9
HOTKEY=Win+Space
CANCEL_KEY=none
```

### Orb Color

```ini
# Named preset
ORB_COLOR=hot_pink

# Any hex color
ORB_COLOR=#A855F7
```

Built-in presets: `hot_pink` (default), `cyan`, `purple`, `blue`, `emerald`, `amber`, `red`, `white`

### Fluid Level

Controls how dense / full the fluid inside the orb appears. `0.0` = wispy mist, `1.0` = fully filled.

```ini
ORB_FLUID_LEVEL=0.75   # or 75%
```

### Speech Provider

```ini
# Real-time WebSocket streaming (default, lowest latency)
PROTOCOL=streaming
API_KEY=your_deepgram_key
MODEL=nova-3
LANGUAGE=en

# Standard REST (OpenAI-compatible: Groq, OpenAI, self-hosted Whisper, vLLM)
PROTOCOL=rest
API_KEY=your_groq_key
MODEL=whisper-large-v3-turbo
BASE_URL=https://api.groq.com
```

Custom endpoint (self-hosted or corporate proxy):
```ini
BASE_URL=wss://stt.internal.company.com:8443/listen
```

### Custom Vocabulary

Create `keywords.txt` beside `mnvoice.exe` (auto-loaded), or use the env var:

```ini
KEYWORDS=Kubernetes, TypeScript, PostgreSQL, herdr, mnvoice
```

One word per line or comma-separated. Lines starting with `#` are comments.

### All options

Every key below works with any provider and any API key unless marked otherwise.

| Variable | Default | Description |
|---|---|---|
| `PROTOCOL` | `streaming` | `streaming` (WebSocket, real-time) or `rest` (OpenAI-compatible batch) |
| `API_KEY` | - | API key / auth token |
| `MODEL` | `nova-3` / `whisper-large-v3-turbo` | Model identifier (default depends on `PROTOCOL`) |
| `BASE_URL` | provider default | Custom endpoint, port, or reverse proxy |
| `LANGUAGE` | `en` | Language code, or `auto` for detection |
| `HOTKEY` | `Alt+Space` | Trigger hotkey. See Keybindings above for syntax. |
| `CANCEL_KEY` | `Escape` | Discard the recording mid-speech (`none` to disable) |
| `FILLER_WORDS` | `0` | `0` strips "uh"/"um"/"erm", `1` keeps them verbatim |
| `KEYWORDS` | - | Comma-separated vocabulary hints. `keywords.txt` beside the exe is auto-loaded too. |
| `AUTO_UPDATE` | off | `1` installs a newer published release automatically. Off by default; see [Updates](#updates). |
| `VAD_SILENCE_MS` | `3000` | Silence after speech that stops recording. Raise if it cuts you off. |
| `VAD_RMS_THRESHOLD` | `400` | Mic energy counted as speech, `0`-`32767`. Raise if noise keeps it listening. |
| `MAX_SECONDS` | `120` | Hard recording limit before forced stop |
| `TRAILING_SPACE` | `1` | Appends a space after each dictation (`0` to disable) |
| `ORB_COLOR` | `hot_pink` | Orb fluid color: preset name or `#RRGGBB` |
| `ORB_FLUID_LEVEL` | `0.75` | Orb fill `0.0`-`1.0`, or `0%`-`100%` |

Delete `mnvoice.env` at any time to fall back to every default above.

#### How filler-word stripping works

`FILLER_WORDS=0` is one setting that adapts to the provider, because not every API offers a switch for it:

| Path | Method |
|---|---|
| Streaming with a provider that supports it (Deepgram) | asks the provider via `filler_words=false` - model-native, best quality |
| REST (Groq, OpenAI, self-hosted Whisper) | no such parameter exists anywhere, so mnvoice filters the returned text locally |

Deepgram is the **recommended** provider rather than a requirement - the REST path works fully, just with the small quality difference above.

Provider-named aliases (`DEEPGRAM_API_KEY`, `GROQ_API_KEY`, `OPENAI_API_KEY`, `DEEPGRAM_MODEL`, and so on) are also accepted for convenience.

---

## Updates

mnvoice can update itself. There is no installer and no package manager, so an
update means: download the new `mnvoice.exe`, move the old one aside, put the new
one in its place, relaunch.

**Automatic.** Set `AUTO_UPDATE=1` in `mnvoice.env`. mnvoice then checks
the release feed at most once per day, and only installs when it is idle - it will
never swap the binary out from under a transcript in flight.

It is off by default on purpose. Replacing a running binary is a decision you
should make, not one that happens silently because a default pointed that way.
Absence, an empty value, or a typo all mean off, so a misspelling cannot quietly
switch it on.

The check reads GitHub's `releases.atom` feed rather than the REST API. That
matters: the unauthenticated API allows 60 requests per hour **per source IP**,
so on a shared or NAT'd address the budget can already be spent by unrelated
traffic and every check would fail with `HTTP 403`. The feed is served from the
web endpoint, so it has no per-IP quota, needs no token, and is a small
machine-readable document instead of a 200 KB page.

**Manual.** Tray menu > `Check for updates`. Same result, whenever you ask.

**Your settings survive.** `mnvoice.env` and `keywords.txt` sit beside the exe as
separate files and are never touched by an update, so your key, vocabulary and
preferences carry across every version.

### Verifying a download

Every release publishes three files:

| File | For |
|---|---|
| `mnvoice.exe` | Direct download, and what the updater fetches |
| `mnvoice-windows-x64.zip` | `mnvoice.exe` + `mnvoice.env.example` + `keywords.txt.example` |
| `SHA256SUMS` | The SHA-256 hash of each of the two files above |

Windows will show a SmartScreen prompt on the first run of any newly downloaded
copy. That is a reputation check on an unsigned binary, not a virus detection -
nothing has ever been flagged in mnvoice.

Publishing the hashes means you do not have to take the download on trust: hash
the file you got and check it against its line in `SHA256SUMS` from the same
release.

```powershell
Get-FileHash ~\Downloads\mnvoice.exe -Algorithm SHA256
Get-Content ~\Downloads\SHA256SUMS
```

The hash of `mnvoice.exe` must match the `mnvoice.exe` line in `SHA256SUMS`
(PowerShell prints upper case, the file lower case). The updater downloads that
same release asset, so a copy it installed passes the same check.

## Building from Source

Requires Rust stable with the `x86_64-pc-windows-msvc` target:

```cmd
git clone https://github.com/mnsky-tyan/mnvoice.git
cd mnvoice
cargo build --release
```

Binary at `target\release\mnvoice.exe`. Run tests:

```cmd
cargo test
```

---

## Privacy & Security

See [SECURITY.md](SECURITY.md) for full details:

- Zero audio written to disk - buffers live in RAM only and are dropped immediately after transmission.
- Point-to-point TLS (WinHTTP `WINHTTP_FLAG_SECURE`) directly to your configured endpoint. Zero third-party calls.
- Zero clipboard reads or writes - dictation uses `SendInput` with `KEYEVENTF_UNICODE` only.
- No global keyboard hooks - only the two registered hotkeys (`RegisterHotKey`) are intercepted.

---

## License

MIT
