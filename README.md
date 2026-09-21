# mnvoice

Ultra-low-latency, hands-free push-to-talk dictation for Windows.  
Press a hotkey, speak naturally - words type directly into whatever window you are using, in real time, with zero clipboard interference.

Built in native Rust using pure Win32, WASAPI, and WinHTTP. No Electron, no Python, no async runtimes.

```
mnvoice.exe (~302 KB)
  ├── Global hotkey (configurable, default Alt+Space to start / stop)
  ├── Standby pre-initialized audio capture (WASAPI, 16 kHz mono, ~15 ms to first audio)
  ├── Dual-layer VAD (local RMS energy + server endpointing, auto-stops on silence)
  ├── Provider-agnostic STT (real-time WebSocket streaming or standard REST)
  ├── Monotonic live word-by-word typing (SendInput KEYEVENTF_UNICODE, zero clipboard touch)
  ├── Custom vocabulary / keyterm prompting (keywords.txt or KEYWORDS= env)
  └── Procedural glass fluid orb indicator (color + fluid level configurable)
```

## Features

- **~15 ms activation** - persistent standby WASAPI engine pre-initializes at launch; Alt+Space starts capture in ~4 ms, first audio in ~15 ms.
- **Zero clipboard pollution** - words are injected directly at the cursor via `SendInput` with `KEYEVENTF_UNICODE`.
- **Real-time word streaming** - 40 ms audio slices over native WinHTTP WebSockets; words appear as you speak.
- **Hands-free auto-stop** - dual VAD (local RMS + server endpointing) detects ~2.2 s of silence and finalizes automatically.
- **Fully configurable** - hotkey, cancel key, orb color, fluid level, STT provider, model, language, and vocabulary all set in one plain text file.
- **Privacy focused** - zero audio written to disk, point-to-point TLS, zero telemetry. See [SECURITY.md](SECURITY.md).
- **Tiny footprint** - ~302 KB binary, ~14 MB working set, ~2.4 MB private RAM, 0% idle CPU.

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

Get a free API key from [Deepgram](https://console.deepgram.com/) (streaming, ~$200 free credit) or [Groq](https://console.groq.com/) (REST, free tier).

### 3. Run

Double-click `mnvoice.exe`, or from a terminal:

```cmd
mnvoice.exe
```

A small icon appears in the system tray. Press **Alt+Space** to start dictating into whatever window is focused.

---

## Running Automatically at Shell / System Startup

mnvoice is designed to run silently in the background. You never interact with it directly - just use your hotkey.

### Option A - Windows Startup folder (recommended for most users)

Run once from a terminal beside `mnvoice.exe`:

```cmd
mnvoice.exe --install-startup
```

This creates a shortcut in your Windows Startup folder. mnvoice launches automatically (hidden, no window) every time you log in. To remove it:

```cmd
mnvoice.exe --uninstall-startup
```

### Option B - PowerShell profile (starts with every PowerShell / Windows Terminal session)

Add to your PowerShell profile (`$PROFILE`):

```powershell
# Start mnvoice in the background if it is not already running
if (-not (Get-Process mnvoice -ErrorAction SilentlyContinue)) {
    Start-Process -WindowStyle Hidden "C:\path\to\mnvoice.exe"
}
```

Replace `C:\path\to\mnvoice.exe` with the actual path. The `-WindowStyle Hidden` flag keeps it completely invisible.

### Option C - WSL / bash profile (starts with every WSL shell)

Add to `~/.bashrc` or `~/.zshrc`:

```bash
# Start mnvoice on Windows side if not already running
if ! powershell.exe -NoProfile -Command \
    "if (Get-Process mnvoice -EA SilentlyContinue) { exit 0 } else { exit 1 }" \
    > /dev/null 2>&1; then
    powershell.exe -NoProfile -WindowStyle Hidden \
        -Command "Start-Process 'C:\path\to\mnvoice.exe'" \
        > /dev/null 2>&1 &
fi
```

### Option D - Task Scheduler (most robust, survives session restarts)

```cmd
schtasks /create /tn "mnvoice" /tr "C:\path\to\mnvoice.exe" /sc onlogon /rl limited /f
```

This registers mnvoice to start on every login via Windows Task Scheduler with no UAC prompt.

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

### All Options

| Variable | Default | Description |
|---|---|---|
| `PROTOCOL` | `streaming` | `streaming` (WebSocket) or `rest` (HTTP) |
| `API_KEY` | - | API key / auth token |
| `MODEL` | `nova-3` / `whisper-large-v3-turbo` | STT model identifier |
| `BASE_URL` | provider default | Custom endpoint URL |
| `LANGUAGE` | `en` | Language code, or `auto` for detection |
| `HOTKEY` | `Alt+Space` | Start / stop hotkey |
| `CANCEL_KEY` | `Escape` | Cancel hotkey (`none` to disable) |
| `ORB_COLOR` | `hot_pink` | Orb fluid color (preset name or `#RRGGBB`) |
| `ORB_FLUID_LEVEL` | `0.75` | Orb fill level `0.0`-`1.0` or `0%`-`100%` |
| `KEYWORDS` | - | Comma-separated custom vocabulary |
| `TRAILING_SPACE` | `1` | Append space after each dictation (`0` to disable) |
| `MAX_SECONDS` | `120` | Max recording duration before auto-stop |

Provider-specific aliases (`DEEPGRAM_API_KEY`, `GROQ_API_KEY`, `OPENAI_API_KEY`, etc.) are also recognized for convenience.

---

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
