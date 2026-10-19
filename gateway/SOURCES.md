# Embedded sources

Inspected and pinned on 2026-10-13. Credentials, account databases and developer
test fixtures from upstream repositories are never included in the executable.

| Integration | Source | Commit | License |
| --- | --- | --- | --- |
| WorkBuddy | Existing local WorkBuddy2API-Hub installation, embedded in vendor/workbuddy/ | Local installation retained | Root LICENSE (MIT) |
| Trae | https://github.com/autumnsentiment/Trae2api-cn | 5283c94290b8503ad8af3658107a64b73eeb032f | vendor/trae/LICENSE (MIT) |
| MiniMax Code | https://github.com/MiniMax-AI/minimax-code | cee079138600224e9ad4d8b954c4a4ed549e15b6 | licenses/MINIMAX-LICENSE (MIT) |
| Qoder | https://github.com/shuishuipingan/qoder2api-hub | ae31046ba387392ff6dcc4170a978081c409fdfb | vendor/qoder/LICENSE (MIT) |

WorkBuddy's modules and legacy dashboard live in `vendor/workbuddy/`. Sibling
imports are package-relative; runtime module lookups and bundled resource paths
follow that package. Credentials, usage paths and provider logic are preserved.

Trae's runtime modules retain their logic; auth_helper's web_login import is
changed to a relative import. The integration wrapper redirects its
mutable files to the gateway data directory and starts an authenticated private
listener. Qoder imports are changed to relative imports so its original module
names cannot collide with other integrations; its runtime logic is preserved.
MiniMax's PKCE device flow, regional endpoints and inference headers follow the
official oauth-core and local-runtime-v2 source; the gateway uses a small Python
adapter instead of bundling the full coding agent.

Other projects researched: https://github.com/denysvitali/llm-proxy (MiniMax account
gateway reference), https://github.com/linqiu919/trae2api (archived, rejected),
https://github.com/wo5366/trae (Node runtime, not embedded), and
https://github.com/TomLiu-QianYuan/minimax-code-api-proxy (client API replacement,
the opposite integration direction, not embedded).

The 0.5.0 sidebar SVG assets are static SVGs from @lobehub/icons-static-svg
1.95.1 ([Lobe Icons](https://github.com/lobehub/lobe-icons), MIT), including the
official Antigravity, Grok and GitHub Copilot vendor marks (`antigravity.svg`,
`grok.svg`, `copilot-color.svg`). Only these SVG assets
are embedded; no React runtime is added. Exact asset names are recorded in
ui/icons/SOURCES.json, with the license in licenses/LOBE-ICONS-LICENSE.

The MultiGateway application icon is original artwork generated with the built-in
imagegen tool. `ui/icons/multigateway.png` is the dashboard brand asset; the root
`app_icon.ico` contains 16–256 px Windows icon frames and is also the favicon.
The design prompt and export sizes are recorded in `ui/icons/multigateway.json`.

## Native OAuth providers (0.5.0)

The 0.5.0 providers — Antigravity, Grok and GitHub Copilot — are
**independent implementations written for this repository**. They speak the same
vendor OAuth and model protocols as the official clients, based on publicly
observable behaviour: OIDC discovery documents, public client metadata that
vendors ship inside their own CLI/IDE binaries (installed-app clients documented
as non-secret), and observed request/response traffic. No upstream project's
source code was vendored, copied, adapted or linked for these integrations,
and no upstream commit is pinned for them. Surfaces were reviewed on 2026-10-18.

Reference projects were consulted as behavioural specifications only:

| Provider | Reference projects (name only) |
| --- | --- |
| Antigravity | CLIProxyAPI; Antigravity-Manager; opencode-antigravity-auth |
| Grok | CLIProxyAPI; grok2api |
| GitHub Copilot | GitHub device-flow and Copilot-token documentation; CLIProxyAPI architecture |

Licence caveats:

- No GPL or CC BY-NC code was copied. AIClient-2-API is GPL-3.0 and
  Antigravity-Manager is CC BY-NC-SA 4.0; both were read as references only.
  grok2api is a web-session proxy whose architecture was noted, not reused.
- None of the reference projects endorses or is affiliated with this repository.

Terms-of-service risk: several vendor surfaces used by these integrations — Cloud
Code Assist for Antigravity, the Grok CLI transport and the Copilot token exchange
— are not published for third-party clients and are not
official partner APIs. Endpoints, client identities and headers can change or be
withdrawn at any time; use the gateway only with your own accounts and quota, and
expect breakage on vendor releases.

