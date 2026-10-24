<p align="center">
  <img src="crates/mg-server/ui/icons/multigateway.png" width="112" height="112" alt="MultiGateway logo" />
</p>

<h1 align="center">MultiGateway</h1>

<p align="center">
  One gateway for every AI account pool.
</p>

<p align="center">
  <a href="#requirements"><img src="https://img.shields.io/badge/Rust-1.90%2B-000000?logo=rust&amp;logoColor=white" alt="Rust 1.90+" /></a>
  <a href="Cargo.toml"><img src="https://img.shields.io/badge/Version-0.11.2-2563EB" alt="Version 0.11.2" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-22C55E" alt="MIT License" /></a>
  <a href="#build-and-run"><img src="https://img.shields.io/badge/Desktop-Windows%20%7C%20macOS-0078D4" alt="Windows and macOS desktop" /></a>
</p>

<p align="center">
  <a href="README.md">简体中文</a> · <strong>English</strong>
</p>

<p align="center">
  <a href="#features">Features</a> ·
  <a href="#providers">Providers</a> ·
  <a href="#requirements">Requirements</a> ·
  <a href="#build-and-run">Build and run</a> ·
  <a href="#api">API</a> ·
  <a href="#layout">Layout</a> ·
  <a href="#tests">Tests</a> ·
  <a href="#license">License</a>
</p>

MultiGateway is a Rust reverse proxy for several AI account pools. WorkBuddy, Trae, MiniMax, Qoder, Antigravity, Grok, and GitHub Copilot share one desktop app and one set of OpenAI, Anthropic, and Responses endpoints.

Version **0.11.2** is the Rust gateway. The Tauri 2 desktop window, tray, and web dashboard are compiled into the Windows executable or macOS app.

---

## Features

- **Shared account pools** for seven providers, with session stickiness and a short cooldown after an account-level upstream failure.
- **China and international regions** for WorkBuddy, Trae, MiniMax, and Qoder. Catalogs, credit queries, and check-in follow the selected region.
- **Model catalogs** show the upstream name, context window, and credit factor, along with each model's image input, tool calls, output ceiling and thinking tiers. When the live catalog omits a factor, a known official snapshot fills it in, including peak and off-peak windows.
- **Check-in** for WorkBuddy personal accounts in China, for Trae, and for Qoder regions that expose the daily check-in API.
- **Compatible APIs**: `/v1/chat/completions`, `/v1/models`, `/v1/responses`, and `/v1/messages`. Streams are forwarded chunk by chunk, including tool calls and reasoning.
- **Usage board** by provider, model, and account.
- **Desktop shell** with WebView2, optional launch at login, silent start, and close-to-tray. Left-click the tray icon for the main window, right-click for a quick panel with today's usage, account readiness, and recent requests. A headless HTTP process is also available.

---

## Providers

| Provider | Regions | Check-in | Notes |
| --- | --- | --- | --- |
| **WorkBuddy** | China / International | China personal accounts | Quota query, account import, and credential refresh |
| **Trae** | China / International | Yes | Account import; international catalog, credits, and check-in use the international hosts |
| **MiniMax** | China / International | No | Account import and credential refresh; Token Plan quota needs a Subscription Key |
| **Qoder** | China / International | When the region exposes it | Account import; catalog includes credit factors; credentials can be refreshed |
| **Antigravity** | Single region | No | Account import and credential refresh |
| **Grok** | Single region | No | Account import, quota query, and credential refresh |
| **GitHub Copilot** | Single region | No | GitHub device-code login, then a short-lived Copilot token |

Request a model with its provider prefix, for example `trae/glm-5.3`, `qoder/Qwen3.8-Max`, or `copilot/gpt-4.1`. A grouped endpoint such as `/trae/v1/models` keeps the upstream model id.

---

## Requirements

- Windows 10/11 x64 or ARM64, or macOS 11+ on Apple Silicon or Intel
- Rust **1.90+** to build from source; MSVC C++ build tools on Windows or Xcode command line tools on macOS
- Tauri 2 uses system WebView2 on Windows and WKWebView on macOS, without a bundled browser runtime

---

## Build and run

```bash
git clone https://github.com/facron2004/MultiGateway.git
cd MultiGateway
cargo run -p mg-server -- --host 127.0.0.1 --port 8791
```

Release build:

```bash
cargo build --locked --release -p mg-server --bin MultiGateway
```

On Windows, use `run_multigateway.bat` to start the app or `build_multigateway.bat` to build the portable executable.

`tauri-build` generates the desktop resources, so Cargo builds the Tauri executable directly. Tauri serves the embedded desktop UI and invokes Rust business functions through IPC. The HTTP gateway serves model APIs for external clients and an optional browser panel. Both panels share one asset bundle and business state, without a Node.js build toolchain or a separate backend process. Tauri uses the system WebView2 runtime on Windows.

The binary is `target/release/MultiGateway.exe`. Run it from a writable directory. Accounts, API keys, and usage default to `gateway-data/` beside the executable. Point it at an existing directory with:

```cmd
MultiGateway.exe --host 127.0.0.1 --port 8791 --data-dir "D:\MultiGateway-data"
```

`--silent` starts the gateway without showing the window. `--headless` runs only the HTTP service. Quit from the tray panel to stop the process.

The dashboard is `http://127.0.0.1:8791/`. API keys are stored in `api_keys.json` inside the data directory, and the previous file is kept as `api_keys.json.bak`.

Exit the app before replacing the executable, and keep the existing `gateway-data/` directory.
Desktop browser data is stored in `webview2/` inside the selected data directory.

On macOS, build both `dist/MultiGateway-0.11.2-macos-arm64.dmg` and `dist/MultiGateway-0.11.2-macos-x64.dmg` with:

```bash
bash scripts/build-macos-dmg.sh
```

To build one architecture on a matching Mac, use `bash scripts/build-macos-dmg.sh aarch64-apple-darwin` or `bash scripts/build-macos-dmg.sh x86_64-apple-darwin`. Drag `MultiGateway.app` from the DMG into Applications. The app is ad-hoc signed and has not been notarized by Apple. macOS data defaults to `~/Library/Application Support/MultiGateway`; an existing `gateway-data/` beside the app takes precedence. Preserve the data directory when upgrading.

GitHub Actions tests, builds, and checks release packages on native Windows x64/ARM64 and macOS Apple Silicon/Intel runners. macOS checks also mount the DMG, verify its signature, and launch the packaged application.

---

## API

| Use | URL |
| --- | --- |
| Dashboard | `http://127.0.0.1:8791/` |
| OpenAI Chat | `http://127.0.0.1:8791/v1/chat/completions` |
| OpenAI Responses | `http://127.0.0.1:8791/v1/responses` |
| Anthropic Messages | `http://127.0.0.1:8791/v1/messages` |
| Model list | `http://127.0.0.1:8791/v1/models` |

Management endpoints (`/gateway/*`, `/settings*` and dashboard assets) accept loopback requests
only. Binding the gateway to `0.0.0.0` exposes the inference API to the LAN without exposing
account management, API keys or settings. Remote inference is rejected until at least one API key
exists; loopback clients can still use the gateway without one. Browser requests carrying a
non-loopback `Origin` are rejected too, so create an API key before calling the gateway from a web page.

The combined `/v1/models` list returns provider-prefixed ids only. Each model carries the capabilities the upstream actually reported, published under the same field set for every provider:

- **Image input**: `supports_vision`, `supports_images`, `vision`, `multimodal`, `abilities.vision`
- **Modalities**: `input_modalities`, `output_modalities`, `modalities`, `architecture`
- **Limits**: `context_length`, `max_input_tokens`, `max_output_tokens`, `max_completion_tokens`
- **Tools**: `supports_tool_calls`, `abilities.functionCall`
- **Thinking strength**: `reasoning_efforts`, `reasoning_default_effort`, `reasoning_can_disable`, plus a `thinking` block (`supported`, `can_disable`, `default_effort`, `efforts`, `levels`)

Thinking tiers share one ladder — `minimal`, `low`, `medium`, `high`, `xhigh`, `max` — and each provider's own naming is folded onto it (Trae's `light` becomes `low`, `extra_high` becomes `xhigh`). An extra `none` in `thinking.levels` means thinking can be turned off. `reasoning_default_effort` prefers the upstream's declared default, then the tier encoded in the model id (`gemini-3.8-flash-low` → `low`), then the provider's usual default.

Missing capabilities stay `null`. The one exception is a capability the provider is *known* to support (image input on Antigravity, Grok and MiniMax), which fills the gap but never overrides an explicit upstream answer. The fields a client switches on — the `supports_*` flags, `reasoning_can_disable`, `thinking`, and both modality lists — appear on **every** entry, so a client reads one shape for all providers.

Requests follow the same map: `reasoning_effort` is translated to each upstream's own control (Gemini's `thinkingBudget`, Trae's `reasoning_effort`, Antigravity's `thinkingConfig`), and the output ceiling is widened when needed so it always exceeds the thinking budget instead of failing upstream with a 400.

---

## Layout

```text
crates/
  mg-core/        Account pools, API keys, usage, and configuration
  mg-protocols/   OpenAI, Anthropic, Gemini, and Responses conversion
  mg-providers/   Catalogs, chat, quotas, and check-in for seven providers
  mg-server/      HTTP service, desktop window, and embedded dashboard
```

---

## Tests

```bash
cargo test --workspace
```

The tests use local fixtures. They do not need real accounts or upstream network access.

---

## License

[MIT](LICENSE).
Icon sources are listed in [`crates/mg-server/ui/icons/SOURCES.json`](crates/mg-server/ui/icons/SOURCES.json).
Third-party license texts are preserved in [`licenses/`](licenses/).
