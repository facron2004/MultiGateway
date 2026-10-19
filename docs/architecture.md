# Repository structure

MultiGateway keeps its own gateway code separate from embedded provider runtimes.
The public launch and build commands remain `python gateway_app.py` and
`build_gateway.bat`.

## Gateway code

| Location | Responsibility |
| --- | --- |
| `gateway_app.py` | CLI options, data paths, HTTP server and desktop window |
| `gateway/runtime.py` | Provider lifecycle and shared model catalog |
| `gateway/handler.py` | Public HTTP routes, authentication and protocol dispatch |
| `gateway/control.py` | Dashboard management contract and unified usage board |
| `gateway/activity.py` | Gateway request history and stream usage observation |
| `gateway/ports.py` | Per-provider listeners |
| `gateway/protocols.py`, `gateway/transport.py` | Protocol translation and upstream HTTP transport |
| `gateway/minimax.py`, `gateway/minimax_pool.py` | MiniMax adapter and account pool |
| `gateway/trae.py`, `gateway/qoder.py` | Wrappers around embedded provider backends |
| `gateway/native/` | Antigravity, Grok and Copilot implementations and shared OAuth support |
| `gateway/dashboard.html`, `gateway/ui/` | Shared dashboard markup, scripts and styles, served at `/` |
| `gateway/tests/` | Local tests with mock providers; no live accounts required |

Changes to gateway routing, management or the shared dashboard belong here.
Provider-specific code belongs in its adapter or native provider module.

## Embedded runtimes

`gateway/vendor/workbuddy/`, `gateway/vendor/trae/` and `gateway/vendor/qoder/`
contain the embedded integrations. Their provenance and licenses are listed in
[`gateway/SOURCES.md`](../gateway/SOURCES.md).

WorkBuddy's `wb_*.py` modules and legacy `dashboard.html` were previously at the
repository root. They now share a package, matching the other embedded runtimes.
The legacy WorkBuddy panel remains available at `/workbuddy`.
Use package imports such as
`from gateway.vendor.workbuddy import wb_proxy`; sibling modules use relative
imports so all callers share the same account pool, settings and log state.

Keep upstream changes inside the corresponding vendor directory. MultiGateway's
Windows executable is built with `MultiGateway.spec` and `build_gateway.bat`;
the embedded WorkBuddy repacker is retained upstream code for its own desktop app.
When adding a resource read directly from disk, include it in the spec's `datas`.
This includes the WorkBuddy dashboard and downloadable `wb_agents.py` CLI source.

## Maintenance and data

`scripts/fetch_pricing.py` refreshes the bundled `pricing/pricing.json` snapshot:

```bash
python scripts/fetch_pricing.py --dry-run
python scripts/fetch_pricing.py
python scripts/fetch_pricing.py --embed
```

`--embed` also updates the fallback snapshot inside
`gateway/vendor/workbuddy/wb_pricing.py`. The script resolves paths from the
repository location, so it can run from another working directory.

Mutable gateway data remains in `gateway-data/` beside the launcher or executable,
or the directory selected by `--data-dir`. WorkBuddy accounts and usage remain
under `workbuddy/accounts/` and `workbuddy/usage/`; the other providers retain
their existing directories. Moving source files does not migrate user data.
An existing root-level `dashboard.html` remains a WorkBuddy panel override.

`gateway-build/`, `gateway-dist/`, `.gateway-deps/` and local runtime data are
ignored outputs. `docs/research/` contains provider protocol notes.

After changing module or resource paths, run:

```bash
python -m unittest discover -s gateway/tests
```
