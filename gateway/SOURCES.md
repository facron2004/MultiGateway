# Embedded sources

Inspected and pinned on 2026-10-13. Credentials, account databases and developer
test fixtures from upstream repositories are never included in the executable.

| Integration | Source | Commit | License |
| --- | --- | --- | --- |
| WorkBuddy | Existing local WorkBuddy2API-Hub installation | Local installation retained | Root LICENSE (MIT) |
| Trae | https://github.com/autumnsentiment/Trae2api-cn | 5283c94290b8503ad8af3658107a64b73eeb032f | vendor/trae/LICENSE (MIT) |
| MiniMax Code | https://github.com/MiniMax-AI/minimax-code | cee079138600224e9ad4d8b954c4a4ed549e15b6 | licenses/MINIMAX-LICENSE (MIT) |
| Qoder | https://github.com/shuishuipingan/qoder2api-hub | ae31046ba387392ff6dcc4170a978081c409fdfb | vendor/qoder/LICENSE (MIT) |

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

The 0.3 sidebar uses the existing local app_icon.ico for WorkBuddy and the
Trae, MiniMax, Qoder static SVGs from @lobehub/icons-static-svg 1.95.1
([Lobe Icons](https://github.com/lobehub/lobe-icons), MIT). Only these SVG assets
are embedded; no React runtime is added. Exact asset names are recorded in
ui/icons/SOURCES.json, with the license in licenses/LOBE-ICONS-LICENSE.
The four 0.5.0 provider marks (`antigravity.svg`, `grok.svg`, `copilot.svg`,
`qwen.svg`) are original geometric SVGs authored for this project; they are not
vendor trademarks and are not part of the Lobe icon set.

## Native OAuth providers (0.5.0)

The 0.5.0 providers — Antigravity, Grok, GitHub Copilot and Qwen Code — are
**independent implementations written for this repository**. They speak the same
vendor OAuth and model protocols as the official clients, based on publicly
observable behaviour: OIDC discovery documents, public client metadata that
vendors ship inside their own CLI/IDE binaries (installed-app clients documented
as non-secret), and observed request/response traffic. No upstream project's
source code was vendored, copied, adapted or linked for these four integrations,
and no upstream commit is pinned for them. Surfaces were reviewed on 2026-10-18.

Reference projects were consulted as behavioural specifications only:

| Provider | Reference projects (name only) |
| --- | --- |
| Antigravity | CLIProxyAPI; Antigravity-Manager; opencode-antigravity-auth |
| Grok | CLIProxyAPI; grok2api |
| GitHub Copilot | GitHub device-flow and Copilot-token documentation; CLIProxyAPI architecture |
| Qwen Code | Qwen Code upstream OAuth flow (specification only) |

Licence caveats:

- No GPL or CC BY-NC code was copied. AIClient-2-API is GPL-3.0 and
  Antigravity-Manager is CC BY-NC-SA 4.0; both were read as references only.
  grok2api is a web-session proxy whose architecture was noted, not reused.
- None of the reference projects endorses or is affiliated with this repository.

Terms-of-service risk: several vendor surfaces used by these integrations — Cloud
Code Assist for Antigravity, the Grok CLI transport, the Copilot token exchange
and the Qwen portal — are not published for third-party clients and are not
official partner APIs. Endpoints, client identities and headers can change or be
withdrawn at any time; use the gateway only with your own accounts and quota, and
expect breakage on vendor releases.

