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
