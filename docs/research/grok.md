# xAI Grok / Grok CLI (Grok Build) — OAuth & Upstream Inference Reference

**Purpose:** hard technical facts for a self-contained Python reverse proxy that reuses a Grok
OAuth login instead of an `xAI-...` API key.
**Research date:** 2026-10-18.
**Confidence legend:**

| Marker | Meaning |
| --- | --- |
| **VERIFIED** | Stated by an xAI primary source (docs.x.ai, auth.x.ai discovery document, console) or observed directly as a live HTTP response. |
| **LIKELY** | Consistent across ≥2 independent open-source implementations that ship the behaviour; not documented by xAI. |
| **UNVERIFIED** | Single source, inferred, or contested between implementations. Do not build on it without probing. |

> **Bottom line (read this first).** xAI *does* run a real OAuth 2.0 authorization-code + PKCE and
> device-code server at `auth.x.ai`, and the resulting bearer token *is* accepted for inference — but
> **only on the Responses API surface**, and the token is routed to **one of two different hosts**
> depending on an undocumented `tier` claim inside the JWT: the public API (`api.x.ai/v1/responses`)
> for entitled accounts, or the Grok CLI internal proxy (`cli-chat-proxy.grok.com/v1/responses`)
> otherwise. There is **no** usable `/v1/chat/completions` or `/v1/messages` on the CLI-proxy surface,
> so an OpenAI-Chat-shaped proxy must translate to the Responses schema, not just retarget a base URL.

---

## 1. Is there an OAuth flow for Grok clients?

**Yes.** xAI operates an OIDC/OAuth 2.0 authorization server at `https://auth.x.ai` that serves the
Grok CLI (`grok`, product name **Grok Build**) and is also reachable from third-party clients.
The discovery document is public and live. **VERIFIED**

Source: <https://auth.x.ai/.well-known/openid-configuration> (fetched 2026-10-18)

```json
{
  "issuer": "https://auth.x.ai",
  "authorization_endpoint": "https://auth.x.ai/oauth2/authorize",
  "device_authorization_endpoint": "https://auth.x.ai/oauth2/device/code",
  "token_endpoint": "https://auth.x.ai/oauth2/token",
  "userinfo_endpoint": "https://auth.x.ai/oauth2/userinfo",
  "revocation_endpoint": "https://auth.x.ai/oauth2/revoke",
  "jwks_uri": "https://auth.x.ai/.well-known/jwks.json",
  "response_types_supported": ["code"],
  "subject_types_supported": ["public"],
  "id_token_signing_alg_values_supported": ["ES256"],
  "scopes_supported": ["openid","profile","email","offline_access","grok-cli:access",
    "grok-workspace:serve","team:read","teams:read","teams:write","org:read","orgs:read",
    "orgs:write","api:access","grok-plugins:access","conversations:read",
    "conversations:write","workspaces:read","workspaces:write","api-keys:read",
    "api-keys:write","logs:read","billing:read","billing:write"],
  "token_endpoint_auth_methods_supported": ["client_secret_basic","client_secret_post","none"],
  "claims_supported": ["sub","iss","aud","exp","iat","name","given_name","family_name",
    "email","email_verified","picture"],
  "grant_types_supported": ["authorization_code","refresh_token",
    "urn:ietf:params:oauth:grant-type:device_code"],
  "code_challenge_methods_supported": ["S256"],
  "request_parameter_supported": false,
  "request_uri_parameter_supported": false
}
```

Consequences that matter for a proxy:

* `token_endpoint_auth_methods_supported` includes `none` → **public client, no client secret**.
* `grant_types_supported` includes device code → **a headless device-code login is possible**
  (no localhost listener, no SSH tunnel).
* `code_challenge_methods_supported: ["S256"]` → PKCE S256 only (no `plain`).
* `id_token_signing_alg_values_supported: ["ES256"]` → JWTs are ES256; treat `aud`/`iss` accordingly.
* No `client_credentials`, no `password`, no token-exchange grant.

### 1.1 Client identity (client_id)

**LIKELY** — identical value in every implementation surveyed; xAI does not publish it.

```text
b1a00492-073a-47ea-816f-4c329264a828
```

This is described in the sources as the *public desktop/CLI client id of the official Grok CLI*
("not a client secret"). It is hardcoded, not registered per-app.

| Source | Constant / line | URL |
| --- | --- | --- |
| CLIProxyAPI `internal/auth/xai/types.go` | `ClientID = "b1a00492-073a-47ea-816f-4c329264a828"` | <https://github.com/router-for-me/CLIProxyAPI/blob/v7.1.60/internal/auth/xai/types.go> |
| ZeroClaw `crates/zeroclaw-providers/src/auth/xai_oauth.rs` | `pub const XAI_OAUTH_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";` | <https://github.com/zeroclaw-labs/zeroclaw/blob/21fbd953/crates/zeroclaw-providers/src/auth/xai_oauth.rs> |
| Hermes Agent `hermes_cli/auth.py` | `XAI_OAUTH_CLIENT_ID = "b1a00492-073a-47ea-816f-4c329264a828"` | <https://github.com/NousResearch/hermes-agent/commit/3a503f00a60a413cf5f8a5310cf332b19302ef66> |
| 9router `open-sse/providers/registry/grok-cli.js` | `clientId: "b1a00492-073a-47ea-816f-4c329264a828"` | <https://github.com/fzrilsh/bercocok-tanam/blob/main/docs/GROK-CLI-OAUTH-ANALYSIS.md> |
| OmniRoute `open-sse/config/providers/registry/grok-cli/index.ts` | `clientIdDefault: resolvePublicCred("grok_id", ...)` (same value, obfuscated) | <https://github.com/diegosouzapw/OmniRoute/commit/0adcb9097479acefb56a4f14c5d01f016868a1c8> |

### 1.2 Scopes

Two variants are in the wild. The **narrow** one is what Grok CLI / CLIProxyAPI / ZeroClaw send; the
**wide** one adds conversation history scopes (9router and OmniRoute send it for the device flow).

**LIKELY** (narrow — recommended default):

```text
openid profile email offline_access grok-cli:access api:access
```

**LIKELY** (wide — used by device-code implementations that also read conversation history):

```text
openid profile email offline_access grok-cli:access api:access conversations:read conversations:write
```

* `grok-cli:access` is the scope that authorises the *Grok Build / CLI* product surface.
* `api:access` is the scope that nominally authorises the *public inference API*. **Having the scope
  is not the same as being entitled** — see §6 and the 403/402 behaviour in §4.4.
* `offline_access` is required to receive a `refresh_token`.

### 1.3 Redirect URI and loopback port

**LIKELY** — the official client binds a fixed loopback port, which is the only redirect registered
for this client id:

```text
http://127.0.0.1:56121/callback
```

| Source | Detail |
| --- | --- |
| CLIProxyAPI `internal/auth/xai/types.go` | `RedirectHost = "127.0.0.1"`, `CallbackPort = 56121`, `RedirectPath = "/callback"` — <https://github.com/router-for-me/CLIProxyAPI/blob/v7.1.60/internal/auth/xai/types.go> |
| ZeroClaw | `pub const XAI_OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";` and `TcpListener::bind("127.0.0.1:56121")` — <https://github.com/zeroclaw-labs/zeroclaw/blob/21fbd953/crates/zeroclaw-providers/src/auth/xai_oauth.rs> |

> Practical warning (**LIKELY**): implementations that invent a random port get a redirect-URI
> rejection. Hermes Agent tries `127.0.0.1:{8765 + pid % 800}/xai/callback` and its own source says
> *"Browser login uses xAI's public desktop client and may fail due to redirect URI restrictions …
> For the most reliable experience, use the official Grok CLI (`grok login`) and let Hermes
> auto-import."* (<https://github.com/NousResearch/hermes-agent/commit/3a503f00a60a413cf5f8a5310cf332b19302ef66>).
> For a proxy product, prefer the **device-code flow** (no redirect at all) over a fabricated
> loopback port.

### 1.4 Authorization request (browser + PKCE, S256)

**LIKELY** — assembled from ZeroClaw and CLIProxyAPI:

```text
GET https://auth.x.ai/oauth2/authorize
  ?response_type=code
  &client_id=b1a00492-073a-47ea-816f-4c329264a828
  &redirect_uri=http%3A%2F%2F127.0.0.1%3A56121%2Fcallback
  &scope=openid%20profile%20email%20offline_access%20grok-cli%3Aaccess%20api%3Aaccess
  &state=<random>
  &nonce=<random>
  &code_challenge=<BASE64URL(SHA256(verifier))>
  &code_challenge_method=S256
  &plan=generic
  &referrer=<client-name>
```

Notes:

* `plan` and `referrer` are **non-standard xAI query parameters**. ZeroClaw sends `plan=generic` and
  `referrer=zeroclaw` (<https://github.com/zeroclaw-labs/zeroclaw/blob/21fbd953/crates/zeroclaw-providers/src/auth/xai_oauth.rs>);
  CLIProxyAPI sends `plan=generic` and `referrer=cli-proxy-api`
  (<https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/v7.1.60/internal/auth/xai/xai.go>).
  They appear to be telemetry/plan-hint only; they are **not** enforced.
* ZeroClaw's token exchange additionally re-sends `code_challenge` + `code_challenge_method` in the
  POST body, with the comment *"xAI's shared OAuth client validates these PKCE fields again at token
  exchange."* That is **UNVERIFIED** (single implementation); the standard `code_verifier` alone is
  what CLIProxyAPI sends and it works there. Sending both is harmless.

### 1.5 Device-code flow (recommended for a server-side proxy)

**VERIFIED** that the flow exists (discovery advertises `device_authorization_endpoint` and the
`urn:ietf:params:oauth:grant-type:device_code` grant). **LIKELY** for the exact request/response
shapes below.

**Step 1 — request a device code**

```http
POST https://auth.x.ai/oauth2/device/code
Content-Type: application/x-www-form-urlencoded

client_id=b1a00492-073a-47ea-816f-4c329264a828
&scope=openid profile email offline_access grok-cli:access api:access conversations:read conversations:write
&referrer=grok-build
```

Response (MUST be treated as opaque; `verification_uri` is **not** always `auth.x.ai`):

```json
{
  "device_code": "...",
  "user_code": "XXXX-XXXX",
  "verification_uri": "https://accounts.x.ai/oauth2/device",
  "verification_uri_complete": "https://accounts.x.ai/oauth2/device?user_code=XXXX-XXXX",
  "expires_in": 1800,
  "interval": 5
}
```

Sources for the response shape: 9router (<https://github.com/fzrilsh/bercocok-tanam/blob/main/docs/GROK-CLI-OAUTH-ANALYSIS.md>,
shows `expires_in: 600`, `verification_uri: https://auth.x.ai/activate`) and OmniRoute's unit test
(`verification_uri: https://accounts.x.ai/oauth2/device`, `expires_in: 1800`) —
<https://github.com/diegosouzapw/OmniRoute/commit/0adcb9097479acefb56a4f14c5d01f016868a1c8>.
**Do not hardcode the verification host or the TTL**; both have been observed to differ.

> **PKCE asymmetry (important).** For this client the *device* flow does **not** use PKCE, and sending
> a `code_verifier` at poll time causes `invalid_grant` / `Access denied`. 9router documents this as
> their exact production bug: the device-code request sent no `code_challenge`, but polling sent
> `code_verifier`, and xAI rejected it. Fix was to add `grok-cli` to a `noPkceProviders` list.
> <https://github.com/fzrilsh/bercocok-tanam/blob/main/docs/GROK-CLI-OAUTH-ANALYSIS.md> **LIKELY**
> (two independent implementations now omit the verifier:
> <https://github.com/diegosouzapw/OmniRoute/commit/0adcb9097479acefb56a4f14c5d01f016868a1c8>).

**Step 2 — poll the token endpoint**

```http
POST https://auth.x.ai/oauth2/token
Content-Type: application/x-www-form-urlencoded

grant_type=urn:ietf:params:oauth:grant-type:device_code
&device_code=<device_code>
&client_id=b1a00492-073a-47ea-816f-4c329264a828
```

Pending/terminal errors are RFC 8628 standard: `authorization_pending`, `slow_down` (back off +5 s),
`access_denied` / `authorization_denied`, `expired_token`. ZeroClaw implements exactly this state
machine (<https://github.com/zeroclaw-labs/zeroclaw/blob/21fbd953/crates/zeroclaw-providers/src/auth/xai_oauth.rs>).

**Step 3 — client identity headers during OAuth.** The official client sends version/surface headers
on the OAuth calls themselves; 9router sets
`User-Agent: grok-pager/0.2.93 grok-shell/0.2.93 (linux; x86_64)` on both the device-code and token
requests, and OmniRoute asserts in tests:

```text
x-grok-client-version: <current CLI version>
x-grok-client-surface: ui
```

**LIKELY** — see §5.

### 1.6 Alternative: import an existing `grok login` credential (zero-OAuth implementation)

**LIKELY**, and the most robust option for a first release. The official CLI persists its session to:

```text
~/.grok/auth.json        (Windows: %USERPROFILE%\.grok\auth.json)
```

Schema observed by two implementations — a **map keyed by `"{issuer}::{client_id}"`**:

```json
{
  "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {
    "key": "<access token JWT>",
    "refresh_token": "<refresh token>",
    "expires_at": "2026-10-18T12:34:56Z",
    "user_id": "...",
    "principal_id": "...",
    "email": "...",
    "oidc_issuer": "https://auth.x.ai",
    "oidc_client_id": "b1a00492-073a-47ea-816f-4c329264a828",
    "auth_mode": "oidc"
  }
}
```

* ZeroClaw parses exactly these fields (`key`, `refresh_token`, `expires_at`, `user_id`,
  `principal_id`, `email`, `oidc_issuer`, `oidc_client_id`) and computes the lookup key as
  `format!("{XAI_OAUTH_ISSUER}::{XAI_OAUTH_CLIENT_ID}")`.
  <https://github.com/zeroclaw-labs/zeroclaw/blob/21fbd953/crates/zeroclaw-providers/src/auth/xai_oauth.rs>
* Hermes reads the same file, accepting `key` **or** `access_token`, and falls back to matching
  `oidc_issuer` + `oidc_client_id` when the exact key is absent.
  <https://github.com/NousResearch/hermes-agent/commit/3a503f00a60a413cf5f8a5310cf332b19302ef66>
* OmniRoute's `mapTokens` accepts a raw `auth.json` blob and prefers the entry whose key matches the
  active client id (<https://github.com/diegosouzapw/OmniRoute/commit/0adcb9097479acefb56a4f14c5d01f016868a1c8>).

---

## 2. Token response shape and refresh semantics

### 2.1 Response

**LIKELY** — standard RFC 6749 body, parsed identically by all three implementations:

```json
{
  "access_token": "<ES256 JWT>",
  "refresh_token": "<opaque>",
  "id_token": "<ES256 JWT>",
  "token_type": "Bearer",
  "expires_in": 21600,
  "scope": "openid profile email offline_access grok-cli:access api:access"
}
```

* `expires_in` is **not stable**. 9router observed tokens dying at exactly +6 h
  (<https://github.com/fzrilsh/bercocok-tanam/blob/main/docs/GROK-CLI-OAUTH-ANALYSIS.md>, issues
  #2734/#2737); OmniRoute uses `21600` only as a *fallback when the field is missing*
  (`const expiresIn = data.expires_in || 21600`). **Always prefer `expires_in`, then the JWT `exp`.**

### 2.2 Access-token JWT claims (what a proxy can read without a userinfo call)

**LIKELY** — from OmniRoute's mapper and CLIProxyAPI's entitlement parser:

```json
{
  "sub": "user-123",
  "email": "user@example.com",
  "team_id": "team-456",
  "tier": 2,
  "principal_type": "Team",        // or "Organization" / personal
  "principal_id": "team-456",
  "exp": 1760000000
}
```

* `email` and `sub` are the identity pair (CLIProxyAPI `parseJWTIdentity` reads `id_token`, then
  `access_token`; ZeroClaw prefers `email` then `sub`).
* **`tier` is the single most important claim** — it decides which upstream host your token may use.
  See §3.2. CLIProxyAPI treats `tier > 0` (or `"4"`) as "prefers standard API", `tier == 0` or
  `"free"` as "CLI only", and *missing/unknown* as "fail closed to CLI".
  <https://patch-diff.githubusercontent.com/raw/router-for-me/CLIProxyAPI/pull/4206.diff>
* For team/org principals the real identifier is `principal_id`, and `x-email` is deliberately
  **omitted** on the wire (OmniRoute `getWireEmail`: `principalType === "team" || "organization"`
  ⇒ no `x-email`).

### 2.3 Refresh

**LIKELY**

```http
POST https://auth.x.ai/oauth2/token
Content-Type: application/x-www-form-urlencoded

grant_type=refresh_token
&client_id=b1a00492-073a-47ea-816f-4c329264a828
&refresh_token=<refresh_token>
```

Optional (OmniRoute sends these for team/org accounts; single source, **UNVERIFIED**):

```text
&principal_type=Team
&principal_id=team-456
```

Semantics to implement:

* **Rotation:** responses may contain a *new* `refresh_token`; persist it, but if absent keep the old
  one (`data.refresh_token || credentials.refreshToken` — OmniRoute, CLIProxyAPI, Hermes all do this).
* **Terminal vs transient:** `invalid_grant` and `invalid_client` are terminal → mark the credential
  dead and require re-login. 5xx (`temporarily_unavailable`) is transient → exponential backoff with
  jitter (OmniRoute: 3 attempts, 200 ms × 2^n, 0.5–1.5× jitter). **LIKELY**
* **Proactive refresh:** refresh ≈5 min before expiry (Hermes `XAI_ACCESS_TOKEN_REFRESH_SKEW_SECONDS = 300`;
  9router `refreshLeadMs: 300000`). Also refresh reactively on a 401.
* **No refresh token ⇒ re-login.** Each refresh failure that is terminal should be surfaced as a
  typed "re-authenticate" state, not retried in a loop.

---

## 3. Inference API surface reachable with an OAuth token

### 3.1 There are two upstream hosts, not one

| Host | Base | Nature | Auth |
| --- | --- | --- | --- |
| Public API | `https://api.x.ai/v1` | Documented, stable, OpenAI-compatible Responses + Chat Completions + media | API key **or** entitled OAuth token |
| Grok CLI proxy | `https://cli-chat-proxy.grok.com/v1` | Undocumented internal proxy used by Grok Build / `grok` | OAuth token only |

**VERIFIED** for `api.x.ai` (docs). **LIKELY** for `cli-chat-proxy.grok.com/v1` — three independent
implementations use it:

```text
# OmniRoute open-sse/config/grokBuild.ts
GROK_BUILD_PROXY_BASE_URL  = "https://cli-chat-proxy.grok.com/v1"
GROK_BUILD_RESPONSES_URL   = `${GROK_BUILD_PROXY_BASE_URL}/responses`
GROK_BUILD_MODELS_URL      = `${GROK_BUILD_PROXY_BASE_URL}/models`
```
<https://github.com/diegosouzapw/OmniRoute/commit/0adcb9097479acefb56a4f14c5d01f016868a1c8>

```go
// CLIProxyAPI internal/auth/xai/types.go (after PR #4206)
DefaultAPIBaseURL    = "https://api.x.ai/v1"
CLIChatProxyBaseURL  = "https://cli-chat-proxy.grok.com/v1"
```
<https://patch-diff.githubusercontent.com/raw/router-for-me/CLIProxyAPI/pull/4206.diff>

grok2api states it plainly: *"`cli-chat-proxy.grok.com` 是 Grok Build 产品上游，不是公开、长期稳定的第三方
API 契约"* (it is the Grok Build product upstream, **not** a public or long-term-stable third-party API
contract) — <https://github.com/chenyme/grok2api/blob/449b95fa/backend/docs/RESPONSES_COMPATIBILITY.md>.

### 3.2 Routing rule: the `tier` claim decides the host

**LIKELY** — this is the single most consequential implementation detail found, from CLIProxyAPI PR
#4206 *"fix(xai): route OAuth requests by access tier"*:

```go
// internal/auth/xai/entitlement.go
FreeOAuthModel      = "grok-4.5"        // model exposed by the Grok CLI free entitlement
ComposerModelPrefix = "grok-composer-"  // models served only by the Grok CLI proxy
```

Resolution order (verbatim behaviour):

1. A **custom** `base_url` supplied by the operator wins.
2. OAuth + model starting with `grok-composer-` → **CLI proxy** (these models exist only there).
3. Explicit `using_api` credential flag → override.
4. OAuth token with `tier > 0` → **`api.x.ai`**, and if that returns 402/403-entitlement, retry
   **once** through the CLI proxy — but only for model `grok-4.5`.
5. OAuth token with `tier == 0` / `"free"` → **CLI proxy**.
6. `tier` missing/unparseable → **CLI proxy** (fail closed).

Entitlement-fallback trigger is narrow: HTTP `402` always; HTTP `403` only when the body contains one
of `billing, payment, subscription, entitlement, insufficient credit, credit balance, credits
required, api tier, tier_required, tier-gated, paid plan, plan access, upgrade your plan, supergrok` —
and never when it contains `blocked, policy, safety, abuse, mtls, certificate`.

Also (same PR): the CLI proxy accepts **HTTP POST only** — *"Websocket transport intentionally does
not use this helper because the CLI chat proxy accepts HTTP POST only"* (405 on upgrade). So a
WebSocket/Responses-WS path is `api.x.ai`-only.

### 3.3 Endpoint inventory

| Path | api.x.ai | cli-chat-proxy | Notes |
| --- | --- | --- | --- |
| `POST /v1/responses` | **VERIFIED** | **LIKELY** | The only text path that matters for OAuth |
| `POST /v1/responses/compact` | **LIKELY** | **LIKELY** | Forces non-stream; used for context compaction |
| `GET /v1/responses/{id}` | **VERIFIED** | **LIKELY** | Needs server-side storage / account stickiness |
| `DELETE /v1/responses/{id}` | **LIKELY** | **LIKELY** | |
| `GET /v1/models` | **LIKELY** | **LIKELY** | CLI proxy: `.../v1/models`; used for capability discovery |
| `POST /v1/chat/completions` | **VERIFIED** (API key); OAuth **UNVERIFIED** | **not native** | Deprecated by xAI; see §7 |
| `POST /v1/messages` (Anthropic) | **VERIFIED** but deprecated | absent | see §7 |
| `POST /v1/images/generations`, `/v1/videos/generations` | **VERIFIED** | media via api.x.ai only | CLI proxy is text-only |
| `GET /billing`, `GET /billing?format=credits` | — | **LIKELY** | Grok Build quota introspection |

xAI's own docs push everything to `/v1/responses`: *"The Responses API is the recommended way to
interact with SpaceXSI models"*, and Chat Completions is labelled **Deprecated** with *"no reasoning
content returned"*, *"function calling only"*, *"limited updates"* —
<https://docs.x.ai/developers/model-capabilities/text/comparison.md>.

`billing` endpoints (undocumented, observed on Grok Build 0.2.93): `GET /billing` and
`GET /billing?format=credits` return `monthlyLimit`, `used`, `onDemandCap`, `onDemandUsed`,
`prepaidBalance`, `isUnifiedBillingUser`, `topUpMethod`, `currentPeriod.type` —
<https://github.com/chenyme/grok2api/blob/449b95fa/backend/docs/RESPONSES_COMPATIBILITY.md>. **LIKELY**

### 3.4 Request/response schema (Responses)

**VERIFIED** (docs) for the public API; the CLI proxy is wire-compatible for the subset below
(**LIKELY**, with per-field exceptions noted).

```json
POST /v1/responses
{
  "model": "grok-4.6",
  "input": [
    {"role": "system", "content": "…"},
    {"role": "user", "content": [{"type": "input_text", "text": "…"},
                                  {"type": "input_image", "image_url": "data:image/png;base64,…"}]}
  ],
  "stream": true,
  "store": false,
  "include": ["reasoning.encrypted_content"],
  "reasoning": {"effort": "high"},
  "max_output_tokens": 8192,
  "tools": [{"type": "function", "name": "get_weather",
             "parameters": {"type": "object", "properties": {}}}],
  "tool_choice": "auto",
  "prompt_cache_key": "<session id>"
}
```

Parameter mapping away from Chat Completions (**VERIFIED**,
<https://docs.x.ai/developers/model-capabilities/text/comparison.md>):

| Chat Completions | Responses |
| --- | --- |
| `messages` | `input` |
| `max_tokens` | `max_output_tokens` |
| — | `previous_response_id`, `store`, `include` |

Response body: `output` is an array of typed items — `message` (with `content[].type = "output_text"`),
`reasoning`, `function_call`, `web_search_call`, `x_search_call`, `code_interpreter_call`,
`file_search_call`, `mcp_call` — plus `usage` including `cost_in_usd_ticks`.

**CLI-proxy quirks (LIKELY, from shipped workarounds):**

* `store` defaults to `false`; `include` should always carry `reasoning.encrypted_content`
  (OmniRoute: `if (transformed.store === undefined) transformed.store = false; transformed.include = ensureReasoningInclude(...)`).
* **Rejects** `presence_penalty`, `frequency_penalty`, `logprobs`, `top_logprobs`, and (model-dependent)
  `reasoning_effort` / `reasoning` — clients must strip them.
* **Hard cap of 200 tools per request** (`xaiMaxTools = 200`); beyond that the upstream 400s, so cap
  client-side.
* Native tool discriminators observed on 0.2.99: `function`, `web_search`, `x_search`,
  `image_generation`, `collections_search`, `file_search`, `code_execution`, `code_interpreter`,
  `mcp`, `shell`. `namespace`, `tool_search`, `local_shell`, `apply_patch`, `computer_use_preview`
  are **not** in the 0.2.99 enum.
* `reasoning.effort` accepted set on the CLI proxy: `low | medium | high` (OmniRoute whitelist);
  `grok-composer-2.5-fast` rejects `effort` entirely.

### 3.5 Streaming (SSE)

**VERIFIED** for both surfaces being SSE with `data:` JSON lines terminated by `data: [DONE]`.

Responses-API event stream (event `type` discriminates; **LIKELY** for exact names on this surface):

```text
data: {"type":"response.created","response":{...}}
data: {"type":"response.output_item.added","output_index":0,"item":{...}}
data: {"type":"response.output_text.delta","output_index":0,"delta":"Hel"}
data: {"type":"response.reasoning_summary_text.delta","delta":"…"}
data: {"type":"response.output_item.done","output_index":0,"item":{...}}
data: {"type":"response.completed","response":{"usage":{...},"output":[...]}}
data: [DONE]
```

Implementation facts a proxy must handle (**LIKELY**):

* `response.completed` may arrive with an **empty `response.output`**; CLIProxyAPI reconstructs it from
  the preceding `response.output_item.done` events keyed by `output_index` (`xaiPatchCompletedOutput`).
  Assume nothing about ordering guarantees beyond monotonic `sequence_number`.
* Usage on the CLI proxy is reported in `response.completed`; grok2api forwards standard
  `usage.input_tokens` / `output_tokens` / `output_tokens_details.reasoning_tokens` /
  `cost_in_usd_ticks`.
* The CLI proxy emits private events; a proxy should forward only `response.*`, `error`, and `[DONE]`
  (grok2api filters exactly these and preserves standard SSE comment/`id`/`retry` fields).

Chat Completions SSE shape if you ever use `api.x.ai` with a key (**VERIFIED**,
<https://docs.x.ai/developers/model-capabilities/text/streaming.md>) — note reasoning arrives as
`delta.reasoning_content` alongside `delta.content`:

```json
data: {"id":"<completion_id>","object":"chat.completion.chunk","created":<ts>,"model":"grok-4.7",
 "choices":[{"index":0,"delta":{"reasoning_content":"The","role":"assistant"}}],
 "usage":{"prompt_tokens":41,"completion_tokens":1,"total_tokens":42,
  "prompt_tokens_details":{"text_tokens":41,"audio_tokens":0,"image_tokens":0,"cached_tokens":0}},
 "system_fingerprint":"fp_xxxxxxxxxx"}

data: [DONE]
```

### 3.6 Tool calling, reasoning, images, prompt caching

| Capability | Status | Evidence |
| --- | --- | --- |
| Client function calling | Supported (Responses, both hosts) | **VERIFIED** docs; `function` is the base discriminator |
| Parallel tool calls | Supported, on by default | **VERIFIED** docs |
| Strict structured output | Supported (`text.format` / `json_schema`) | **VERIFIED** docs; CLI proxy maps legacy `response_format` |
| Reasoning content | `reasoning.encrypted_content` via `include`; summaries stream on newer models | **VERIFIED** docs; **LIKELY** CLI proxy |
| Reasoning effort | `low/medium/high` (+`xhigh` on grok-4.6 per docs); CLI proxy accepts `low/medium/high` | **VERIFIED** docs / **LIKELY** CLI |
| Image **input** | Yes — `input_image` with `data:` URI or https URL; jpg/png, ≤20 MiB per image, no count limit | **VERIFIED** docs (api.x.ai); **LIKELY** CLI proxy |
| Image **generation** | api.x.ai: `/v1/images/generations` or `image_generation` tool. CLI proxy: `image_generation` tool only | **VERIFIED** / **LIKELY** |
| Prompt caching | `prompt_cache_key` + cached-token pricing | **VERIFIED** docs; CLI proxy honours `prompt_cache_key` for stickiness |
| Server-side tools | `web_search`, `x_search`, `code_interpreter`, `file_search`, `mcp` | **VERIFIED** docs (api.x.ai) |
| `store` / `previous_response_id` | api.x.ai yes; **CLI proxy: stateful chains work but must be pinned to the same account** | **LIKELY** |

---

## 4. Model IDs, quota, free tier

### 4.1 Model IDs on the public API (`api.x.ai`) — **VERIFIED**

<https://docs.x.ai/developers/models.md> and <https://docs.x.ai/developers/rate-limits.md>

```text
grok-4.7                              # current flagship, 500k ctx, $2/$6 per 1M
grok-4.6
grok-4.5
grok-4.3                              # 1M ctx, $1.25/$2.50
grok-4.20-0309-reasoning
grok-4.20-0309-non-reasoning
grok-4.20-multi-agent-0309
grok-build-0.1                         # 256k ctx, $1/$2 — the coding model
grok-imagine-image | grok-imagine-image-2.0 | grok-imagine-image-quality
grok-imagine-video | grok-imagine-video-1.5 | grok-imagine-video-1.5-lite
grok-voice-3.0
```

Aliases: `<model>` → latest stable, `<model>-latest` → latest, `<model>-<date>` → pinned.

**Retired 2026-05-15** (**VERIFIED**): `grok-code-fast-1`, `grok-4-0709`, `grok-4-fast-*`,
`grok-4-1-fast-*`, `grok-3`, `grok-imagine-image-pro`. Crucially, the slugs still resolve:

```text
grok-code-fast-1  →  grok-build-0.1
grok-4-fast-*     →  grok-4.3 (low reasoning effort)
grok-3            →  grok-4.3 (none reasoning effort)
```

<https://docs.x.ai/developers/migration/may-15-retirement.md>

> Answers the "does `grok-code-fast-1` have a free tier?" question: **it did**, in third-party clients
> (OpenCode Zen made it free; that ended 2026-01-24 and the model itself was retired 2026-05-15 and now
> silently redirects to `grok-build-0.1`). Do **not** ship `grok-code-fast-1` as a free-tier model id —
> it will bill at `grok-build-0.1` rates. (<https://github.com/link-assistant/agent/blob/main/docs/case-studies/issue-133/README.md>)

### 4.2 Model IDs on the Grok CLI proxy (`cli-chat-proxy.grok.com`) — **LIKELY**

```text
grok-4.5                     # 500k ctx, supports reasoning + tool calling
grok-composer-2.5-fast       # 200k ctx, CLI-proxy-only, rejects reasoning.effort
grok-build                   # legacy id seen before the 0.2.101 client
grok-build-0.1               # coding model (api.x.ai canonical name)
```

* OmniRoute's registry (`open-sse/config/providers/registry/grok-cli/index.ts`) declares exactly
  `grok-4.5` (500k, reasoning, toolCalling) and `grok-composer-2.5-fast` (200k, no reasoning).
* CLIProxyAPI hardcodes `FreeOAuthModel = "grok-4.5"` as *the model exposed by the Grok CLI free
  entitlement*, and `ComposerModelPrefix = "grok-composer-"` as CLI-proxy-only.
  <https://patch-diff.githubusercontent.com/raw/router-for-me/CLIProxyAPI/pull/4206.diff>
* Hermes' OAuth picker lists `grok-4.6` (default), `grok-build-0.1`, `grok-4.3`,
  `grok-4.20-0309-reasoning`, `grok-4.20-0309-non-reasoning`, `grok-4.20-multi-agent-0309` and says
  the catalogue is derived from the `models.dev` cache, i.e. **it is discovery-driven, not fixed**.
  <https://hermes-agent.nousresearch.com/docs/guides/xai-grok-oauth>
* grok2api notes free-tier response bodies carry model ids ending in `-build-free`.
  <https://github.com/chenyme/grok2api/blob/449b95fa/backend/docs/RESPONSES_COMPATIBILITY.md>

⇒ **Design rule: never hardcode the model list. Call `GET {base}/models` per credential and cache,
with a static fallback list.**

### 4.3 Quota: what a subscription actually grants

Two entirely separate billing systems exist; do not conflate them.

**(a) Public API = pay-per-token, tiered by cumulative spend. VERIFIED**
(<https://docs.x.ai/developers/rate-limits.md>)

```text
Tier 0  $0        grok-4.7: 150 RPS / 50M TPM
Tier 1  $50       grok-4.7: 172 RPS / 53M TPM
Tier 2  $250      grok-4.7: 208 RPS / 60M TPM
Tier 3  $1,000    grok-4.7: 312 RPS / 74M TPM
Tier 4  $5,000    grok-4.7: 500 RPS / 100M TPM
Enterprise on request
```
Per-second cap is derived as `RPM / 48`, minimum 2. Exceeding → HTTP `429`. `grok-4.3`/`grok-build-0.1`
are `37/10M` at Tier 0; `grok-4.20-multi-agent-0309` is `9/2.5M`.

**(b) Subscription ("The X Subscription", formerly SuperGrok / X Premium+) = weekly pool. VERIFIED**
(<https://docs.x.ai/x-subscription/usage-limits.md>)

* Each *paid seat* gets a **weekly pool**, shared across eligible first-party Grok **and Cursor**
  surfaces — not split per product.
* Third-party models inside Cursor are **not** in the pool (paid "Additional Usage").
* After the pool is exhausted → Additional Usage credits / top-ups; spending caps apply.
* Grok Build is one of the metered products inside that pool.

Grok Build's own usage accounting (**LIKELY**, reverse-engineered from 0.2.93/0.2.99):
`GET /billing` and `GET /billing?format=credits` on the CLI proxy; the paid pool is read over gRPC
`GrokBuildBilling/GetGrokCreditsConfig` and the product breakdown enum is
`0=Third Party, 1=API, 2=Grok Build, 3=Grok Plugins, 4=Chat, 5=Imagine, 6=Voice`.
Free accounts have no weekly pool and are instead limited by `GET https://grok.com/rest/rate-limits`
(the `fast` window). <https://github.com/chenyme/grok2api/blob/449b95fa/backend/docs/RESPONSES_COMPATIBILITY.md>

### 4.4 Free tier: what is actually free

| Claim | Status | Evidence |
| --- | --- | --- |
| Grok Build is **free to try on any X or Grok account**, with usage limits | **LIKELY** | <https://github.com/phuryn/grok-build-vscode/commit/27ede44766b8d4b6e2f6555476f274c13790678d> (re-verified against x.ai/build 2026-09-29) |
| A subscription (SuperGrok / X Premium+) unlocks Grok Build for sustained use | **LIKELY** | same |
| An **API key** is what unlocks `grok-4.x` + `grok-imagine` | **LIKELY** | same |
| The OAuth surface exposes a free model, `grok-4.5`, via the CLI transport | **LIKELY** | `FreeOAuthModel = "grok-4.5"` in CLIProxyAPI PR #4206 |
| Free-tier requests on Build report `cost_in_usd_ticks: 0` | **LIKELY** | grok2api ("Build OAuth 实测响应中 Free 请求返回 0") |
| OAuth inference is gated by *subscription tier*, not merely by scope | **VERIFIED (user report)** | see §4.5 |

### 4.5 The 402/403 entitlement wall (critical for product risk)

**VERIFIED** as a real, reproduced user-facing failure — Hermes Agent issue #26847, closed
`not_planned`: a standard **SuperGrok ($30/mo)** account with an active X Premium+ completed OAuth
fine, then every inference call returned:

```json
HTTP 403
{"code":"The caller does not have permission to execute the specified operation",
 "error":"You have either run out of available resources or do not have an active Grok subscription. Manage your subscription at https://grok.com/?_s=usage or subscribe at https://grok.com/supergrok."}
```

The reporter's diagnosis: the OAuth token routes to the same backend surface as **Grok Build**, which
was then restricted to *SuperGrok Heavy*; `grok.com/?_s=usage` displayed *"Grok Build Beta — Beta is
only available for SuperGrok Heavy users."* Workaround: use an `XAI_API_KEY`.
<https://github.com/NousResearch/hermes-agent/issues/26847>

This is exactly why CLIProxyAPI added the 402/403 → CLI-proxy fallback and the `tier`-claim router.
**Assume entitlement gating will happen and design for it.**

---

## 5. Required headers

### 5.1 On `cli-chat-proxy.grok.com` (mandatory, **LIKELY**)

```text
Authorization: Bearer <access_token>
Content-Type: application/json
Accept: text/event-stream            # streaming; application/json otherwise
User-Agent: grok-shell/0.2.101 (windows; x86_64)
x-grok-client-version: 0.2.101
x-grok-client-identifier: grok-shell
x-grok-client-mode: headless
X-XAI-Token-Auth: xai-grok-cli
x-authenticateresponse: authenticate-response
x-grok-model-override: <model>        # optional
x-grok-conv-id: <session id>          # conversation/session affinity
x-userid: <principal id>              # optional
x-grok-user-id: <principal id>        # optional
x-email: <email>                      # omitted for team/org principals
```

Verbatim source (OmniRoute `open-sse/config/grokBuild.ts`):
<https://github.com/diegosouzapw/OmniRoute/commit/0adcb9097479acefb56a4f14c5d01f016868a1c8>

```ts
export const GROK_BUILD_DEFAULT_CLIENT_VERSION = "0.2.101";
export const GROK_BUILD_CLIENT_IDENTIFIER = "grok-shell";
export const GROK_BUILD_TOKEN_AUTH = "xai-grok-cli";

export function getGrokBuildClientHeaders(clientMode = "headless") {
  return {
    "x-grok-client-version": getGrokBuildClientVersion(),
    "x-grok-client-identifier": GROK_BUILD_CLIENT_IDENTIFIER,
    "x-grok-client-mode": clientMode,
    "User-Agent": getGrokBuildUserAgent(),   // `grok-shell/${v} (${os}; ${arch})`
  };
}

export function getGrokBuildSessionHeaders({ token, model, stream = false, clientMode = "headless", userId, email, principalType } = {}) {
  return {
    "Content-Type": "application/json",
    Accept: stream ? "text/event-stream" : "application/json",
    ...getGrokBuildClientHeaders(clientMode),
    "X-XAI-Token-Auth": GROK_BUILD_TOKEN_AUTH,
    "x-authenticateresponse": "authenticate-response",
    ...(token ? { Authorization: `Bearer ${token}` } : {}),
    ...(model ? { "x-grok-model-override": model } : {}),
    ...(userId ? { "x-userid": userId, "x-grok-user-id": userId } : {}),
    ...(wireEmail ? { "x-email": wireEmail } : {}),
  };
}
```

Platform/arch are normalised: `darwin→macos`, `win32→windows`, `arm64→aarch64`, `x64→x86_64`.

grok2api pins the same triple (its "version contract"):

```text
x-grok-client-version: 0.2.99
x-grok-client-identifier: grok-shell
User-Agent: grok-shell/0.2.99 (linux; x86_64)
```
<https://github.com/chenyme/grok2api/blob/449b95fa/backend/docs/RESPONSES_COMPATIBILITY.md>

**Header-name caveat:** these are lowercase on the wire. A mixed-case user override (e.g.
`X-Grok-Client-Version`) will *not* replace the lowercase default in some HTTP stacks and the two
merge into an invalid value — opencodex fixed exactly this
(<https://github.com/lidge-jun/opencodex/commit/93fe7e1b608fef7fd69f0cd12e7239f7fa9df8d8>).

### 5.2 Client-version drift: HTTP 426

**LIKELY** — CLIProxyAPI's current `main` states:

```
// Keep in sync with the current Grok CLI client version that chat-proxy
// expects. The server rejects older versions with HTTP 426; it required
// 1.0.13+ as of 2026-10-01 (#6249).
xaiClientVersionFallback = helps.DefaultXAIFallbackClientVersion
```

and it resolves the current version **from the npm registry** at runtime, falling back to the
hardcoded value. <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/runtime/executor/xai_executor.go>

Observed version strings over time: `0.2.72` (`grok_cli_rs`), `0.2.93`, `0.2.99`, `0.2.101`, then
`1.0.13+`. **Do not hardcode a single version; make it configurable and expect 426.**

### 5.3 On `api.x.ai` (with an OAuth token or API key)

```text
Authorization: Bearer <token_or_xai_key>
Content-Type: application/json
```

Nothing else is required (**VERIFIED** by every xAI doc example). `x-grok-conv-id` is harmless and
useful for prompt-cache affinity (**LIKELY**); CLIProxyAPI sets it on both transports:

```go
if sessionID != "" { r.Header.Set("x-grok-conv-id", sessionID) }
```

### 5.4 Web (`grok.com`) surface — cookies, not OAuth

Not reachable with the OAuth bearer token. It is cookie-based and Cloudflare-guarded (**LIKELY**):

* `POST https://grok.com/rest/app-chat/conversations/new` — the app-chat endpoint; requires the
  `sso` cookie, `Origin: https://grok.com`, `Referer: https://grok.com/`.
  <https://huggingface.co/spaces/bigsuperangel/grok2api/blob/main/app/services/reverse/app_chat.py>
* Cookies in play: `sso`, `sso-rw` (session), plus `cf_clearance`, `__cf_bm`, `_cfuvid`, `cf_chl_*`
  (Cloudflare). `sso`/`sso-rw` must always be regenerated from the stored SSO token.
* Additional anti-bot inputs: `x-statsig-id` (signed per method+path), `x-xai-request-id` (fresh
  UUID v4 per request), Chrome TLS/HTTP2 fingerprint, and a `grok-site-verification` meta read from
  `https://grok.com/index`.
* Rate limiting/quota for web accounts: `GET https://grok.com/rest/rate-limits`.
* File upload for images: `POST /rest/app-chat/upload-file` → `fileMetadataId` → `fileAttachments`.
* **UNVERIFIED** upstream response shapes; grok2api deliberately does not forward `Set-Cookie`.

⇒ The web surface is a browser-impersonation problem (TLS fingerprint, Cloudflare, statsig signing)
and is explicitly out of scope for a clean OAuth-based proxy.

---

## 6. Decisive answer: is the OAuth token usable, and for what?

**OAuth *does* work for inference — but only through the Responses API, and only on the transport the
account is entitled to. A public-API key is not required, and an OAuth token cannot reach the
Chat-Completions-shaped public surface reliably.**

| Question | Answer | Confidence |
| --- | --- | --- |
| Is there a real xAI OAuth server for Grok clients? | Yes, `auth.x.ai`, OIDC discovery public, code + device grants, PKCE S256 | **VERIFIED** |
| Can an OAuth token call `api.x.ai/v1/responses`? | Yes, for tokens whose JWT says `tier > 0`; may 402/403 otherwise | **LIKELY** |
| Can an OAuth token call `cli-chat-proxy.grok.com/v1/responses`? | Yes, this is the default/fallback path and the free-tier path | **LIKELY** |
| Can it call `api.x.ai/v1/chat/completions`? | Not demonstrated by any implementation; all of them translate to `/responses` | **UNVERIFIED — assume no** |
| Can it call `api.x.ai/v1/messages` (Anthropic)? | That endpoint exists but is deprecated even for API keys; no implementation uses it with OAuth | **UNVERIFIED — assume no** |
| Can it call `grok.com` web endpoints? | No — those need `sso` cookies + Cloudflare clearance | **LIKELY** |
| Is an `XAI_API_KEY` still needed? | Only as a fallback for accounts whose OAuth token is tier-gated (the documented 403 workaround) | **VERIFIED (user report)** |
| Does the OAuth token also work for TTS / image / video / transcription? | Yes on `api.x.ai` — Hermes reuses one bearer for all of them | **LIKELY** |

**Design consequence:** the proxy's Grok provider is *a Responses-API client with two possible base
URLs*, plus a credential that may or may not be entitled. Everything else (OpenAI Chat, Anthropic
Messages) is **translation**, not retargeting.

---

## 7. Anthropic compatibility

**xAI's native Anthropic-compatible endpoint exists but is deprecated — do not build on it.**

**VERIFIED** (<https://docs.x.ai/developers/rest-api-reference/inference/legacy.md>):

```text
POST /v1/messages    (Anthropic-compatible: system, messages, max_tokens, stop_sequences,
                      tools[].input_schema, tool_choice; returns type:"message",
                      stop_reason: "end_turn"|"max_tokens"|"tool_use", usage{input_tokens,
                      cache_read_input_tokens, output_tokens})
POST /v1/complete    (legacy Anthropic text completion)
```

Both carry the banner: *"**Deprecated**: The Anthropic SDK compatibility is fully deprecated. Please
migrate to the Responses API or the gRPC API."* And the capability matrix confirms
*"Anthropic compatibility is fully deprecated … xAI explicitly directs users to Responses or gRPC"*
(<https://raw.githubusercontent.com/wxxb789/ghc-proxy/5d0590a2e5354bede72a2161732d233b1e8e9e00/docs/research/xai-api-capability-matrix-2026-08-14.md>).

**On the CLI-proxy transport there is no `/v1/messages` at all.** Every proxy surveyed implements it
as a translation:

* grok2api: *"Anthropic Messages 使用 `POST /v1/messages`，要求 `anthropic-version`… Chat Completions 与
  Messages 都先转换为标准 Responses 输入项。Build 将其发送到原生 Responses 上游，并把 JSON/SSE 转回调用方协议"* —
  <https://github.com/chenyme/grok2api/blob/449b95fa/backend/docs/RESPONSES_COMPATIBILITY.md>
* OmniRoute/9router route Claude Code into the Grok provider via this same translation
  (their notes mention reasoning-strip regressions specifically triggered by Claude Code payloads).

**Recommendation:** implement Anthropic Messages ⇄ OpenAI Responses directly (not via Chat
Completions) in the proxy:

```text
POST /v1/messages  →  {model, input[], tools[], tool_choice, stream}
                   →  translate content blocks: text → input_text / output_text,
                      image (base64|url) → input_image, tool_use → function_call,
                      tool_result → function_call_output, thinking → reasoning (encrypted_content)
                   →  POST {base}/responses
                   →  translate back: message_start / content_block_start / content_block_delta /
                      content_block_stop / message_delta / message_stop
```

Anthropic `thinking.signature` has no equivalent: do not fabricate one (grok2api explicitly refuses).

---

## 8. Concrete open-source implementations (repo + path + URL)

| # | Project | File | What it proves |
| --- | --- | --- | --- |
| 1 | **CLIProxyAPI** (Go) | `internal/auth/xai/types.go`, `xai.go`, `token.go`, `pkce.go`, `entitlement.go` | Canonical client_id/scopes/port, full exchange+refresh, **tier-based routing**, 402/403 fallback |
| 2 | **CLIProxyAPI** (Go) | `internal/runtime/executor/xai_executor.go` | `/responses` usage, header set, SSE reconstruction, 200-tool cap, **HTTP 426 version drift**, npm version resolution |
| 3 | **ZeroClaw** (Rust) | `crates/zeroclaw-providers/src/auth/xai_oauth.rs` | Full device-code state machine + `~/.grok/auth.json` importer |
| 4 | **OmniRoute** (TS) | `open-sse/config/grokBuild.ts`, `open-sse/executors/grok-cli.ts`, `src/lib/oauth/providers/grok-cli.ts` | Exact header constants, refresh-with-principal, request sanitisation |
| 5 | **grok2api** (Python) | `backend/docs/RESPONSES_COMPATIBILITY.md` | The best single write-up: version contract, native tool enums, billing endpoints, weekly-pool semantics, web-SSO handling |
| 6 | **Hermes Agent** (Python) | `hermes_cli/auth.py` | Alternative PKCE attempt, `~/.grok/auth.json` import, 403 diagnostics |
| 7 | **9router** (JS) | `open-sse/providers/registry/grok-cli.js`, `src/lib/oauth/providers.js` | Device-code-without-PKCE bug analysis; `cli-chat-proxy` user profile call |
| 8 | **opencodex** (TS) | `src/providers/xai-transport.ts` | Header case-sensitivity pitfall, `x-grok-conv-id` derivation from `prompt_cache_key` |
| 9 | **OpenDigitalProductFactory** | `docs/superpowers/specs/2026-06-07-grok-device-code-oauth-design.md` | Field report: `grok login --device-auth` → token → `api.x.ai/v1/responses` works |

### Constants quoted from each

**#1/#2 CLIProxyAPI** — <https://github.com/router-for-me/CLIProxyAPI/blob/v7.1.60/internal/auth/xai/types.go>

```go
const (
	// DefaultAPIBaseURL is the default xAI Responses API base URL.
	DefaultAPIBaseURL = "https://api.x.ai/v1"
	// Issuer is xAI's OAuth issuer.
	Issuer = "https://auth.x.ai"
	// DiscoveryURL is the OIDC discovery endpoint used to resolve OAuth endpoints.
	DiscoveryURL = Issuer + "/.well-known/openid-configuration"
	// ClientID is the public xAI Grok CLI OAuth client ID.
	ClientID = "b1a00492-073a-47ea-816f-4c329264a828"
	// Scope is the OAuth scope set required for xAI API access.
	Scope = "openid profile email offline_access grok-cli:access api:access"
	// RedirectHost is the loopback host used by xAI OAuth.
	RedirectHost = "127.0.0.1"
	// CallbackPort is the preferred loopback callback port.
	CallbackPort = 56121
	// RedirectPath is the loopback callback path registered by the xAI client.
	RedirectPath = "/callback"
)
```

and after PR #4206:

```go
DefaultAPIBaseURL   = "https://api.x.ai/v1"
CLIChatProxyBaseURL = "https://cli-chat-proxy.grok.com/v1"
FreeOAuthModel      = "grok-4.5"
ComposerModelPrefix = "grok-composer-"
UsingAPIKey         = "using_api"
```

and in `xai_executor.go` (main): `xaiTokenAuthValue = "xai-grok-cli"`,
`xaiClientIdentifierValue = "grok-shell"`, `xaiAuthenticateResponseValue = "authenticate-response"`,
`xaiMaxTools = 200`, `xaiClientVersionFallback = helps.DefaultXAIFallbackClientVersion`,
plus the comment about **HTTP 426 from stale versions** and `1.0.13+ as of 2026-10-01 (#6249)`.

**#3 ZeroClaw** — <https://github.com/zeroclaw-labs/zeroclaw/blob/21fbd953/crates/zeroclaw-providers/src/auth/xai_oauth.rs>

```rust
// Public OAuth client id used by Grok Build CLI/OpenClaw auth profiles; not a client secret.
pub const XAI_OAUTH_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const XAI_OAUTH_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const XAI_OAUTH_ISSUER: &str = "https://auth.x.ai";
pub const XAI_OAUTH_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
pub const XAI_OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";
const XAI_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
```

Note it **discovers** endpoints rather than hardcoding them, and validates that every discovered URL
is https and on `x.ai` / `*.x.ai` (`require_trusted_endpoint`) — worth copying for SSRF hygiene.

**#4 OmniRoute** — <https://github.com/diegosouzapw/OmniRoute/commit/0adcb9097479acefb56a4f14c5d01f016868a1c8>

```ts
export const GROK_BUILD_PROXY_BASE_URL = "https://cli-chat-proxy.grok.com/v1";
export const GROK_BUILD_RESPONSES_URL = `${GROK_BUILD_PROXY_BASE_URL}/responses`;
export const GROK_BUILD_MODELS_URL = `${GROK_BUILD_PROXY_BASE_URL}/models`;

export const GROK_BUILD_OAUTH_ISSUER = "https://auth.x.ai";
export const GROK_BUILD_DEVICE_CODE_URL = `${GROK_BUILD_OAUTH_ISSUER}/oauth2/device/code`;
export const GROK_BUILD_TOKEN_URL = `${GROK_BUILD_OAUTH_ISSUER}/oauth2/token`;

export const GROK_BUILD_DEFAULT_CLIENT_VERSION = "0.2.101";
export const GROK_BUILD_CLIENT_IDENTIFIER = "grok-shell";
export const GROK_BUILD_TOKEN_AUTH = "xai-grok-cli";
export const GROK_BUILD_REASONING_INCLUDE = "reasoning.encrypted_content";
export const GROK_BUILD_OAUTH_REFERRER = "grok-build";
export const GROK_BUILD_OAUTH_SCOPES = Object.freeze([
  "openid","profile","email","offline_access","grok-cli:access","api:access",
  "conversations:read","conversations:write",
]);
```

**#5 grok2api** — <https://github.com/chenyme/grok2api/blob/449b95fa/backend/docs/RESPONSES_COMPATIBILITY.md>

```text
x-grok-client-version: 0.2.99
x-grok-client-identifier: grok-shell
User-Agent: grok-shell/0.2.99 (linux; x86_64)
```

**#6 Hermes** — <https://github.com/NousResearch/hermes-agent/commit/3a503f00a60a413cf5f8a5310cf332b19302ef66>
(superseded by the docs page, which switched to device code)

```python
XAI_OAUTH_ISSUER = "https://auth.x.ai"
XAI_OAUTH_AUTHORIZE_URL = "https://auth.x.ai/oauth/authorize"   # note: no "2" — likely wrong/legacy
XAI_OAUTH_TOKEN_URL = "https://auth.x.ai/oauth/token"           # note: no "2" — likely wrong/legacy
XAI_OAUTH_CLIENT_ID = "b1a00492-073a-47ea-816f-4c329264a828"
XAI_OAUTH_SCOPE = "openid profile email offline_access grok-cli:access api:access"
GROK_CLI_AUTH_PATH = Path.home() / ".grok" / "auth.json"
```

⚠️ Its `/oauth/authorize` and `/oauth/token` paths **contradict the live discovery document**
(`/oauth2/...`). Treat those two constants as bugs; use discovery.

**#7 9router** — <https://github.com/fzrilsh/bercocok-tanam/blob/main/docs/GROK-CLI-OAUTH-ANALYSIS.md>

```javascript
export default {
  id: "grok-cli",
  oauth: {
    clientId: "b1a00492-073a-47ea-816f-4c329264a828",
    deviceCodeUrl: "https://auth.x.ai/oauth2/device/code",
    tokenUrl: "https://auth.x.ai/oauth2/token",
    refreshUrl: "https://auth.x.ai/oauth2/token",
    scope: "openid profile email offline_access grok-cli:access api:access conversations:read conversations:write",
    referrer: "grok-build",
    refreshLeadMs: 300000,
  },
};
```

and its device-code request:

```http
POST https://auth.x.ai/oauth2/device/code
Content-Type: application/x-www-form-urlencoded
User-Agent: grok-pager/0.2.93 grok-shell/0.2.93 (linux; x86_64)
```

---

## 9. Implications for the Python reverse proxy (design notes, not code)

1. **Provider abstraction = "Responses client", not "OpenAI client".** Model the upstream as
   `POST {base}/responses` with `input[]`; translate OpenAI Chat and Anthropic Messages *into* it.
   Do not assume `/chat/completions` exists.
2. **Two base URLs behind one credential.** Resolve `base` per request:
   custom override → `grok-composer-*` ⇒ CLI proxy → explicit `using_api` flag → JWT `tier` claim →
   default CLI proxy. Cache the decision on the credential; expose an operator override.
3. **Credential object should carry:** `access_token`, `refresh_token`, `expires_at`, `token_endpoint`,
   `id_token`, `email`, `sub`, `principal_type`, `principal_id`, `tier`, `base_url`, `kind`
   (`oauth` | `api_key`), plus a `last_error`/`reauth_required` flag.
4. **Login = device code first.** No redirect URI to register, works through SSH/containers,
   user-facing URL comes from the response. Print `verification_uri_complete` when present.
   Fall back to importing `~/.grok/auth.json` when it exists (best UX, zero failure modes).
5. **Always send the CLI header triple** on the CLI-proxy transport (`x-grok-client-version`,
   `x-grok-client-identifier: grok-shell`, `X-XAI-Token-Auth: xai-grok-cli`,
   `x-authenticateresponse`, matching `User-Agent`), and treat **HTTP 426** as "bump the version",
   not as a fatal error.
6. **Sanitise the body per transport:** strip `presence_penalty`, `frequency_penalty`, `logprobs`,
   `top_logprobs`, and unsupported `reasoning_effort`; cap `tools` at 200; force `store:false` and
   `include:["reasoning.encrypted_content"]` where the client did not ask otherwise.
7. **Handle the entitlement wall explicitly:** on `402`, or `403` whose body matches the
   billing/subscription markers, retry once on the CLI proxy (for `grok-4.5`) and otherwise surface a
   typed "subscription does not include API access — provide an xAI API key" error. Do **not** loop.
8. **Token lifetime:** refresh at `expires_at - 300s` and reactively on 401; persist rotated refresh
   tokens; treat `invalid_grant` as terminal.
9. **SSE relay:** parse `data:` lines, forward `response.*`/`error`/`[DONE]`, reconstruct
   `response.output` from `response.output_item.done` if `response.completed` is empty, and re-emit
   Anthropic/OpenAI chunk shapes only after a full event is assembled (never mid-JSON).
10. **Never mix `sso` cookies into the OAuth path.** Two completely different auth universes.

---

## 10. Open questions / risks

**Top 3 risks**

1. **Entitlement gating is invisible until inference time.** OAuth login can succeed while every
   request 402/403s (issue #26847). The `tier` claim is undocumented and its semantics were inferred
   from behaviour. A free/standard account may be limited to `grok-4.5` on the CLI proxy, and the
   *paid* surface may require SuperGrok Heavy (as of 2026-05). **Mitigation:** two-transport fallback,
   `GET /billing` probe at connect time, and an explicit API-key escape hatch.
2. **`cli-chat-proxy.grok.com` is an internal product endpoint with no contract.** grok2api says so
   outright; CLIProxyAPI's own comment shows the server started enforcing a minimum client version
   with **HTTP 426** (`1.0.13+` as of 2026-10-01, up from `0.2.93/0.2.99/0.2.101` within months).
   Header names, tool enums, SSE event names, and model ids can change without notice.
3. **`/v1/chat/completions` is deprecated and unproven on OAuth.** xAI labels it legacy, returns no
   reasoning content there, and no surveyed implementation uses it with an OAuth token. Building the
   proxy on Chat Completions semantics (rather than translating to Responses) risks a dead end —
   plus reasoning/thinking and encrypted-state features would be silently lost.

**Open questions**

* Does `api.x.ai/v1/chat/completions` accept an OAuth bearer at all (200 vs 401/402/403)? Not answered
  by any source found. Needs a live probe with both a `tier>0` and a `tier==0` token.
* Does `api.x.ai/v1/models` accept an OAuth bearer? Hermes calls it (`fetch_api_models(token, base)`),
  which implies yes, but no captured response was found.
* Exact semantic of the `tier` claim values (0/1/2/3/4? "free"? "heavy"?) — only `>0` / `==0` /
  `"free"` are attested; `"pro"` is explicitly treated as unknown by CLIProxyAPI.
* Is `/v1/messages` still functional at `api.x.ai` with an OAuth token, or already 404/410? Docs say
  deprecated but present; no OAuth datapoint exists.
* Are device-code scopes `conversations:read/write` required, harmless, or rejected for this client?
  Both narrow and wide scope strings appear in shipping code.
* Does the CLI proxy stream `response.reasoning_summary_text.*` for `grok-4.5`/`grok-4.6`, or only
  `reasoning.encrypted_content` blobs? Affects whether "thinking" can be shown to users.
* Rate limits **on the OAuth/CLI path**: no published numbers. Only indirect signals (weekly pool,
  free `fast` window, `429` on web endpoints). Expect much lower limits than the documented API tiers.
* Whether `principal_type`/`principal_id` in the refresh body is required for team/org accounts or
  merely tolerated (single implementation, **UNVERIFIED**).
* `grok-pager/0.2.93` vs `grok-shell/…`: which User-Agent family is authoritative for the *OAuth*
  calls specifically? 9router uses `grok-pager … grok-shell …` (two tokens); everyone else uses
  `grok-shell/<v> (<os>; <arch>)`.
* Long-term: xAI may publish a first-party "Coding Plan" OAuth contract (Hermes has a
  `xai-coding-plan` provider entry) — if that lands with documented scopes/limits, the tier-hint
  heuristic can be deleted.
