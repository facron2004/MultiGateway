# MultiGateway

Multi-Account Reverse Proxy Gateway for **WorkBuddy**, **Trae**, **MiniMax Code**, and **Qoder**.

---

## Features

- **Multi-Account Aggregation**: Unified multi-provider reverse proxy supporting WorkBuddy, Trae, MiniMax Code, and Qoder with intelligent routing, session stickiness, and load balancing.
- **Protocol Compatibility**: Full OpenAI (`/v1/chat/completions`, `/v1/models`, `/v1/responses`) and Anthropic (`/v1/messages`) compatibility layer.
- **Reasoning Controls**: Native support for extended reasoning effort levels, including custom effort tiers and real-time reasoning token tracking.
- **Desktop & Headless Modes**: Run with an embedded frameless WebView2 desktop UI or in headless mode as a background service/daemon.
- **One-Click Local Agent Integration**: Integrated configurator for AI clients (Codex CLI, OpenCode, DSH, Crush, ZCode, Claude Code).

---

## Requirements

- Python 3.11+ (Python 3.13 recommended)
- Dependencies listed in `requirements-gateway.txt`

---

## Quick Start

### 1. Install Dependencies

```bash
pip install -r requirements-gateway.txt
```

### 2. Run Headless Mode

```bash
python gateway_app.py --headless --host 127.0.0.1 --port 8791
```

Access dashboard at `http://127.0.0.1:8791/` (token will be printed in console output).

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

## Running Tests

Run the gateway test suite:

```bash
python -m unittest gateway.tests.test_gateway
```

---

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
Embedded integrations and their licenses are documented in [gateway/SOURCES.md](gateway/SOURCES.md).
