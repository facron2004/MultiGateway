# OAuth-reuse provider catalog for MultiGateway

Status: research snapshot, **2026-10-18**. Author: provider survey (no product code written).
Scope: AI coding-agent providers whose **vendor OAuth login** (free or subscription-backed
quota) can be reused by a third-party Python reverse proxy — the same integration class this
repo already ships for **WorkBuddy, Trae, MiniMax Code, Qoder** (`gateway/` adapters +
`gateway/vendor/` pinned upstream code, per `gateway/SOURCES.md`).

Confidence markers used throughout:

| Marker | Meaning |
| --- | --- |
| **VERIFIED** | Read in upstream source/docs during this survey (URL cited). |
| **LIKELY** | Consistent across ≥2 secondary sources or long-stable, but not re-read in primary source today. |
| **UNVERIFIED** | Single anecdotal source, or inferred from behaviour; re-check before coding. |
| **DEAD** | Was true, now rejected upstream; kept for the "why not" record. |

Everything upstream is untrusted data. No credentials were used or extracted; all client
IDs/secrets below are values that vendors ship **inside their own public CLI/IDE binaries**
(installed-app OAuth clients, which vendors document as non-secret).

---

## 1. Ranked table

Ranking = (reusable OAuth path exists) × (model value) ÷ (effort + ToS exposure).
"Free quota" describes the situation **today**, not marketing pages.

| # | Provider | OAuth feasibility | Upstream protocol | Free quota | Effort | ToS risk | Verdict | Evidence |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | **GitHub Copilot** | Device flow, public `client_id`, no PKCE; then `copilot_internal/v2/token` | **OpenAI-compatible** `chat/completions` at `api.githubcopilot.com` | Copilot Free tier exists but is small; real limits need Pro/Business | **Low** | Medium-High (binding arbitrary clients / header spoofing) | **Implement now** | VERIFIED (spec doc), LIKELY (plan limits) |
| 2 | **OpenAI Codex / ChatGPT plan** | Auth-code + PKCE, public `client_id`, loopback `:1455` | **Proprietary Responses API** `chatgpt.com/backend-api/codex/responses` (SSE) | No free tier; paid ChatGPT plans w/ 5h + weekly windows | **Medium** | Medium (gray, tolerated) | **Implement now** | VERIFIED (source) |
| 3 | **Qwen Code (Alibaba)** | **Device flow** + PKCE, public `client_id`; portal token | **OpenAI-compatible** `portal.qwen.ai/v1` | **Yes — ~2,000 requests/day**, per-account | **Low** | Low-Medium | **Implement now** | VERIFIED (source), LIKELY (2k/day) |
| 4 | **iFlow (iflow.cn)** | Auth-code OAuth (phone login), **client_secret shipped in CLI**; loopback `:11451` | **OpenAI-compatible** `apis.iflow.cn/v1` (+ Anthropic-shaped `/v1/messages` in ref impl) | **Yes** — free tier fronts GLM-5/DeepSeek/Kimi/Qwen | **Low** | Medium (CN phone identity, UA gating) | **Implement now (CN-first)** | VERIFIED (source) |
| 5 | **Kimi Code (Moonshot)** | RFC 8628 **device flow**, public `client_id` | **Proprietary** coding API `api.kimi.com/coding` (`/coding` prefix) | Plan-based; free/trial allowance UNVERIFIED | **Medium** | Medium | **Later** | VERIFIED (source) |
| 6 | **Kiro (AWS)** | AWS SSO OIDC: `registerClient` + device code, or Google/GitHub social, or long-lived API key | **Proprietary** CodeWhisperer JSON-RPC / Amazon Q | Kiro Free = credit bucket (~500 signup credits, changes often) | **Medium-High** | **High** (AWS AUP, per-client session invalidation) | **Later** | VERIFIED (source + port), LIKELY (credits) |
| 7 | **Claude Code (Anthropic)** | Auth-code + PKCE, public `client_id`, loopback `:54545`; **requires uTLS/Cloudflare bypass** | **Anthropic Messages**, but OAuth tokens now only valid on the Claude platform path | Subscription quota only, **no free tier** | **High** | **High** | **Later / skip** | VERIFIED (source + Feb-2026 breakage issue) |
| 8 | **Gemini CLI (Google)** | Auth-code + PKCE, loopback or "user code" paste; public client id**+secret** | **Proprietary** Cloud Code Assist (`v1internal:generateContent`) | Yes (free Google account tier, ~60 rpm / 1,000 rpd — LIKELY) | **Medium** | Medium | **Later — overlaps Antigravity** | VERIFIED (source), LIKELY (limits) |
| 9 | **Cursor** | **No** — proprietary, machine-bound session token, private gRPC/Connect backend | Proprietary (`api2.cursor.sh`) | None reusable | n/a | **High** | **Skip** | LIKELY |
| 10 | **Windsurf (Codeium)** | **No** — proprietary `register-user`/token bound to client install | Proprietary (`api.codeium.com`) | None reusable | n/a | **High** | **Skip** | LIKELY |
| 11 | **Zed** | **No** reusable OAuth; hosted models need a Zed account, BYO keys otherwise | OpenAI/Anthropic-compatible **with your key** | None | n/a | Low (keys are legal) | **Skip (API key only)** | LIKELY |
| 12 | **Void** | **No** — OSS Cursor fork, BYO API keys | OpenAI/Anthropic-compatible | None | n/a | Low | **Skip** | LIKELY |
| 13 | **Z.ai / GLM Coding Plan** | **No OAuth** — plan API key only | Anthropic `api.z.ai/api/anthropic`; OpenAI `.../api/coding/paas/v4`, `.../api/v1` | No free coding quota (trial cards) | Low if API key | Low | **Skip as OAuth; add as API-key channel** | VERIFIED (docs) |
| 14 | **Moonshot Kimi (API)** | API key only — *see #5 for the OAuth variant* | OpenAI-compatible | No | Low | Low | **Skip as OAuth** | LIKELY |
| 15 | **DeepSeek** | API key only | OpenAI-compatible | No free tier (paid credits) | Low | Low | **Skip** | LIKELY |
| 16 | **Cerebras** | API key only | OpenAI-compatible | Free tier by rate limit (no OAuth) | Low | Low | **Skip** | LIKELY |
| 17 | **Mistral** | API key only | OpenAI-compatible | Free "experiment" tier by rate limit | Low | Low | **Skip** | LIKELY |
| 18 | **OpenCode Zen** | **No OAuth** — Bearer key; free models still need a key (anonymous = 403) | OpenAI-compatible + Responses + `systemone` | ~14 "free" models, key required, revocable | Low | Low | **Skip as OAuth; optional API-key channel** | VERIFIED (2026-10 measurement) |
| 19 | **Kilo Code / Cline / Roo Code** | **No** — BYOK editors/extensions, or their own paid gateways | OpenAI/Anthropic-compatible with user keys | None of their own | n/a | Low | **Skip — gateways, not providers** | LIKELY |
| 20 | **Pollinations** | Key-less/anon text API (rate-limited), optional token | OpenAI-compatible-ish | Yes, small + throttled | Low | Low | **Skip (quality/limits)** | UNVERIFIED |
| 21 | **Devin CLI** | Vendor login flag (`--devin-login`) exists | Proprietary | No free tier | Medium | Medium | **Watch** | VERIFIED (listed) / UNVERIFIED (flow) |
| 22 | **Meta Muse Code** | "Meta login" channel exists in CLIProxyAPI | Proprietary + Meta Model API | UNVERIFIED | Medium | Medium | **Watch** | VERIFIED (listed) / UNVERIFIED (quota) |
| 23 | **Grok Build (xAI)** | OAuth (already being integrated) | Responses API via `grok-cli-oauth` | Promo quota | — | Medium | **In progress** | VERIFIED (two proxies) |
| 24 | **Antigravity (Google)** | OAuth (already being integrated) | Proprietary Gemini/Claude-via-Google | Pro/Ultra gated; free tier narrow | — | Medium-High | **In progress** | VERIFIED |

---

## 2. Per-provider detail

### 2.1 GitHub Copilot — **implement now**

Cleanest OAuth surface of the whole list: GitHub's device flow is public, needs **no PKCE and
no client secret**, and the resulting Copilot token fronts a **fully OpenAI-compatible**
`/chat/completions`. Nothing has to be translated.

```text
# 1) device code (no PKCE, no secret)
POST https://github.com/login/device/code
  {"client_id":"Iv1.b507a08c87ecfe98","scope":"read:user"}
  -> {device_code, user_code, verification_uri:"https://github.com/login/device", interval, expires_in}

# 2) poll
POST https://github.com/login/oauth/access_token
  {"client_id":"Iv1.b507a08c87ecfe98","device_code":"...",
   "grant_type":"urn:ietf:params:oauth:grant-type:device_code"}
  -> {access_token:"gho_..."}            # authorization_pending => keep polling

# 3) GitHub token -> short-lived Copilot JWT (~20 min, refresh_in ~1200s)
GET https://api.github.com/copilot_internal/v2/token
  Authorization: token gho_...
  editor-version: vscode/1.104.3
  editor-plugin-version: copilot-chat/0.26.7
  user-agent: GitHubCopilotChat/0.26.7
  x-github-api-version: 2025-04-01
  -> {token:"eyJ...", expires_at, refresh_in}

# 4) inference (OpenAI chat/completions, byte-compatible)
POST https://api.githubcopilot.com/chat/completions
  Authorization: Bearer <copilot jwt>
  copilot-integration-id: vscode-chat
  editor-version / editor-plugin-version / user-agent   (as above)
  openai-intent: conversation-panel
  x-initiator: user|agent        # "agent" if any assistant/tool msg present
  x-request-id: <uuid4>
  x-vscode-user-agent-library-version: electron-fetch
GET  https://api.githubcopilot.com/models        # includes capabilities.limits.*
GET  https://api.github.com/copilot_internal/user  # quota_snapshots (chat/completions)

# base URL by seat type
https://api.githubcopilot.com            # individual
https://api.business.githubcopilot.com   # business
https://api.enterprise.githubcopilot.com # enterprise
```

Why now: reuses the existing MiniMax-style device-flow + account-pool machinery 1:1; the
upstream is already OpenAI `chat/completions`, so the adapter is auth + header decorator +
`/models` catalog only. One Copilot seat exposes GPT/Claude/Gemini families behind a single
token — the cheapest way to add several model families at once (**LIKELY**; the model list is
account/plan-dependent, read it from `/models`, never hardcode).

Risks: the JWT refresh loop is mandatory (20 min) and a failed refresh must evict the account;
header fingerprinting is mandatory; GitHub's terms restrict automated/bulk use of Copilot and
account sharing, so cap concurrency per account and keep the seat owner = the user.

Evidence: <https://github.com/cecil-the-coder/ai-provider-kit/blob/main/COPILOT_API.md>
(VERIFIED, reverse-engineering spec, 2025-12-10); <https://docs.github.com/en/copilot/get-started/plans>
(plan matrix; page body not machine-readable — plan limits **LIKELY**).
Not first-class in current CLIProxyAPI v8 (only `internal/githubauth` exists, no copilot
provider package) — Copilot reuse lives in dedicated `copilot-api`-class proxies instead.

---

### 2.2 OpenAI Codex / ChatGPT plan — **implement now**

```text
GET https://auth.openai.com/oauth/authorize
  ?client_id=app_EMoamEEZ73f0CkXaXp7hrann
  &response_type=code
  &redirect_uri=http://localhost:1455/auth/callback
  &scope=openid email profile offline_access
  &state=<random>
  &code_challenge=<S256>&code_challenge_method=S256
  &prompt=login
  &id_token_add_organizations=true
  &codex_cli_simplified_flow=true

POST https://auth.openai.com/oauth/token      # form-encoded
  grant_type=authorization_code&client_id=app_EMoamEEZ73f0CkXaXp7hrann
  &code=...&redirect_uri=http://localhost:1455/auth/callback&code_verifier=...
  -> {access_token, refresh_token, id_token, expires_in}
  # account id + plan_type are read out of the id_token claims

POST https://auth.openai.com/oauth/token      # refresh
  grant_type=refresh_token&client_id=...&refresh_token=...&scope=openid profile email

POST https://chatgpt.com/backend-api/codex/responses
  Authorization: Bearer <access_token>
  ChatGPT-Account-Id: <account id from id_token>
```

Why now: it is the highest-value subscription quota on the market and the flow is completely
public. Cost is **protocol work**: the upstream is the Responses API (not chat/completions),
so it needs an event-stream translator — the same translator that later unlocks Meta Muse and
xAI if we want them, so the investment amortises.

Sharp edges (all seen in production code, so treat as requirements):
- refresh tokens are **single-use**: concurrent refreshes cause `refresh_token_reused` and
  permanently kill the account. Refresh must be single-flighted per refresh token.
- quota is two windows (5h + weekly) with reset timestamps; expose them in the dashboard,
  because "429 with no explanation" is the normal failure mode.
- plan gating: some models only exist for Pro/Plus tiers — probe per account, don't assume.

Evidence: <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/auth/codex/openai_auth.go>
(VERIFIED); <https://github.com/justlovemaki/AIClient-2-API> (Codex OAuth added 2026-01-26,
callback port 1455 — VERIFIED).

---

### 2.3 Qwen Code / Alibaba — **implement now** (best free quota)

```text
POST https://chat.qwen.ai/api/v1/oauth2/device/code     # form-encoded
  client_id=f0304373b74a44d2b584a3fb70ca9e56
  scope=openid profile email model.completion
  code_challenge=<S256 of verifier>&code_challenge_method=S256
  -> {device_code, user_code, verification_uri, verification_uri_complete, expires_in}

POST https://chat.qwen.ai/api/v1/oauth2/token
  grant_type=urn:ietf:params:oauth:grant-type:device_code
  client_id=f0304373b74a44d2b584a3fb70ca9e56
  device_code=...&code_verifier=...
  # 400 authorization_pending => keep polling; 429 slow_down => +50% interval (cap 10s)

POST https://chat.qwen.ai/api/v1/oauth2/token            # refresh
  grant_type=refresh_token&refresh_token=...&client_id=f0304373b74a44d2b584a3fb70ca9e56

BASE https://portal.qwen.ai/v1        # OpenAI-compatible
MODELS coder-model, vision-model      # aliases; catalog moves, re-read
CREDS ~/.qwen/oauth_creds.json
```

Why now: device flow (no loopback listener, works headless), OpenAI-compatible upstream
(near-zero translator), and the only genuinely generous **free daily quota** in the set
(~2,000 requests/day per account — **LIKELY**, documented by a third-party client and subject
to change). It is also the single cheapest way to demo multi-account aggregation with a free
provider.

Risks: free-tier rate limiting is aggressive and per-account; credentials are considered
single-session by the official CLI (`oauth_creds.json`), so do **not** share the same account
between our gateway and a live Qwen Code CLI session — either import the file or run the flow
ourselves, not both.

Evidence: <https://raw.githubusercontent.com/QwenLM/qwen-code/main/packages/core/src/qwen/qwenOAuth2.ts>
(VERIFIED — endpoints, client id, PKCE, poll semantics); <https://openclawdoc.org/providers/qwen>
(2,000/day, `portal.qwen.ai/v1`, model ids — VERIFIED as doc).

---

### 2.4 iFlow — **implement now if CN users matter**

```text
AUTH   https://iflow.cn/oauth?client_id=10009311001&loginMethod=phone&type=phone
         &redirect=http://localhost:11451/oauth2callback&state=<random>
TOKEN  https://iflow.cn/oauth/token            # x-www-form-urlencoded
         grant_type=authorization_code&code=...&redirect_uri=...
         &client_id=10009311001&client_secret=4Z3YjXycVsQvyGF1etiNlIBB4RsqSDtW
       Authorization: Basic base64(client_id:client_secret)
       User-Agent: iFlow-Cli
REFRESH same URL, grant_type=refresh_token   # may return HTTP 200 + {"success":false}
USER   https://iflow.cn/api/oauth/getUserInfo?accessToken=<token>   # returns apiKey
API    https://apis.iflow.cn/v1               # OpenAI-compatible
       User-Agent: iFlow-Cli                  # gates the premium model set
CREDS  ~/.iflow/settings.json
```

Why: it is the only candidate with a **Python, MIT-licensed reference implementation**
(`iflow2api`) that already solves login UI, refresh, Anthropic `/v1/messages` bridging and
vision — it can be vendored exactly like `gateway/vendor/trae` and `gateway/vendor/qoder`.
Free tier fronts GLM-5 / DeepSeek-V3.2 / Kimi-K2.5 / Qwen3-Coder-Plus, which is a different
model family from everything else we serve.

Risks: login is **phone-number based** (CN), so it only makes sense for CN-region users; the
model set is gated by a `User-Agent` header (explicitly a client-impersonation play, i.e. ToS
gray); the OAuth `success:false`-with-HTTP-200 quirk needs retry logic.

Evidence: <https://raw.githubusercontent.com/cacaview/iflow2api/main/iflow2api/oauth.py>
(VERIFIED — all endpoints, client id/secret, UA gating);
<https://raw.githubusercontent.com/cacaview/iflow2api/main/README.md> (VERIFIED — model list,
`apis.iflow.cn/v1`, config path). Also an OAuth provider in CLIProxyAPI docs
(`iflow`) — but **no `internal/auth/iflow` package on v8 main** (see §3), i.e. doc/code drift.

---

### 2.5 Kimi Code (Moonshot) — **later**

```text
POST https://auth.kimi.com/api/oauth/device_authorization      # (kimi.ai: auth.kimi.ai)
  client_id=17e5f671-d194-4dfb-9706-5516cb48c098
POST https://auth.kimi.com/api/oauth/token
  client_id=17e5f671-...&device_code=...&grant_type=urn:ietf:params:oauth:grant-type:device_code
  # 200 + {"error":"authorization_pending"} keeps polling (not a 4xx!)
  # also: slow_down / expired_token / access_denied
BASE   https://api.kimi.com/coding     (kimi.ai: https://api.kimi.ai/coding)
HEADER X-Msh-Platform: CLIProxyAPI / X-Msh-Device-Id / X-Msh-Version / X-Msh-Device-Model
```

Why later: clean device flow, but the upstream is a **coding-specific proprietary API** (needs
a transport/format adapter), and the free allowance is unverified — it looks plan-driven like
Codex. Revisit after Codex lands, since the device-flow plumbing is shared.

Evidence: <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/auth/kimi/kimi.go>
(VERIFIED).

---

### 2.6 Kiro (AWS) — **later, high risk**

```text
# OIDC client + device authorization (per-account registration!)
POST https://oidc.{region}.amazonaws.com/client/register            # -> clientId, clientSecret
POST https://oidc.{region}.amazonaws.com/device_authorization
POST https://oidc.{region}.amazonaws.com/token                       # device_code / refresh_token
# Social login (Google/GitHub) uses Kiro's own social-auth refresh endpoint instead.

# Inference (proprietary, JSON-RPC style, NOT OpenAI)
POST https://codewhisperer.{region}.amazonaws.com/                   # x-amz-target: ...
POST https://q.{region}.amazonaws.com/                               # Amazon Q (no JSON-RPC)
HEADERS tokentype: API_KEY | ...   (API-key mode), profileArn: arn:aws:codewhisperer:...

# On-disk credentials the IDE writes
%USERPROFILE%\.aws\sso\cache\kiro-auth-token.json
{accessToken, refreshToken:"aorAAAAAG...", profileArn, authMethod:"builder-id"|"social"|"idc",
 clientId, clientSecret, clientIdHash, region, startUrl}
```

Why not now: three independent hazards. (1) **Each OIDC client registration supports one live
session** — importing a refresh token without calling `registerClient` first invalidates the
user's other devices (this is why OmniRoute re-registers per connection). (2) Public OIDC
clients **expire (~90 days)**, forcing re-auth. (3) The upstream is AWS JSON-RPC with a
`profileArn` and credit accounting, i.e. real adapter work. On top of that, AWS's acceptable-use
terms are the least friendly in this list toward third-party proxying.

Free tier: "KIRO FREE" credit bucket (historically 500 credits on signup) — **LIKELY**, changes
without notice; query it via the CodeWhisperer usage endpoint rather than assuming.

Evidence: <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/v6.8.46-0/internal/auth/kiro/aws.go>
(VERIFIED — endpoints, token file schema, region logic); CLIProxyAPI **v8 dropped the kiro
package** (VERIFIED by directory listing) while AIClient-2-API still ships `claude-kiro-oauth`
(VERIFIED); <https://raw.githubusercontent.com/diegosouzapw/OmniRoute/976d670ff3a7712df0c695f13095c43eace5e29b/docs/guides/KIRO_SETUP.md>
(VERIFIED — OIDC single-session problem, 90-day expiry, API-key mode).

---

### 2.7 Claude Code / Anthropic — **later / skip (breaking, hostile)**

```text
GET https://claude.ai/oauth/authorize
  ?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e
  &response_type=code&redirect_uri=http://localhost:54545/callback
  &scope=user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload
  &code_challenge=<S256>&code_challenge_method=S256&state=...

POST https://platform.claude.com/v1/oauth/token      # NOT api.anthropic.com (Claude Code 2.1.220+)
  {"grant_type":"authorization_code","code":"...","redirect_uri":"http://localhost:54545/callback",
   "client_id":"9d1c250a-...","code_verifier":"...","state":"..."}
GET  https://api.anthropic.com/api/oauth/profile
GET  https://api.anthropic.com/api/oauth/claude_cli/roles
```

**DEAD as a "just reuse the oat token" play.** Since ~2026-02-20 Anthropic rejects
`sk-ant-oat01-*` tokens for API access: `x-api-key` → `invalid x-api-key`, and
`Authorization: Bearer` → **`OAuth authentication is currently not supported.`** Only console
`sk-ant-api03-*` keys with separate billing work. The surviving path is the Claude Code client
path (`platform.claude.com` token endpoint + `claude_cli` role/session endpoints), which
requires **Firefox-fingerprinted TLS (uTLS) to defeat Cloudflare** plus a generated device-ID
pool, and breaks on every Claude Code release.

Verdict: skip unless a user explicitly asks. Effort high, ToS exposure high (Anthropic has
already shown willingness to break it), and no free tier to justify the risk. Re-evaluate only
if a stable third-party implementation survives >2 releases.

Evidence: <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/auth/claude/anthropic_auth.go>
(VERIFIED — endpoints, client id, scopes, uTLS transport); <https://github.com/anthropics/claude-code/issues/28091>
(VERIFIED — the Feb-2026 rejection, closed as duplicate).

---

### 2.8 Gemini CLI — **later; overlaps Antigravity**

```text
GET https://accounts.google.com/o/oauth2/v2/auth
  ?client_id=<GOOGLE_OAUTH_CLIENT_ID>
  &redirect_uri=http://127.0.0.1:<ephemeral>/oauth2callback
  &response_type=code&access_type=offline
  &scope=cloud-platform userinfo.email userinfo.profile&state=...
# headless variant: same client, redirect_uri=https://codeassist.google.com/authcode,
# then paste the code back (no loopback listener needed)

# client secret shipped in the CLI (installed-app client, explicitly non-secret):
<GOOGLE_OAUTH_CLIENT_SECRET>

UPSTREAM Cloud Code Assist  (cloudcode-pa.googleapis.com / v1internal:generateContent)
         -> NOT OpenAI-compatible; needs a Gemini-format adapter
CREDS    ~/.gemini/oauth_creds.json
```

Useful but redundant **if** Antigravity already ships: both draw on the same Google account's
free Gemini allowance, and Antigravity gives more (Gemini + Claude-via-Google) with the same
login. Keep Gemini CLI on the "later" list as a *fallback* Google channel for users who never
installed Antigravity, not as a parallel quota.

Evidence: <https://raw.githubusercontent.com/google-gemini/gemini-cli/main/packages/core/src/code_assist/oauth2.ts>
(VERIFIED — client id/secret, loopback + user-code flows, scopes, creds path).

---

### 2.9 Cursor / Windsurf / Zed / Void — **skip (client-only / proprietary)**

These have no reusable OAuth. In all four the "login" mints a token that is bound to the
client's own attestation (machine id, install signature, version headers), and the inference
endpoint is a private API, not a documented public one.

```text
Cursor      auth via api2.cursor.sh / cursor.sh session; private Connect/gRPC protocol;
            background agents & tab models require the shipped client's attestation.
Windsurf    api.codeium.com register-user -> session token; proprietary GetChatMessage
            protocol (protobuf); token tied to plugin install.
Zed         hosted models require a Zed account+plan; otherwise you supply OpenAI/Anthropic
            keys. No third-party OAuth surface at all.
Void        open-source Cursor fork; BYO API keys (no vendor quota to reuse).
```

Verdict: **skip**. Also ethically/legally the worst category: no public client, so any reuse
requires extracting secrets from a signed binary.

Evidence: **LIKELY** (consistent across proxy ecosystems; Cursor/Windsurf are absent from
CLIProxyAPI's OAuth provider list and from AIClient-2-API's provider matrix).

---

### 2.10 API-key-only vendors (no OAuth quota) — **skip as OAuth, add as key channels**

```text
Z.ai GLM Coding Plan   Anthropic: https://api.z.ai/api/anthropic
                       OpenAI chat: https://api.z.ai/api/coding/paas/v4
                       OpenAI responses: https://api.z.ai/api/v1
                       auth: plan API key (separate key namespace per product)
Moonshot Kimi (API)    https://api.moonshot.cn/v1  (OpenAI-compatible, key)
DeepSeek               https://api.deepseek.com/v1 (OpenAI-compatible, key)
Cerebras               https://api.cerebras.ai/v1  (OpenAI-compatible, key; free rate tier)
Mistral                https://api.mistral.ai/v1   (OpenAI-compatible, key; free tier)
```

Z.ai is worth noting because it is *deliberately* key-based: the Coding Plan key is restricted
to a whitelist of tools and endpoints (Anthropic + OpenAI shapes), so there is no OAuth to
reuse — but adding it as an **API-key provider** in our gateway costs almost nothing and gives
a cheap Claude-Code-compatible channel. Same for Kimi's API-key path once its OAuth (§2.5)
lands.

Evidence: <https://docs.z.ai/devpack/quick-start> (VERIFIED — endpoints + key model).

---

### 2.11 2025–2026 entrants: which are providers, which are gateways

| Entrant | Real OAuth free quota? | What it actually is |
| --- | --- | --- |
| **Trae** | yes (already integrated here) | vendor IDE with own quota |
| **Qoder** | yes (already integrated here) | vendor IDE with own quota |
| **MiniMax Code** | yes (PKCE device flow, already integrated) | vendor CLI |
| **Kimi Code** | yes, device flow (§2.5) | vendor CLI + coding API |
| **iFlow** | yes, phone OAuth (§2.4) | **gateway** in front of GLM/DeepSeek/Kimi/Qwen, but with its *own* free allowance |
| **Grok Build (xAI)** | promo quota, OAuth | vendor CLI (in progress here) |
| **Antigravity** | gated (Pro/Ultra) | vendor IDE (in progress here) |
| **Devin CLI** | no free tier | vendor CLI, `--devin-login` |
| **Meta Muse Code** | unverified | vendor CLI, "Meta login" channel just appeared in CLIProxyAPI |
| **OpenCode Zen** | "free" models, **key required**, anon = 403 | **gateway** (own billing, revocable free models) |
| **Kilo Code / Cline / Roo Code** | no | **BYOK editors** — they consume our gateway, they are not providers |
| **Pollinations** | small anon allowance | **gateway** (aggregator), quality too low for coding |
| **OpenCode / Crush / DSH** | n/a | **clients** — targets for our "one-click configure" feature |

OpenCode Zen deserves one number, because it is the most commonly cited "free OAuth-ish"
option and it is **not** OAuth: measured 2026-10, every free model (`space-bunny-free`,
`jev-1.13-free`, `big-pickle`, `mimo-*`, `ling-*`, `nemotron-*`, `longcat-*`, `fledge-*`,
`muse-spark-*-contributor-free`) returns `403 FreeTierError` for anonymous traffic; a Bearer
key from `opencode.ai/auth` is mandatory, and replaying the client's `x-opencode-*` headers
does **not** lift the 403. It is a key-based gateway with a free-model catalog
(`https://opencode.ai/zen/v1`), so it belongs in the "API-key provider" bucket, not this
catalog's OAuth bucket. Evidence: <https://pypi.org/project/zen-free-proxy/> (VERIFIED
measurement write-up).

---

## 3. How the leading multi-provider proxies structure this

### CLIProxyAPI (`router-for-me/CLIProxyAPI`) — the reference implementation

<https://github.com/router-for-me/CLIProxyAPI> · MIT · Go · OpenAI + Gemini + Claude + Grok
compatible front ends; OAuth credentials live as files under `auth-dir`, managed via
Management API (`POST /v0/management/oauth/{provider}` → `auth_url`, then `/callback`).

Architecture worth copying: one `internal/auth/<provider>` package per OAuth provider (URL
builder, PKCE, token exchange, refresh with single-flight + backoff, token storage,
filename/email derivation), one translator per upstream protocol, and a credential pool with
round-robin + per-credential model aliases/exclusions (`oauth-model-alias`,
`oauth-excluded-models`).

Provider tiers, **as of the v8 source tree** (this is the honest stability signal):

```text
internal/auth/ on main (v8):  antigravity  claude  codex  devin  kimi  meta  vertex  xai
internal/auth/ on v6:         ... + kiro  (+ gemini/gemini-cli paths elsewhere)
docs still advertise:         gemini-cli, vertex, aistudio, antigravity, claude, codex,
                              qwen, iflow, kimi
```

So: **stable first-party OAuth** = gemini-cli/vertex/aistudio/antigravity, claude, codex,
kimi, xai; **moved/dropped** = kiro, qwen, iflow, copilot (docs–code drift; treat as
experimental/plugin tier). Newcomers `devin` and `meta` exist as auth packages but are
unproven.

### AIClient-2-API (`justlovemaki/AIClient-2-API`) — closest to our problem

<https://github.com/justlovemaki/AIClient-2-API> · **GPL-3.0** (⚠ viral — cannot be vendored
into this MIT repo) · Node.js · "client-only APIs → local OpenAI-compatible interface".

Provider types it ships: `gemini-antigravity`, `claude-kiro-oauth`, `codex` (OAuth),
`grok-cli-oauth` + `grok-web` (cookie/SSO), `kimi`, `minimax`, plus `openai-custom`/
`claude-custom` key types. Notable engineering we should mirror:

- fixed OAuth callback ports (Antigravity 8086, Codex 1455, Grok CLI 56121, Kiro 19876-19880);
- **TLS sidecar** (Go uTLS) to defeat JA3/JA4 fingerprint blocking on Grok/Cloudflare;
- account pool with health checks, 429 cooldown, and **cross-type fallback chains**
  (`claude-kiro-oauth → claude-custom → openai-custom`);
- `notSupportedModels` per account (per-seat model gating).

Because of GPL-3.0, use it as a **specification**, never as a source of code.

### new-api / one-api — different class

<https://github.com/QuantumNous/new-api> · <https://github.com/songquanpeng/one-api> ·
API-key aggregation + billing/quota + multi-tenant front end (OpenAI-shaped). They do **not**
implement vendor OAuth for coding agents; they are what you point at *after* an OAuth proxy
(or at raw vendor keys). Useful as a UI/billing reference only.

### grok2api — single-provider web-session proxy

<https://github.com/Tomiya233/grok2api_new> · turns the Grok **web** session (cookie/SSO) into
an OpenAI-compatible API with multi-account rotation. Structurally the same trick as our
WorkBuddy integration (web session, not OAuth), and a good reference for the cookie-import UX
if Grok's OAuth path ever regresses. Not a multi-provider design.

### antigravity-manager (`lbjlaq/Antigravity-Manager`) — single-provider, but best UX reference

<https://github.com/lbjlaq/Antigravity-Manager> · **CC BY-NC-SA 4.0** (⚠ non-commercial —
do not copy code) · Tauri/Rust, Antigravity accounts → OpenAI + Anthropic + Gemini triple
front end. Notable: quota-per-model dashboards, 403/captcha detection with an "appeal" action
link, tiered routing (Ultra/Pro/Free), silent downgrade of background tasks to Flash to save
premium quota, and JSON-Schema flattening to avoid upstream 400s. All of that is behaviour we
will eventually need for Antigravity, so read it before designing our Antigravity adapter.

### Ports / forks worth watching (cheap secondary evidence)

`OmniRoute` (has the best Kiro write-up), `9Router` (Next.js CLIProxyAPI re-implementation),
`vibeproxy`/`Quotio`/`ProxyPilot` (menu-bar wrappers; they enumerate exactly which providers
users consider viable: Claude, Codex, Gemini, Antigravity, Grok, Kimi, Copilot, Kiro, Cursor,
Trae, GLM), `cliproxy-rs` (Rust port, same config format), `sub2api` (Cloudflare Workers).

What this tells us about **stability tiers** in practice:

```text
Tier A (boring, multi-implementation, survives releases):
    Codex OAuth · Gemini CLI/Antigravity (Google) · Qwen · Kimi · GitHub Copilot
Tier B (works, but one bad release away from breakage):
    Claude OAuth · Grok (OAuth) · iFlow · Kiro
Tier C (single-source, unproven, or region-locked):
    Devin · Meta Muse · Pollinations-class gateways
```

---

## 4. Recommended implementation order — next 3 after Antigravity and Grok

Assumption: Antigravity and Grok land first; the adapter contract in `gateway/` is
`auth(pkce/device) → token store → open(path, body, headers)` + `models()` + `status()`, with
a private loopback listener and a per-provider account pool.

**1. GitHub Copilot** — *lowest cost per added model family*
- Reuses the MiniMax device-flow + account-pool code almost unchanged; **no PKCE**, no
  browser listener at all (device code only) → no new OAuth surface to test.
- Upstream is literally OpenAI `chat/completions`, so **zero translator work**; `/models`
  gives capability limits we can feed into the existing catalog (`wb_modelsdev.py`).
- One seat covers GPT + Claude + Gemini families, which immediately raises the gateway's
  "one account, many families" value.
- Only real work: 20-minute JWT refresh loop, mandatory header set, seat-type base URL, and a
  per-account concurrency cap to stay politely inside Copilot ToS.
- Do it first because it is the fastest path to a *second* multi-family provider and it
  exercises device flow (shared with Qwen/Kimi) with the simplest possible protocol.

**2. OpenAI Codex** — *highest quota value, and the translator is reusable*
- The flow is public and stable; the expense is the Responses-API translator + SSE event
  mapping, which is the same work needed later for xAI/Meta — so it is an investment, not a
  one-off.
- Adds the 5h/weekly quota model, which we need anyway for a credible dashboard (Codex is the
  reference implementation of "quota windows" in every proxy).
- Must-haves from day one: single-flight refresh (`refresh_token_reused` kills accounts),
  account-id extraction from `id_token`, and per-plan model probing.
- Second, not first, purely because of translator effort — it is the bigger strategic win.

**3. Qwen Code** — *the free tier that makes the gateway demo-able*
- Device flow again (so the flow engine is already proven by Copilot), OpenAI-compatible
  upstream (no translator), and ~2,000 requests/day free per account.
- Gives us a genuinely free, non-trial provider — different risk profile from every
  subscription provider above, and the first one where multi-account aggregation is *the*
  feature rather than an optimisation.
- Watch: single-session credential semantics; import-or-run, never both.

**Deferred, with trigger conditions:**
- **iFlow** — implement next if CN-market demand exists (phone-login only). Cheap: MIT Python
  reference can be vendored like Trae/Qoder.
- **Kimi Code** — after Codex, since it shares device flow but needs a proprietary adapter and
  its free allowance is unproven.
- **Kiro** — only on explicit user request; OIDC session-invalidation + 90-day client expiry +
  AWS terms make it a support burden.
- **Claude** — only if a third-party implementation survives two Claude Code releases; today
  it is uTLS-vs-Cloudflare maintenance with no free tier.
- **Gemini CLI** — only if Antigravity shipping slips; otherwise redundant Google quota.

Ordering rationale in one line: **maximise (new model families × accounts) per unit of new
protocol code** — Copilot adds three families with no translator, Codex adds the best quota
with a reusable translator, Qwen adds free capacity with no translator.

---

## 5. Cross-cutting risks and guardrails (apply to every provider above)

1. **Refresh single-flight is not optional.** Codex (`refresh_token_reused`), Kiro (one live
   session per OIDC client), Qwen and Claude all invalidate on concurrent refresh. Every
   adapter needs a per-credential mutex + persisted "blocked until" on 429.
2. **Fingerprint, don't just header.** Cloudflare/JA3 checks bite Claude (needs Firefox uTLS),
   Grok (needs TLS sidecar), iFlow (needs `User-Agent: iFlow-Cli`). Budget for a
   `wb_fingerprint.py`-style module, which this repo already has.
3. **Quota must be read, not assumed.** Codex (5h/weekly), Copilot (`copilot_internal/user`),
   Kiro (CodeWhisperer usage), Antigravity (per-model quota) all expose it; surfacing it is
   the difference between "mysterious 429" and a usable dashboard.
4. **Per-account model gating.** Seats differ (Copilot plan, Codex plan, Kiro free vs pro,
   Antigravity Pro/Ultra). Store `notSupportedModels` per account like AIClient-2-API.
5. **Legal posture.** Every entry is "user's own credentials, user's own quota, local proxy".
   Do not add account marketplaces, credential resale, or shared pools across users — that is
   the line that turns a tolerated tool into a ToS violation. Antigravity-Manager's
   non-commercial licence and AIClient-2-API's GPL are also hard constraints on code reuse.
6. **Doc/code drift is the norm.** CLIProxyAPI's docs list `qwen`/`iflow` while v8's source no
   longer has those packages; Copilot and Kiro moved out. Pin upstream commits in
   `gateway/SOURCES.md` and re-verify endpoints at integration time, never at doc-reading time.

---

## 6. Open items to verify before coding (highest value first)

1. Copilot: current plan/entitlement matrix and whether `/chat/completions` still accepts the
   `copilot_internal/v2/token` JWT for *all* model families on a Free seat. (**LIKELY**, not
   re-read today.)
2. Copilot: current `editor-version` / `copilot-chat` values (hardcoded ones rot; derive from
   the released VS Code + Copilot Chat versions at build time).
3. Qwen: whether the 2,000/day free allowance still applies to newly created accounts, and
   whether `portal.qwen.ai/v1` model aliases (`coder-model`, `vision-model`) are stable.
4. Kimi: whether a free/trial allowance exists at all, and the exact `X-Msh-*` header set
   required by the coding endpoint.
5. Kiro: current free-credit size and whether `registerClient` is still open to third-party
   clients.
6. Meta Muse / Devin: whether either exposes a reusable public OAuth client (they are listed
   by CLIProxyAPI but not documented here) — do not schedule work before this is answered.

---

## Appendix: evidence index

| Source | Used for |
| --- | --- |
| <https://github.com/cecil-the-coder/ai-provider-kit/blob/main/COPILOT_API.md> | Copilot device flow, token exchange, headers, endpoints |
| <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/auth/codex/openai_auth.go> | Codex OAuth constants, PKCE, refresh, id_token claims |
| <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/auth/claude/anthropic_auth.go> | Claude endpoints, client id, scopes, uTLS requirement |
| <https://github.com/anthropics/claude-code/issues/28091> | Claude `sk-ant-oat*` rejection (Feb 2026) |
| <https://raw.githubusercontent.com/QwenLM/qwen-code/main/packages/core/src/qwen/qwenOAuth2.ts> | Qwen device flow, client id, PKCE, poll semantics |
| <https://openclawdoc.org/providers/qwen> | Qwen free 2,000/day, `portal.qwen.ai/v1`, model ids |
| <https://raw.githubusercontent.com/cacaview/iflow2api/main/iflow2api/oauth.py> | iFlow OAuth endpoints, client id/secret, UA gating |
| <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/auth/kimi/kimi.go> | Kimi device flow, client id, coding base URL, headers |
| <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/v6.8.46-0/internal/auth/kiro/aws.go> | Kiro endpoints, token file schema, regions |
| <https://raw.githubusercontent.com/diegosouzapw/OmniRoute/976d670ff3a7712df0c695f13095c43eace5e29b/docs/guides/KIRO_SETUP.md> | Kiro OIDC single-session, 90-day expiry, API-key mode |
| <https://raw.githubusercontent.com/google-gemini/gemini-cli/main/packages/core/src/code_assist/oauth2.ts> | Gemini CLI client id/secret, loopback + user-code flows |
| <https://docs.z.ai/devpack/quick-start> | GLM Coding Plan key-only endpoints |
| <https://pypi.org/project/zen-free-proxy/> | OpenCode Zen free-tier reality (key required, anon 403) |
| <https://github.com/router-for-me/CLIProxyAPI> + <https://mintlify.wiki/router-for-me/CLIProxyAPI/configuration/oauth-providers.md> | OAuth provider matrix, Management API, model alias/exclusion design |
| <https://github.com/justlovemaki/AIClient-2-API> | Provider types, callback ports, TLS sidecar, fallback chains, GPL-3.0 |
| <https://github.com/lbjlaq/Antigravity-Manager> | Antigravity UX/quota/403 handling reference, CC BY-NC-SA |
| <https://github.com/Tomiya233/grok2api_new> | Web-session (cookie/SSO) variant of the same trick |
| `gateway/SOURCES.md`, `gateway/qoder.py` in this repo | Existing adapter contract and vendoring conventions |
