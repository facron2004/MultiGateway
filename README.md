<p align="center">
  <img src="gateway/ui/icons/multigateway.png" width="112" height="112" alt="MultiGateway logo" />
</p>

<h1 align="center">MultiGateway</h1>

<p align="center">
  One gateway for multiple AI account pools.
</p>

<p align="center">
  <a href="#requirements"><img src="https://img.shields.io/badge/Python-3.11%2B-3776AB?logo=python&amp;logoColor=white" alt="Python 3.11+" /></a>
  <a href="gateway/__init__.py"><img src="https://img.shields.io/badge/Version-0.5.0-2563EB" alt="Version 0.5.0" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-22C55E" alt="MIT License" /></a>
  <a href="#4-build-windows-executable"><img src="https://img.shields.io/badge/Desktop-Windows-0078D4" alt="Windows desktop" /></a>
</p>

<p align="center">
  <a href="#features">Features</a> ·
  <a href="#providers">Providers</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#repository-layout">Structure</a> ·
  <a href="#running-tests">Tests</a> ·
  <a href="#license">License</a>
</p>

Multi-Account Reverse Proxy Gateway for **WorkBuddy**, **Trae**, **MiniMax Code**,
**Qoder**, **Antigravity**, **Grok**, and **GitHub Copilot**.

Current release: **0.5.0** — adds three in-process "native OAuth" providers
(Antigravity, Grok, GitHub Copilot) next to the existing vendored
WorkBuddy/Trae/MiniMax/Qoder integrations. Single-region providers hide the
国内版/国际版 switch in the dashboard.

---

## Features

- **Multi-Account Aggregation**: One gateway in front of seven providers — WorkBuddy, Trae, MiniMax Code, Qoder, Antigravity, Grok, GitHub Copilot — with account pools, session stickiness, and load balancing.
- **OAuth Account Pool**: In-process device-code and loopback authorization-code flows, credential import, silent token refresh, per-account health and region tracking.
- **Protocol Compatibility**: Full OpenAI (`/v1/chat/completions`, `/v1/models`, `/v1/responses`) and Anthropic (`/v1/messages`) compatibility layer, including SSE translation for providers that only speak Gemini or Responses-style APIs.
- **Reasoning Controls**: Native support for extended reasoning effort levels, including custom effort tiers and real-time reasoning token tracking.
- **Desktop & Headless Modes**: Run with an embedded frameless WebView2 desktop UI or in headless mode as a background service/daemon.
- **Unified Usage Board**: Requests from every provider land in one dashboard with per-provider, per-model, per-account usage, tokens, credits, and cost estimates.
- **One-Click Local Agent Integration**: Integrated configurator for AI clients (Codex CLI, OpenCode, DSH, Crush, ZCode, Claude Code).

---

## Providers

| Provider | What it is | OAuth flow | Requirements & caveats |
| --- | --- | --- | --- |
| **WorkBuddy** | WorkBuddy desktop agent account pool | Browser account login, polled by the local WorkBuddy backend | Uses the existing local WorkBuddy/WorkBuddy2API-Hub installation; 国内/国际 realms |
| **Trae** | Trae coding agent (vendored backend) | Browser web login forwarded to a loopback helper (port 8765) | Single region (国内); no 国际版 switch |
| **MiniMax Code** | MiniMax Code agent (MIT client protocol adapter) | OAuth device code + PKCE (国内/国际 endpoints) | 国内/国际 realms; no account import |
| **Qoder** | Qoder coding agent (vendored backend) | Browser login handled by the bundled Qoder backend | 国内/国际 realms |
| **Antigravity** | Google Antigravity IDE account | Google OAuth authorization code over a loopback callback | Needs local OAuth client settings and a Google account with Cloud Code Assist access; quota is per Google account/tier, endpoints are undocumented and churn |
| **Grok** | xAI Grok CLI / Grok Build account | xAI OAuth authorization code + PKCE over a loopback callback | Responses API only (Chat Completions is not offered to OAuth tokens); free tier is gated by account tier and can return 402/403 at inference time |
| **GitHub Copilot** | GitHub Copilot seat | GitHub device flow, then exchange the GitHub token for a short-lived Copilot token | Requires an active Copilot seat; per-request editor/header fingerprinting and the ~20-minute token refresh are mandatory |

Single-region providers (Trae, Antigravity, Grok, GitHub Copilot) have
no region choice and hide the region switch; WorkBuddy, MiniMax Code and Qoder keep
the 国内/国际 switch.

Provider-specific notes:

- **Antigravity** requires the Google account to have Code Assist access (a Pro/Ultra
  subscription or eligible free tier). Its quota is fractional per model, and the
  upstream endpoints are observed behaviour, not a published API.
- **Grok** OAuth can only call the Responses API; the gateway translates OpenAI Chat
  and Anthropic Messages requests internally. Standard/free accounts may be limited to
  the CLI transport and a single free model, so entitlement errors must be surfaced, not hidden.
- **GitHub Copilot** requires a Copilot seat. GitHub's terms restrict automated or
  shared use, so keep one seat per user and cap concurrency per account.

OAuth grants for the three native providers are stored under the gateway data
directory (`native/<provider>/accounts/`); only the user's own accounts and quota
are used.

---

## Requirements

- Python 3.11+ (Python 3.13 recommended; required by the Windows build script)
- Dependencies listed in `requirements-gateway.txt`

---

## Quick Start

### 1. Install Dependencies

```bash
git clone https://github.com/facron2004/MultiGateway.git
cd MultiGateway
python -m pip install -r requirements-gateway.txt
```

### 2. Run Headless Mode

```bash
python gateway_app.py --headless --host 127.0.0.1 --port 8791
```

Access dashboard at `http://127.0.0.1:8791/` (token will be printed in console output).

For Antigravity OAuth, copy [`.env.example`](.env.example) to `.env` beside the
launcher or executable, then fill in the OAuth client ID and secret used by your
Antigravity installation. Environment variables with the same names take
precedence. `.env` stays local and is excluded from Git; the other providers do
not require this configuration.

### 3. Run Desktop Application

```bash
python gateway_app.py
```

### 4. Build Windows Executable

```cmd
build_gateway.bat
```

This will produce `MultiGateway.exe` using PyInstaller.

---

## Repository Layout

```text
gateway_app.py          Desktop and headless entry point
gateway/               Gateway routing, management and provider adapters
  native/              In-process OAuth providers
  ui/                  Shared dashboard assets
  vendor/              Embedded WorkBuddy, Trae and Qoder runtimes
  tests/               Local protocol and HTTP integration tests
scripts/               Maintenance tools (fetch_pricing.py)
pricing/               Bundled price snapshot
docs/                  Architecture notes and protocol research
```

See [the structure guide](docs/architecture.md) for module responsibilities and
the boundary between gateway code, embedded integrations and runtime data.

---

## Running Tests

Run the gateway test suite:

```bash
python -m unittest discover -s gateway/tests
```

Native provider tests live in `gateway/tests/test_native_<provider>.py`; they mock the
vendor protocol locally, so no real account or network access is needed.

---

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
Embedded integrations and their licenses are documented in [gateway/SOURCES.md](gateway/SOURCES.md).
The three 0.5.0 OAuth integrations are independent implementations; reference projects
that were consulted for behaviour only are listed there.
