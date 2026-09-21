# mnvoice

Ultra-low-latency, hands-free push-to-talk dictation utility for Windows.
Streams audio over WebSockets in real time and directly types text into the focused window with zero clipboard interference.

Built from scratch in native Rust using pure Win32, WASAPI, and WinHTTP. Zero Electron, zero Python, zero async runtimes.

```
mnvoice.exe (~302 KB)
  ├── Global hotkey (Alt+Space to start, Esc/Alt+Space to stop)
  ├── Standby pre-initialized audio capture (WASAPI, 16 kHz mono, ~15ms to first audio byte)
  ├── Dual-layer VAD (local RMS energy + server endpointing for hands-free auto-stop on silence)
  ├── Provider-agnostic STT (Real-time WebSocket streaming or standard REST audio/transcriptions)
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
- **Privacy & Security Focused**: Zero audio saved to disk. Point-to-point TLS encryption directly to your chosen endpoint. Zero telemetry or third-party tracking.
- **Minimal Resource Footprint**:
  - Binary size: **~302 KB**
  - Working set RAM: **~10.9 MB**
  - Private memory: **~1.9 MB**
  - Idle CPU: **0.0%**

## Supported Speech Providers

`mnvoice` is not hardwired to any single vendor. It supports two generic protocols:

1. **Real-Time WebSocket Streaming (`PROTOCOL=streaming`)** *(Recommended)*:
   - Lowest latency (~150-200ms). Streams live linear16 audio chunks.
   - Example providers: [Deepgram](https://deepgram.com/) (`model=nova-3` or `nova-2`), or any compatible WebSocket STT server.
2. **OpenAI-Compatible REST (`PROTOCOL=rest`)**:
   - Posts audio WAV files to any `/v1/audio/transcriptions` multipart endpoint.
   - Example providers: [Groq](https://console.groq.com/) (`whisper-large-v3-turbo`), [OpenAI](https://platform.openai.com/) (`whisper-1`), or self-hosted Whisper / vLLM / Ollama servers.

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
PROTOCOL=streaming
API_KEY=your_api_key_here
MODEL=nova-3
LANGUAGE=en
```

### 3. Custom Vocabulary (Optional)

Create a `keywords.txt` file next to `mnvoice.exe` and list your technical terms, project names, or rare names (one per line or comma-separated):

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
| `PROTOCOL` | `streaming` | Protocol mode: `streaming` (WebSocket) or `rest` (HTTP) |
| `API_KEY` | - | Authentication key / token for your speech provider |
| `MODEL` | `nova-3` (streaming) / `whisper-large-v3-turbo` (rest) | Speech model identifier |
| `BASE_URL` | `https://api.deepgram.com` (streaming) / `https://api.groq.com` (rest) | Custom endpoint URL / host / reverse proxy |
| `LANGUAGE` | `en` | Language code (or `auto` for detection) |
| `KEYWORDS` | - | Comma-separated custom keywords / keyterms |
| `TRAILING_SPACE` | `1` | Automatically append a space after transcription |
| `MAX_SECONDS` | `120` | Maximum recording limit before automatic cutoff |

*(Note: Provider-specific aliases such as `DEEPGRAM_API_KEY`, `GROQ_API_KEY`, and `OPENAI_API_KEY` are also automatically recognized for convenience.)*

## Privacy & Security

See [SECURITY.md](SECURITY.md) for full details:
- **Zero local audio retention**: Audio is processed purely in volatile RAM buffers and never written to disk.
- **Direct encrypted TLS connections**: All audio and keystroke streams communicate directly with your configured endpoint over native Windows TLS.
- **Zero clipboard modifications**: Dictation is typed directly via native Unicode keystrokes (`KEYEVENTF_UNICODE`), never reading or clearing your clipboard.
- **Zero telemetry**: No telemetry, analytics, or third-party phone-home calls.

## License

MIT License.
