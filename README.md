# mnvoice

Ultra-low-latency, hands-free push-to-talk dictation utility for Windows.
Streams audio over WebSockets in real time and directly types text into the focused window with zero clipboard interference.

Built from scratch in native Rust using pure Win32, WASAPI, and WinHTTP. Zero Electron, zero Python, zero async runtimes.

```
mnvoice.exe (~302 KB)
  ├── Global hotkey (Alt+Space to start, Esc/Alt+Space to stop)
  ├── Standby pre-initialized audio capture (WASAPI, 16 kHz mono, ~15ms to first audio byte)
  ├── Dual-layer VAD (local RMS energy + server endpointing for hands-free auto-stop on silence)
  ├── Real-time streaming transcription (Deepgram Nova-3 via native WinHTTP WebSockets, Groq Whisper fallback)
  ├── Monotonic live word-by-word typing (SendInput with KEYEVENTF_UNICODE, zero clipboard history clobbering)
  ├── Custom vocabulary / keyterm prompting (keywords.txt / KEYWORDS env)
  └── Procedural glass fluid orb indicator (36px, 32-bit premultiplied ARGB layered window, click-through)
```

## Features

- **Instantaneous Activation (~15 ms)**: Uses a persistent standby audio engine that pre-initializes the WASAPI audio graph at application startup. When you press Alt+Space, hardware capture starts in ~4 ms and the first audio buffer is captured in ~15 ms with zero truncation.
- **Direct Keystroke Injection**: Transcribed words flow directly into the active window at the cursor via `SendInput` with `KEYEVENTF_UNICODE`. Your system clipboard history remains completely untouched.
- **True Real-Time Word Streaming**: Audio is streamed in 40 ms slices over native WinHTTP WebSockets. Words stream into your document in real time as you speak.
- **Hands-Free Silence Auto-Stop**: Dual-layer Voice Activity Detection (local RMS energy calculation + server endpointing) detects when you finish speaking (2.2s silence threshold) and finalizes automatically.
- **Lightweight Glass Fluid Indicator**: A 36px procedural glass orb with undulating fluid floats 2px above your taskbar during recording. Renders with pure GDI premultiplied 32-bit ARGB (`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE`), consuming only ~6 KB buffer memory.
- **Custom Vocabulary**: Easily add specialized acronyms, technical jargon, or hard-to-pronounce names via a simple `keywords.txt` file or `KEYWORDS=` environment variable.
- **Minimal Resource Footprint**:
  - Binary size: **~302 KB**
  - Working set RAM: **~10.9 MB**
  - Private memory: **~1.9 MB**
  - Idle CPU: **0.0%**

## Getting Started

### 1. Build from Source

Requires the standard Rust toolchain with the MSVC target on Windows:

```cmd
git clone https://github.com/your-username/mnvoice.git
cd mnvoice
cargo build --release
```

The optimized binary is generated at `target/release/mnvoice.exe`.

### 2. Configuration

Copy `mnvoice.env.example` to `mnvoice.env` next to `mnvoice.exe` (or set environment variables):

```ini
PROVIDER=deepgram
DEEPGRAM_API_KEY=your_deepgram_api_key_here
DEEPGRAM_MODEL=nova-3
DEEPGRAM_LANGUAGE=en
```

> **Deepgram**: Get a free API key at [console.deepgram.com](https://console.deepgram.com/) ($200 free credit, ~770 hours of audio).
>
> **Groq Fallback**: You can also use Groq Whisper (`whisper-large-v3-turbo`) by setting `PROVIDER=groq` and `GROQ_API_KEY=gsk_...`.

### 3. Custom Vocabulary (Optional)

Create a `keywords.txt` file next to `mnvoice.exe` and list your technical terms or rare names (one per line or comma-separated):

```text
# Custom terminology
mnvoice
herdr
Kubernetes
TypeScript
PostgreSQL
```

### 4. Run

Launch `mnvoice.exe`. It runs unobtrusively in the system tray:
- **Alt+Space**: Start dictation. The pink glass fluid orb appears at the bottom of the screen.
- Speak naturally. Words type into your active window in real time.
- Stop speaking for ~2 seconds, or tap **Alt+Space** / **Esc** to stop manually.
- Right-click tray icon: View status or Exit.

#### Launch on Startup (Optional)

To start automatically with Windows:
```cmd
mnvoice.exe --install-startup
```
To remove:
```cmd
mnvoice.exe --uninstall-startup
```

## Configuration Reference

Settings can be placed in `mnvoice.env` beside the executable or exported as environment variables:

| Variable | Default | Description |
|---|---|---|
| `PROVIDER` | `deepgram` | Speech provider (`deepgram` or `groq`) |
| `DEEPGRAM_API_KEY` | - | Deepgram API key |
| `DEEPGRAM_MODEL` | `nova-3` | Deepgram model (`nova-3`, `nova-2`, etc.) |
| `DEEPGRAM_LANGUAGE` | `en` | Language code (or `auto` for detection) |
| `KEYWORDS` | - | Comma-separated custom keywords / keyterms |
| `TRAILING_SPACE` | `1` | Automatically append a space after transcription |
| `MAX_SECONDS` | `120` | Maximum recording limit before automatic cutoff |
| `GROQ_API_KEY` | - | Groq API key (for Groq fallback provider) |
| `GROQ_MODEL` | `whisper-large-v3-turbo` | Groq Whisper model id |

## Technical Architecture

Unlike typical dictation utilities built on Python, Electron, or heavy web runtimes that consume 300MB - 1GB of memory and introduce hundreds of milliseconds of startup lag:

1. **Win32 Message Loop**: Event-driven native thread using `RegisterHotKey` and `Shell_NotifyIconW`.
2. **Persistent WASAPI Standby Engine**: Eliminates Windows Audio Engine kernel graph setup latency (~500ms) by keeping the audio client pre-allocated in standby, transitioning to capture in ~4ms upon trigger.
3. **Pure WinHTTP WebSockets**: Streams linear16 audio chunks over native Windows HTTP WebSocket protocol with zero third-party networking dependencies.
4. **Direct Unicode Injection**: Synthesizes inputs via Windows `SendInput` with `KEYEVENTF_UNICODE`, allowing direct typing into any application (browsers, code editors, terminal multiplexers) without modifying clipboard history.

## License

MIT License.
