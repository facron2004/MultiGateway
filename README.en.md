<p align="center">
  <img src="crates/mg-server/ui/icons/multigateway.png" width="112" height="112" alt="MultiGateway logo" />
</p>

<h1 align="center">MultiGateway</h1>

<p align="center">
  One gateway for every AI account pool.
</p>

<p align="center">
  <a href="#requirements"><img src="https://img.shields.io/badge/Rust-1.90%2B-000000?logo=rust&amp;logoColor=white" alt="Rust 1.90+" /></a>
  <a href="Cargo.toml"><img src="https://img.shields.io/badge/Version-0.11.1-2563EB" alt="Version 0.11.1" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-22C55E" alt="MIT License" /></a>
  <a href="#build-and-run"><img src="https://img.shields.io/badge/Desktop-Windows-0078D4?logo=windows&amp;logoColor=white" alt="Windows desktop" /></a>
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

Version **0.11.1** is the Rust gateway. The desktop window, tray, and web dashboard are compiled into `MultiGateway.exe`.

---

## Features

- **Shared account pools** for seven providers, with session stickiness and a short cooldown after an account-level upstream failure.
- **China and international regions** for WorkBuddy, Trae, MiniMax, and Qoder. Catalogs, credit queries, and check-in follow the selected region.
- **Model catalogs** show the upstream name, context window, and credit factor. When the live catalog omits a factor, a known official snapshot fills it in, including peak and off-peak windows.
- **Check-in** for WorkBuddy personal accounts in China, for Trae, and for Qoder regions that expose the daily check-in API.
- **Compatible APIs**: `/v1/chat/completions`, `/v1/models`, `/v1/responses`, and `/v1/messages`. Streams are forwarded chunk by chunk, including tool calls and reasoning.
- **Usage board** by provider, model, and account.
- **Desktop shell** with WebView2, optional launch at login, silent start, and close-to-tray. A headless HTTP process is also available.

---

## Providers

| Provider | Regions | Check-in | Notes |
| --- | --- | --- | --- |
| **WorkBuddy** | China / International | China personal accounts | Quota query and credential refresh |
| **Trae** | China / International | Yes | International catalog, credits, and check-in use the international hosts |
| **MiniMax** | China / International | No | Credential refresh; Token Plan quota needs a Subscription Key |
| **Qoder** | China / International | When the region exposes it | Catalog includes credit factors; credentials can be refreshed |
| **Antigravity** | Single region | No | Account import and credential refresh |
| **Grok** | Single region | No | Account import, quota query, and credential refresh |
| **GitHub Copilot** | Single region | No | GitHub device-code login, then a short-lived Copilot token |

Request a model with its provider prefix, for example `trae/glm-5.3`, `qoder/Qwen3.8-Max`, or `copilot/gpt-4.1`. A grouped endpoint such as `/trae/v1/models` keeps the upstream model id.

---

## Requirements

- Windows 10/11 x64
- Rust **1.90+** and the MSVC C++ build tools to build from source
- WebView2 for the desktop window

---

## Build and run

```bash
git clone https://github.com/facron2004/MultiGateway.git
cd MultiGateway
cargo run -p mg-server -- --host 127.0.0.1 --port 8791
```

Release build:

```bash
cargo build --release -p mg-server
```

The binary is `target/release/MultiGateway.exe`. Run it from a writable directory. Accounts, API keys, and usage default to `gateway-data/` beside the executable. Point it at an existing directory with:

```cmd
MultiGateway.exe --host 127.0.0.1 --port 8791 --data-dir "D:\MultiGateway-data"
```

`--silent` starts the gateway without showing the window. `--headless` runs only the HTTP service. Quit from the tray menu to stop the process.

The dashboard is `http://127.0.0.1:8791/`. API keys are stored in `api_keys.json` inside the data directory, and the previous file is kept as `api_keys.json.bak`.

Exit the app before replacing the executable, and keep the existing `gateway-data/` directory.

---

## API

| Use | URL |
| --- | --- |
| Dashboard | `http://127.0.0.1:8791/` |
| OpenAI Chat | `http://127.0.0.1:8791/v1/chat/completions` |
| OpenAI Responses | `http://127.0.0.1:8791/v1/responses` |
| Anthropic Messages | `http://127.0.0.1:8791/v1/messages` |
| Model list | `http://127.0.0.1:8791/v1/models` |

The combined `/v1/models` list returns provider-prefixed ids only. Each model carries the capabilities the upstream actually reported:

- `supports_reasoning` and `capabilities.reasoning`
- `reasoning_efforts` and `reasoning_default_effort`
- `supports_vision` and `input_modalities`

Missing capabilities stay `null`. The gateway does not infer them from the model name.

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
