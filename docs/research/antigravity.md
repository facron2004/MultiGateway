# Google Antigravity — OAuth + upstream LLM protocol reference

Research target: implement a self-contained Python provider in MultiGateway that reuses Antigravity's
OAuth login and speaks to the same upstream API the official IDE speaks to.

**Research date:** 2026-10-18 (Asia/Shanghai). Antigravity ships weekly; treat every constant here as
"correct as of the commit/date cited", not eternal.

**Method:** no Antigravity install was available in this environment, so *nothing here is verified
against a live capture from the official binary*. Facts are graded by the quality of the source:

| Marker | Meaning |
|---|---|
| **[V]** VERIFIED | Directly read in source code of an OSS implementation that is known to work in production, **or** an unambiguous quote from a primary spec document. Multiple independent implementations agree. |
| **[L]** LIKELY | Read in one credible OSS implementation, or consistent across several but not independently confirmed. |
| **[U]** UNVERIFIED | Reported second-hand (issue/PR text, blog, doc), or inferred. Confirm before shipping. |

Where sources disagree, the disagreement is called out explicitly with a `⚠ CONFLICT` block — those
blocks are the ones most likely to cost you a debugging day.

---

## 1. OAuth client

### 1.1 Identifiers

```text
client_id     = <GOOGLE_OAUTH_CLIENT_ID>
client_secret = <GOOGLE_OAUTH_CLIENT_SECRET>
```

**[V]** The same pair appears verbatim in four independent, actively maintained implementations:

- `looplj/axonhub` — `llm/transformer/antigravity/constants.go`
  <https://raw.githubusercontent.com/looplj/axonhub/9acca6be/llm/transformer/antigravity/constants.go>
- `router-for-me/CLIProxyAPI` — `internal/auth/antigravity/constants.go`
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/auth/antigravity/constants.go>
- `NoeFabris/opencode-antigravity-auth` — `src/constants.ts`
  <https://raw.githubusercontent.com/NoeFabris/opencode-antigravity-auth/main/src/constants.ts>
- `Mirrowel/LLM-API-Key-Proxy` — `src/rotator_library/providers/antigravity_auth_base.py`
  <https://raw.githubusercontent.com/Mirrowel/LLM-API-Key-Proxy/bf7ab7e7d85595c0b85b67ea2b62522747201213/src/rotator_library/providers/antigravity_auth_base.py>

This is a **public** client secret embedded in a distributed desktop app (the "installed application"
anti-pattern Google tolerates for native clients). It is not a security boundary — the OAuth flow is
protected by the loopback redirect and the user's browser session, not by the secret.

> The client id prefix `1071006060591` is a Google Cloud project number, **not** the Antigravity
> product's own project. Do not "correct" it to a `*.apps.googleusercontent.com` form you find
> elsewhere; multiple projects exist (Cloud Code, Gemini CLI, gcloud) with different ids and they are
> **not** interchangeable — quota and tier are attached to the Antigravity one.

### 1.2 Endpoints and redirect

```text
authorization_endpoint = https://accounts.google.com/o/oauth2/v2/auth
token_endpoint         = https://oauth2.googleapis.com/token
userinfo_endpoint      = https://www.googleapis.com/oauth2/v1/userinfo     (axonhub)
                         https://www.googleapis.com/oauth2/v2/userinfo?alt=json  (CLIProxyAPI)
redirect_uri           = http://localhost:51121/oauth-callback
```

**[V]** `authorization_endpoint`, `token_endpoint`, port `51121` and the literal path
`/oauth-callback`:

- axonhub `constants.go`: `AuthorizeURL = "https://accounts.google.com/o/oauth2/v2/auth"`,
  `TokenURL = "https://oauth2.googleapis.com/token"`, `RedirectURI = "http://localhost:51121/oauth-callback"`
- CLIProxyAPI `constants.go`: `CallbackPort = 51121`, `AuthEndpoint`, `TokenEndpoint`;
  `sdk/auth/antigravity.go` builds `fmt.Sprintf("http://localhost:%d/oauth-callback", port)`
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/sdk/auth/antigravity.go>
- NoeFabris `src/constants.ts`: `ANTIGRAVITY_REDIRECT_URI = "http://localhost:51121/oauth-callback"`

**Port 51121 is fixed, not ephemeral.** All three hard-code it (CLIProxyAPI optionally allows
`--callback-port` override). If you use a loopback server, bind 51121 or the consent screen will
redirect somewhere your server is not listening.

⚠ **CONFLICT — callback path.** `Mirrowel/LLM-API-Key-Proxy` sets:

```python
CALLBACK_PORT = 51121
CALLBACK_PATH = "/oauthcallback"      # note: no hyphen
```

<https://raw.githubusercontent.com/Mirrowel/LLM-API-Key-Proxy/bf7ab7e7d85595c0b85b67ea2b62522747201213/src/rotator_library/providers/antigravity_auth_base.py>

Three implementations say `/oauth-callback`, one says `/oauthcallback`. **Resolution:** Google's
loopback redirect matching is exact, so only one of these is the registered value. The `-oauth-callback`
spelling is supported by the majority and by the implementation that also documents the exact
`redirect_uri` used in the token exchange (CLIProxyAPI passes the same string to both authorize and
token calls, which is the value that must match). Treat `/oauth-callback` as correct **[L]** and treat
Mirrowel's spelling as either a bug or a stale registration. Decide empirically once: complete a login
and see which path the browser hits — the wrong one returns `redirect_uri_mismatch` from Google.

### 1.3 Scopes

```text
https://www.googleapis.com/auth/cloud-platform
https://www.googleapis.com/auth/userinfo.email
https://www.googleapis.com/auth/userinfo.profile
https://www.googleapis.com/auth/cclog
https://www.googleapis.com/auth/experimentsandconfigs
```

**[V]** Identical list (same order) in axonhub `constants.go`, CLIProxyAPI `constants.go`, NoeFabris
`src/constants.ts`, Mirrowel `antigravity_auth_base.py`.

Answering the question directly: **yes, they are cloud-platform + userinfo.email/profile, plus two
Antigravity-specific scopes.** `cloud-platform` is the broad one that lets the token call Cloud Code
Assist and Cloud Resource Manager; `userinfo.*` are only used to read the account email for labeling;
`cclog` and `experimentsandconfigs` are Antigravity/Cloud-Code-internal and are what distinguish this
client from a plain `gcloud` login. `openid` is **not** requested by any of the four.

### 1.4 Authorization request parameters

**[V]** From CLIProxyAPI `BuildAuthURL`:

```go
params.Set("access_type", "offline")
params.Set("client_id", ClientID)
params.Set("prompt", "consent")
params.Set("redirect_uri", redirectURI)
params.Set("response_type", "code")
params.Set("scope", strings.Join(Scopes, " "))
params.Set("state", state)
```

`access_type=offline` + `prompt=consent` are what force a `refresh_token` on every login. Omitting
`prompt=consent` means a re-login silently returns no refresh token, which is the classic
"works for an hour then dies" bug.

`include_granted_scopes=true` is added by Mirrowel, absent elsewhere; harmless either way.

### 1.5 PKCE

**No PKCE.** None of the four implementations sends `code_challenge` / `code_challenge_method`, and all
of them send `client_secret` on the token exchange. This is a "confidential" (web/installed) client
with a public secret, so Google does not require PKCE. **[V]** for "no PKCE in any reference
implementation"; **[L]** for "adding PKCE would be rejected or ignored" (Google generally ignores
`code_challenge` for clients not configured for it, but do not assume).

Token exchange body (form-encoded, `Content-Type: application/x-www-form-urlencoded`) **[V]**:

```text
code=<code>&client_id=...&client_secret=...&redirect_uri=<exact same string>&grant_type=authorization_code
```

---

## 2. Token response and refresh

### 2.1 Response shape

Google's standard token response; fields the implementations actually decode:

```json
{
  "access_token": "ya29.a0Af...",
  "expires_in": 3599,
  "refresh_token": "1//0g...",
  "scope": "https://www.googleapis.com/auth/cloud-platform ...",
  "token_type": "Bearer",
  "id_token": "eyJhbGciOiJSUzI1NiIs..."
}
```

**[V]** for `access_token` / `expires_in` / `refresh_token` / `token_type` — CLIProxyAPI's
`TokenResponse` struct and `AntigravityTokenResponse` in `sdk/auth/antigravity.go` decode exactly these
four. **[L]** for `scope` and `id_token` being present: they are part of Google's documented token
response and are returned for this flow, but none of the reference implementations read them, so no
source *proves* their presence here. Do not depend on `id_token`; if you want the email, call the
userinfo endpoint with the access token (that is what CLIProxyAPI does).

`expires_in` is ~3599 s. Persist an absolute expiry, not the delta.

### 2.2 Refresh

**[V]** CLIProxyAPI `antigravity_executor_auth.go` → `refreshTokenSingleFlight`:

```go
form.Set("client_id", antigravityClientID)
form.Set("client_secret", antigravityClientSecret)
form.Set("grant_type", "refresh_token")
form.Set("refresh_token", refreshToken)
// POST https://oauth2.googleapis.com/token
httpReq.Header.Set("Content-Type", "application/x-www-form-urlencoded")
// Real Antigravity uses Go's default User-Agent for OAuth token refresh
httpReq.Header.Set("User-Agent", "Go-http-client/2.0")
```

Two non-obvious details worth copying:

1. **The refresh call carries `User-Agent: Go-http-client/2.0`** — the source comment says the real
   client's token refresh goes through Go's default HTTP client. This is a fingerprint detail; whether
   Google enforces it is unknown, but it costs nothing to match.
2. Refresh is wrapped in a **single-flight keyed by refresh token**, and the refresh context is
   detached from the caller (`context.WithoutCancel`) with a 30 s timeout — a client disconnecting
   must not cancel a token refresh that other requests are waiting on.

Refresh response is the same shape minus `refresh_token` (Google does not rotate it here; CLIProxyAPI
still overwrites the stored one only `if tokenResp.RefreshToken != ""`).

Recommended refresh lead: CLIProxyAPI refreshes when the token has **< 5 minutes** left
(`antigravityRequestTokenSafetyWindow`) and its auth manager refreshes **30 minutes** before expiry
(`RefreshLead()`); Mirrowel caches expiry as RFC3339 in metadata.

---

## 3. Cloud Code Assist API surface

### 3.1 Hosts

```text
https://cloudcode-pa.googleapis.com                  "prod"
https://daily-cloudcode-pa.googleapis.com             "daily"      (a.k.a. daily non-sandbox)
https://daily-cloudcode-pa.sandbox.googleapis.com     "sandbox"
https://autopush-cloudcode-pa.sandbox.googleapis.com  "autopush"   (sandbox, usually dead)
```

**[V]** All four hostnames appear in axonhub `constants.go` (`EndpointDaily`, `EndpointAutopush`,
`EndpointProd`) and NoeFabris `src/constants.ts` (`ANTIGRAVITY_ENDPOINT_DAILY`,
`ANTIGRAVITY_ENDPOINT_AUTOPUSH`, `ANTIGRAVITY_ENDPOINT_PROD`). CLIProxyAPI uses three of them
(`antigravityBaseURLDaily`, `antigravitySandboxBaseURLDaily`, `antigravityBaseURLProd`).
Antigravity-Manager lists the three `.../v1internal` base variants.

⚠ **CONFLICT — which host is primary.**

| Source | Content requests go to | Notes |
|---|---|---|
| CLIProxyAPI | `daily-cloudcode-pa.googleapis.com` | `resolveAntigravityRequestBaseURL` defaults to **daily**, comments call it the consumer default |
| Antigravity-Manager | daily → sandbox only | comment: prod is dropped for *content* because "全流量必报 429" (all traffic 429s), and daily is what the official `language_server` is launched against |
| NoeFabris | daily first, then autopush, then prod | `ANTIGRAVITY_ENDPOINT_FALLBACKS` |
| npdk/tccpc API spec doc | prod listed "Active" | no ordering claim |
| Mirrowel | prod first for `loadCodeAssist` | `ANTIGRAVITY_LOAD_ENDPOINT_ORDER` |

**Resolution:** the *host set* is settled; the *ordering* is a runtime health decision, not a constant.
Both CLIProxyAPI and Antigravity-Manager converge on **daily for content** and **prod for
`loadCodeAssist`**. Build the provider with a per-endpoint health tracker and fall back on
408/404/5xx (see §7.3) rather than hard-coding one host.

### 3.2 Paths

The path shape is `/{version}:{method}` — note the **colon**, it is gRPC-transcoding style, not
`/v1internal/generateContent`.

```text
POST /v1internal:loadCodeAssist
POST /v1internal:onboardUser
POST /v1internal:generateContent
POST /v1internal:streamGenerateContent?alt=sse
POST /v1internal:fetchAvailableModels
POST /v1internal:retrieveUserQuota
POST /v1internal:retrieveUserQuotaSummary        (see §6, [L])
POST /v1internal:countTokens
```

**[V]** for the first six:
- CLIProxyAPI `internal/runtime/executor/antigravity_executor.go` constants:
  `antigravityCountTokensPath = "/v1internal:countTokens"`,
  `antigravityStreamPath = "/v1internal:streamGenerateContent"`,
  `antigravityGeneratePath = "/v1internal:generateContent"`;
  `internal/auth/antigravity/auth.go` builds `"%s/%s:loadCodeAssist"` and `"%s/%s:onboardUser"`.
- NoeFabris `src/plugin/quota.ts`: `POST ${endpoint}/v1internal:fetchAvailableModels` and
  `POST ${endpoint}/v1internal:retrieveUserQuota`.
- Antigravity-Manager `project_resolver.rs`: literal
  `https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist` etc.;
  `upstream/client.rs` `build_url` produces `format!("{}:{}", base_url, method)` and its unit test
  asserts `https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse`.

**[L]** `v1internal:retrieveUserQuotaSummary` — only seen in PR titles, not in source I read:
<https://github.com/Wei-Shaw/sub2api/pull/7558> ("support retrieveUserQuotaSummary for Gemini 3.8 usage")
and <https://github.com/Wei-Shaw/sub2api/pull/8157> ("display 5h/weekly quota summary buckets").
Treat as a bonus endpoint; do not depend on it.

⚠ Note the **`/v1internal` prefix is part of the base, not the path**, in the official client:
every working implementation builds `https://host/v1internal:<method>`. Requests to
`/v1internal/projects/{p}/locations/global/models/{m}:generateContent` (Vertex-style) return HTML 404
on all three hosts — Antigravity-Manager documents this failure mode explicitly in
`call_v1_internal_auxiliary`. Do not improvise a Vertex-shaped URL.

### 3.3 `loadCodeAssist`

Request **[V]** (CLIProxyAPI `internal/auth/antigravity/auth.go` `FetchProjectID`; Antigravity-Manager
`project_resolver.rs` is byte-identical in shape):

```json
{ "metadata": { "ideType": "ANTIGRAVITY" } }
```

NoeFabris/Mirrowel send a richer metadata object, plus an optional project:

```json
{
  "metadata": {
    "ideType": "ANTIGRAVITY",
    "platform": "WINDOWS",
    "pluginType": "GEMINI",
    "duetProject": "<configured project id, optional>"
  },
  "cloudaicompanionProject": "<configured project id or null, optional>"
}
```

Response (fields actually consumed) **[V]**:

```json
{
  "cloudaicompanionProject": "my-project-123456",          // string, or {"id": "..."} object
  "currentTier": { "id": "free-tier" },
  "allowedTiers": [
    { "id": "free-tier", "isDefault": true, "userDefinedCloudaicompanionProject": false },
    { "id": "legacy-tier", "isDefault": false }
  ],
  "paidTier": {
    "id": "g1-pro",
    "availableCredits": [
      { "creditType": "GOOGLE_ONE_AI", "creditAmount": 25, "minimumCreditAmountForUsage": 1 }
    ]
  }
}
```

- `cloudaicompanionProject` may be a **string or `{"id": ...}`** — handle both. **[V]**: Mirrowel
  `_extract_project_id_from_response` exists precisely because of this; CLIProxyAPI
  `extractCloudaicompanionProject` accepts `cloudaicompanionProject` / `projectId` / `project`, each as
  string or `{id}`.
- `paidTier.availableCredits[]` with `creditType == "GOOGLE_ONE_AI"` is where Google One AI credits are
  reported; CLIProxyAPI reads `creditAmount` and `minimumCreditAmountForUsage` and treats
  `creditAmount >= minimumCreditAmountForUsage` as "credits available". **[V]**
  (`antigravity_executor_credits.go`, `updateAntigravityCreditsBalanceForTask`).

### 3.4 `onboardUser`

Request **[V]** (two shapes, both accepted):

```json
// free tier — server provisions the project
{ "tierId": "free-tier", "metadata": { "ideType": "ANTIGRAVITY" } }

// paid / legacy tier — you must supply the project
{
  "tierId": "legacy-tier",
  "metadata": { "ideType": "ANTIGRAVITY", "duetProject": "<project>" },
  "cloudaicompanionProject": "<project>"
}
```

CLIProxyAPI sends snake_case `tier_id` in `OnboardUser` (`requestBody := map[string]any{"tier_id": tierID, ...}`)
while NoeFabris/Mirrowel send camelCase `tierId`. Proto-JSON accepts both; **[L]** that both work.

It is a **long-running operation**: poll until `done` is true.

```json
{ "done": false }
```

```json
{
  "done": true,
  "response": { "cloudaicompanionProject": { "id": "my-project-123456" } }
}
```

Note the nesting: on completion the id is at `response.cloudaicompanionProject.id` (an **object**),
whereas `loadCodeAssist` returns it at the top level (string *or* object). **[V]**

Polling cadence used in the wild: NoeFabris 10 attempts × 5 s per endpoint; Mirrowel 30 × 2 s (60 s
cap); CLIProxyAPI 5 attempts with 2 s sleep and a 30 s per-request timeout.

### 3.5 `generateContent` / `streamGenerateContent`

Request body is the wrapped envelope (§5). Successful non-streaming response **[V]**:

```json
{
  "response": {
    "candidates": [
      {
        "content": { "role": "model", "parts": [ { "text": "..." } ] },
        "finishReason": "STOP"
      }
    ],
    "usageMetadata": {
      "promptTokenCount": 16,
      "candidatesTokenCount": 4,
      "totalTokenCount": 20,
      "thoughtsTokenCount": 12
    },
    "modelVersion": "claude-sonnet-4-6",
    "responseId": "msg_vrtx_01UDKZG8PWPj9mjajje8d7u7"
  },
  "traceId": "abc123..."
}
```

**[V]** for the envelope wrapper `{"response": ... , "traceId": ...}`: CLIProxyAPI's
`convertStreamToNonStream` reconstructs exactly `{"response":{},"traceId":""}` and reads
`responseNode := root.Get("response")` (falling back to a bare root with `candidates` — i.e. accept
both wrapped and unwrapped). Its `modelVersion` / `responseId` / `usageMetadata` handling matches the
fields above. npdk/tccpc's spec doc shows the same shape with response-id formats per family
(`msg_vrtx_...` for Claude, base64-ish for Gemini/GPT-OSS) — **[L]**, doc-only.

Non-streaming `generateContent` returns the **same `{"response": ...}` wrapper** (not a bare
`candidates` object). The `alt=sse` streaming endpoint emits `data: {...}` lines each carrying the same
wrapper, one per chunk, blank-line separated, and — per all implementations — **`[DONE]` is not
emitted by the upstream**; you must synthesize the terminal event yourself. **[V]**:
CLIProxyAPI appends a synthetic `[DONE]` translation only "if `ctx.Err() == nil`" at clean EOF, and
comments that translating `[DONE]` after a read error would report a truncated stream as success.

### 3.6 `fetchAvailableModels` / `retrieveUserQuota`

Both are `POST` with a tiny body and are used purely for quota display. **[V]**

```json
// fetchAvailableModels
{ "project": "<project id>" }          // body is {} when project unknown
```

```json
// retrieveUserQuota
{ "project": "<project id>" }
```

Important quirk: **the User-Agent changes what quota you get back.** NoeFabris deliberately sends a
Gemini-CLI UA to `retrieveUserQuota` to read *Gemini CLI* buckets rather than Antigravity buckets:

```ts
const geminiCliUserAgent = `GeminiCLI/1.0.0/gemini-2.5-pro (${platform}; ${arch})`;
```

and the Antigravity UA to `fetchAvailableModels`. If you only want Antigravity quota, only call
`fetchAvailableModels`. **[V]** (`src/plugin/quota.ts`)

Response shapes: see §6.

---

## 4. Required HTTP headers

There is **no single canonical header set** across implementations, and this is the single biggest
source of confusion in this protocol. Here is the matrix, then a recommendation.

### 4.1 `User-Agent` — three competing formats

**(a) Hub / official-egress style** (Antigravity-Manager, current):

```text
antigravity/hub/2.23.1 (aidev_client; os_type=darwin; arch=arm64; cl=998881661)
```

**[V]** `src-tauri/src/constants.rs`:

```rust
pub const OFFICIAL_HUB_VERSION: &str = "2.23.1";
pub const OFFICIAL_HUB_CL: &str = "998881661";
// format: "antigravity/hub/{OFFICIAL_HUB_VERSION} (aidev_client; os_type={os}; arch={arch}; cl={OFFICIAL_HUB_CL})"
```

with a documented version floor:

```rust
pub const KNOWN_STABLE_VERSION: &str = "4.3.0";
```

and, in `sanitize_egress_user_agent`, two **hard upstream behaviours** stated as fact:

- a UA without the substring `antigravity` (case-insensitive) → **HTTP 403 Forbidden** upstream;
- a UA version **< 3.0.0** (e.g. legacy `1.15.8`) → **HTTP 404** for segmented models
  (`gemini-3.8-flash-low/medium/high`). The code therefore clamps any version below `4.3.0` up to
  `4.3.0`.

**(b) Short form** (CLIProxyAPI, NoeFabris error-path, npdk spec):

```text
antigravity/hub/2.23.0 darwin/arm64        # CLIProxyAPI misc.AntigravityUserAgent()
antigravity/1.15.8 windows/amd64           # npdk spec doc example
```

**[V]** CLIProxyAPI `internal/misc/antigravity_version.go`:

```go
antigravityFallbackVersion = "2.23.0"
antigravityHubPlatform     = "darwin/arm64"
// AntigravityUserAgent() => fmt.Sprintf("antigravity/hub/%s %s", version, platform)
```

with the comment: *"Cloud Code rejects newer models for clients below 2.23.0, so this floor must stay
at or above that version."* The version is refreshed every 6 h from
`https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml`
(an electron-builder YAML). Note the **different auto-updater host** from Antigravity-Manager's
`https://antigravity-auto-updater-974169037036.us-central1.run.app` — same GCP project number
`974169037036`, different service names.

**(c) Browser/Electron style** (NoeFabris default, npdk spec):

```text
Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Antigravity/1.18.3 Chrome/138.0.7204.235 Electron/37.3.1 Safari/537.36
```

**[V]** `src/constants.ts` `getAntigravityHeaders()`, with fallback version `"1.18.3"`.
Antigravity-Manager's `get_default_user_agent()` is the same idea:
`Antigravity/{ver} (Macintosh; Intel Mac OS X 10_15_7) Chrome/132.0.6834.160 Electron/39.2.3`.

**Practical guidance:** the only *enforced* invariants anyone has documented are (i) the UA contains
`antigravity`, and (ii) the version is high enough. Pick form (a) with a version ≥ `4.3.0` and you
satisfy both, while also matching what the current IDE sends. Do not ship `1.15.8`; it is a 404/429
generator on current models.

### 4.2 `X-Goog-Api-Client`

```text
google-cloud-sdk vscode_cloudshelleditor/0.1     # axonhub, NoeFabris (antigravity style)
google-cloud-sdk vscode/1.96.0                    # NoeFabris random pool
google-cloud-sdk vscode/1.95.0                    # NoeFabris random pool
gl-node/22.21.1                                   # CLIProxyAPI (onboardUser only)
gl-node/22.18.0                                   # Mirrowel (auth/discovery only)
```

**[V]** axonhub `ApiClient = "google-cloud-sdk vscode_cloudshelleditor/0.1"`; NoeFabris
`ANTIGRAVITY_API_CLIENTS` array and `GEMINI_CLI_HEADERS["X-Goog-Api-Client"] = "gl-node/22.17.0"`;
CLIProxyAPI `AntigravityGoogAPIClientUA = "gl-node/22.21.1"` (used only in the `onboardUser` request).

Note that **CLIProxyAPI does not send `X-Goog-Api-Client` on `generateContent` at all** — only on
`onboardUser`. Antigravity-Manager never sends it. So it is not required for content.

### 4.3 `Client-Metadata`

```text
{"ideType":"ANTIGRAVITY","platform":"WINDOWS","pluginType":"GEMINI"}
{"ideType":"ANTIGRAVITY","platform":"MACOS","pluginType":"GEMINI"}
{"ideType":"ANTIGRAVITY","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}   # axonhub
{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"} # Mirrowel
```

**[V]** each literal above, from axonhub `ClientMetadata`, NoeFabris
`getAntigravityHeaders()["Client-Metadata"]`, and Mirrowel `ANTIGRAVITY_AUTH_HEADERS["Client-Metadata"]`.

**This header is disputed.** NoeFabris sends it with content requests; Mirrowel sends it only on
auth/discovery calls; **CLIProxyAPI and Antigravity-Manager never send it at all** and both work.
Conclusion **[L]**: `Client-Metadata` is *not* required for `generateContent`; the same information
travels in the **body** (`request.metadata` / the `loadCodeAssist` body's `metadata`). Sending it is
harmless. Do not build logic that depends on it.

⚠ **CONFLICT — the tier-detection trick.** Mirrowel's header comment claims:

```python
# CRITICAL: User-Agent MUST be google-api-nodejs-client/* for standard-tier detection.
# Using antigravity/* UA causes server to return free-tier only (tested via matrix test).
# X-Goog-Api-Client value doesn't affect tier detection.
ANTIGRAVITY_AUTH_HEADERS = {
    "User-Agent": "google-api-nodejs-client/10.3.0",
    "X-Goog-Api-Client": "gl-node/22.18.0",
    "Client-Metadata": '{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}',
}
```

No other implementation corroborates this, and NoeFabris's `project.ts` sends
`User-Agent: google-api-nodejs-client/9.15.1` for `loadCodeAssist` while CLIProxyAPI sends the normal
`antigravity/hub/...` UA. If you need paid/standard tier visibility on `loadCodeAssist`, this is worth
an A/B test; otherwise treat it as **[U]**.

### 4.4 `x-goog-user-project`

Antigravity-Manager is the only implementation that uses it, and it does so **only on non-content
calls** (quota / model listing), *removing* it for `generateContent` / `streamGenerateContent`:

```rust
let is_content_request = matches!(method, "generateContent" | "streamGenerateContent");
if !is_content_request {
    if let Some(proj) = body.get("project").and_then(|v| v.as_str()) { /* set x-goog-user-project */ }
}
if is_content_request { headers.remove("x-goog-user-project"); }
```

plus a documented recovery path: a **403 while `x-goog-user-project` is present triggers exactly one
retry without the header**. **[V]** (`upstream/client.rs`).

**Recommendation:** omit `x-goog-user-project` on content requests. If you send it and get 403, retry
once without it.

### 4.5 `Accept` / `Content-Type`

```text
Content-Type: application/json                      # all requests, always
Authorization: Bearer <access_token>                # all requests, always
Accept: text/event-stream                           # streaming only (npdk spec; optional in practice)
Accept: */*                                         # CLIProxyAPI sends this on loadCodeAssist
```

**[V]** CLIProxyAPI sets `Content-Type`/`Authorization`/`User-Agent` on content requests and
`Accept: */*` on `loadCodeAssist` and the credits query. npdk's spec says streaming also wants
`Accept: text/event-stream`; CLIProxyAPI does not send it and streams fine, so it is **[L]** optional.

Response `Content-Type` for streaming is `text/event-stream`. **[V]**

### 4.6 Extra headers seen in the wild

```text
anthropic-beta: claude-code-20250219     # Antigravity-Manager injects for any model whose id contains "claude"
```

**[V]** `upstream/client.rs`. This is an "upstream privilege header" the proxy adds to match what
Claude-in-Antigravity requests carry. Cheap to include for Claude ids; unknown whether enforced (**[U]**).

Antigravity-Manager's header **blacklist** for client-supplied headers is also instructive — it strips
`user-agent`, `x-session-id`, `x-client-name`, `x-client-version`, `x-machine-id`, `x-vscode-sessionid`,
and anything `x-jeikcode*`, i.e. these are client-fingerprint headers you should **not** forward from
downstream clients. **[V]**

### 4.7 Minimal working header set (my recommendation)

```http
POST /v1internal:streamGenerateContent?alt=sse HTTP/1.1
Host: daily-cloudcode-pa.googleapis.com
Authorization: Bearer <access_token>
Content-Type: application/json
User-Agent: antigravity/hub/2.23.1 (aidev_client; os_type=darwin; arch=arm64; cl=998881661)
Accept: text/event-stream
anthropic-beta: claude-code-20250219      (Claude model ids only, optional)
```

That is exactly the CLIProxyAPI set plus a stronger UA. **[L]**

---

## 5. Request envelope and schema

### 5.1 The wrapper

The body is **not** a Gemini `GenerateContentRequest`. It is:

```json
{
  "project": "<gcp project id>",
  "requestId": "agent/<uuid>/<epoch_ms>/<uuid>/<step>",
  "request": { "contents": [ ... ], "systemInstruction": { ... }, "tools": [ ... ],
               "generationConfig": { ... }, "sessionId": "-1234567890123456789",
               "labels": { ... } },
  "model": "<model id>",
  "userAgent": "antigravity",
  "requestType": "agent"
}
```

**[V]** Key **order** is asserted by Antigravity-Manager's own test
`test_official_claude_alignment` (`src-tauri/src/proxy/pipeline/official_alignment_tests.rs`):

```rust
// 1. Envelope keys: project -> requestId -> request -> model -> userAgent -> requestType
assert_eq!(keys, vec!["project", "requestId", "request", "model", "userAgent", "requestType"]);
```

The same six keys are produced by CLIProxyAPI `geminiToAntigravity`
(`internal/runtime/executor/antigravity_executor_request.go`):

```go
template = helps.SetStringIfDifferent(template, "model", modelName)
template = helps.SetStringIfDifferent(template, "userAgent", "antigravity")
// requestType: "image_gen" for image models, "agent" otherwise, "web_search" when caller set it
template = helps.SetStringIfDifferent(template, "project", projectID)   // deleted when projectID == ""
template, _ = sjson.SetBytes(template, "requestId", generateRequestID()) // "agent-" + uuid
template, _ = sjson.SetBytes(template, "request.sessionId", sessionID)
template, _ = sjson.DeleteBytes(template, "request.safetySettings")
```

and by npdk/tccpc's spec doc (`{"project","model","request","userAgent","requestId"}`).

⚠ **CONFLICT — `requestId` format.** Three variants in the wild:

| Source | Value |
|---|---|
| CLIProxyAPI | `"agent-" + uuid.NewString()` → `agent-3f2b...-...` |
| Antigravity-Manager (official alignment) | `agent/<uuid>/<epoch_ms>/<uuid>/<step>` and, for image gen, `image_gen/<epoch_ms>/<uuid>/12` |
| npdk spec doc | `"agent-abc123"` (informational) |

CLIProxyAPI's hyphenated form is what it actually sends and it works; Antigravity-Manager's slashed
form is what it believes the *official* client sends (it is reconstructing the official envelope from
captures). **Use the slashed official-looking form if you want maximum fidelity, the hyphenated form if
you want the battle-tested one.** Both are accepted — nothing validates the shape **[L]**.

`request.sessionId` is a **negative decimal string** (`"-1234567890123456789"`) in CLIProxyAPI:
`generateSessionID()` returns `"-" + strconv.FormatInt(rand.Int63n(9e18), 10)`, and
`generateStableSessionID()` derives it deterministically from `sha256(first user text)` so that the
same conversation reuses the same session id (important for prompt-cache hits). **[V]**

### 5.2 `request.labels`

Antigravity-Manager adds a `labels` object inside `request`, derived from the model catalog. **[V]**
(tests above):

```json
"labels": {
  "model_enum": "MODEL_PLACEHOLDER_M35",
  "used_claude": "true",
  "used_claude_conservative": "true",
  "used_non_gemini_model": "true",
  "trajectory_id": "0181971f-961c-4ba9-a320-2ae44361bc1d",
  "request_id": "0181971f-961c-4ba9-a320-2ae44361bc1d-0",
  "last_step_index": "2"
}
```

`model_enum` values observed: Claude Sonnet 4.6 → `MODEL_PLACEHOLDER_M35`, Claude Opus 4.6 Thinking →
`MODEL_PLACEHOLDER_M26`, Gemini 3.8 Flash High → `MODEL_PLACEHOLDER_M318`, Gemini 3.8 Flash Low →
`MODEL_PLACEHOLDER_M320`, Gemini 3.1 Pro High → `MODEL_PLACEHOLDER_M37`, GPT-OSS 120B Medium →
`MODEL_OPENAI_GPT_OSS_120B_MEDIUM`, `tab_flash_lite_preview` → `MODEL_PLACEHOLDER_M19`.
**[V]** (`test_official_model_catalog_resolution`).

CLIProxyAPI does **not** send `labels` and works. So `labels` is a fidelity/telemetry feature, not a
requirement. **Recommendation: skip it initially**; add it only if you observe model-routing or
quota-attribution differences. **[L]**

### 5.3 `request.contents`

Standard Gemini. **[V]**

```json
{
  "contents": [
    { "role": "user",  "parts": [ { "text": "Hello" } ] },
    { "role": "model", "parts": [ { "text": "Hi" } ] }
  ]
}
```

- `role` is `user` | `model`. **`assistant` is invalid.** **[V]** (npdk spec, and every
  implementation translates `assistant` → `model`).
- parts may carry `text`, `inlineData` (`{mimeType, data}` base64), `functionCall`
  (`{name, args, id}`), `functionResponse` (`{name, response, id}`), `thought: true`, and
  `thoughtSignature`.
- **Tool-result turns must have `role: "model"`.** CLIProxyAPI actively rewrites a functionResponse
  turn's role to `model` when it contains nothing but function responses, and re-pairs/orders
  `functionResponse` parts against the preceding `functionCall` ids. **[V]**
  (`normalizeAntigravityGeminiFunctionResponseRoles`)
- **Boundary rules:** CLIProxyAPI ensures a *leading* user turn exists (prepending an empty one if
  needed), ensures the last turn is not a model turn, and drops a trailing thought-only model turn.
  These are cited as fixes for real 400s (`#6530`). **[V]** — worth replicating; more than one
  provider has this class of bug.

### 5.4 `systemInstruction`

Must be an object with `parts`, never a bare string. **[V]** (npdk spec: plain string → 400 "Invalid
value"; every implementation builds `{"parts":[{"text": ...}]}`).

```json
"systemInstruction": { "role": "user", "parts": [ { "text": "You are..." } ] }
```

`role` inside `systemInstruction` is optional; Antigravity-Manager's commented-out code in CLIProxyAPI
shows `"role":"user"` being set on the official path. **[L]**

### 5.5 `tools`

```json
"tools": [
  { "functionDeclarations": [
      { "name": "get_weather",
        "description": "...",
        "parameters": { "type": "object", "properties": { ... }, "required": ["location"] } } ] }
]
```

**[V]** Notes that cost real debugging time:

- `parametersJsonSchema` must be **renamed to `parameters`**; `parameters_json_schema` likewise.
  CLIProxyAPI does this rename explicitly before cleaning. **[V]**
- JSON Schema support is *incomplete*: `const`, `$ref`, `$defs`/`definitions`, `$schema`, `$id`,
  `default`, `examples` cause 400s; `const` must become `enum:[value]`; `$ref` must be inlined. **[V]**
  (npdk spec's support matrix, and CLIProxyAPI ships `CleanJSONSchemaForAntigravityTool` /
  `...ForAntigravityClaudeTool` / `...ForAntigravityResponse` cleaners).
- Function names: start with letter or `_`, `[a-zA-Z0-9_.:-]`, ≤64 chars. **No `/`.** **[V]** (npdk spec).
- `googleSearch` / `urlContext` tools **cannot be combined with `functionDeclarations`** in one
  request. **[V]** (npdk spec; NoeFabris implements a separate `google_search` tool that issues a
  second API call for exactly this reason).
- For Claude model ids, CLIProxyAPI forces `request.toolConfig.functionCallingConfig.mode = "VALIDATED"`
  and flattens multi-branch `anyOf`/`oneOf` unions in tool schemas (the Claude side rejects them). **[V]**
  (`sanitizeAntigravityRequestSchemas`, `sanitizeAntigravityToolSchemaDocumentForModel`).

### 5.6 `generationConfig`

```json
"generationConfig": {
  "maxOutputTokens": 65536,
  "temperature": 0.7,
  "topP": 0.95,
  "topK": 40,
  "stopSequences": ["STOP"],
  "thinkingConfig": { "thinkingBudget": -1, "includeThoughts": true },
  "responseSchema": { ... },
  "imageConfig": { ... }
}
```

**[V]** for the field names (npdk spec table + Antigravity-Manager tests). Key facts:

- **`thinkingBudget`**: for Claude and Gemini 2.5, a positive integer count of thinking tokens. For
  Gemini 3.x, the official default is a sentinel: `-1` means "model default / dynamic", and tiered
  models carry per-tier defaults (`gemini-3.8-flash-low` → `1000`, `-medium` → `4000`, `-high` → `-1`,
  `gemini-3.1-pro-high` → `10001`, `claude-sonnet-4-6` → `1024`). **Do not normalize `-1` to a number**;
  it is meaningful and CLIProxyAPI/Antigravity-Manager both pass it through. **[V]**
- **Hard invariant: `maxOutputTokens > thinkingBudget`.** Violating it is a 400. **[V]** both the npdk
  spec ("maxOutputTokens must be GREATER than thinkingBudget") and Antigravity-Manager's test
  `test_thinking_budget_invariant_scaled_down_when_output_capped`, which asserts it and clamps
  `budget = maxTokens - 1024` on overflow.
- **`maxOutputTokens` is capped server-side per model**, and the proxy must clamp *and* rescale the
  budget together. CLIProxyAPI clamps to `MaxCompletionTokens` from its registry; Antigravity-Manager
  forces `64000` for Claude and `65536` for Gemini in its official-envelope alignment. **[V]**
- `thinkingLevel` (string: `minimal|low|medium|high`) exists in the **model catalog metadata** and is
  used by the Gemini-CLI-style path, but the Antigravity content path uses numeric `thinkingBudget`.
  **[V]** for the catalog field; **[L]** that `generationConfig.thinkingLevel` is not the right knob
  here.

### 5.7 Model id placement

The model goes in the **body** (`"model": "..."`), and the URL carries **no model segment**. **[V]**
(Antigravity-Manager `build_url` unit test; CLIProxyAPI `buildRequest`.)

---

## 6. Models and quota

### 6.1 The model list is server-driven, and it churns fast

`POST /v1internal:fetchAvailableModels` returns the authoritative catalog with quota attached:

```json
{
  "models": {
    "gemini-3.8-flash-high": {
      "displayName": "Gemini 3.8 Flash (High)",
      "model": "MODEL_PLACEHOLDER_M318",
      "modelProvider": "MODEL_PROVIDER_GOOGLE",
      "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
      "maxOutputTokens": 65536,
      "maxTokens": 1048576,
      "supportsThinking": true,
      "thinkingBudget": -1,
      "minThinkingBudget": 32,
      "supportsImages": true,
      "supportsVideo": true,
      "recommended": true,
      "quotaInfo": { "remainingFraction": 0.87, "resetTime": "2026-10-18T21:00:00Z" }
    }
  }
}
```

**[V]** Field list and per-model values from Antigravity-Manager `src-tauri/src/models/official_model.rs`
(`OfficialModelInfo`) + `official_alignment_tests.rs`; `quotaInfo.remainingFraction` / `resetTime` from
NoeFabris `src/plugin/quota.ts`.

`modelProvider` values: `MODEL_PROVIDER_ANTHROPIC` | `MODEL_PROVIDER_GOOGLE` | `MODEL_PROVIDER_OPENAI`.
`apiProvider` values: `API_PROVIDER_ANTHROPIC_VERTEX` | `API_PROVIDER_GOOGLE_GEMINI` |
`API_PROVIDER_OPENAI_VERTEX`. `thinkingLevel` arrives as a string **or an integer** (`gemini-2.5-flash`
→ `"3"`) — parse permissively. **[V]**

**Build the provider against `fetchAvailableModels`, not against a hard-coded list.** Every mature
implementation does exactly this and every one of them has been burned by model retirement.

### 6.2 Concrete model ids observed (as of Oct 2026)

Gemini:

```text
gemini-pro-agent                       # the real upstream id behind "3.1 Pro High" tier
gemini-3.1-pro-high     gemini-3.1-pro-low
gemini-3-pro-high       gemini-3-pro-low       gemini-3-pro-medium
gemini-3.8-flash-high / -medium / -low / -tiered
gemini-3.7-flash-*      gemini-3.6-flash-*     gemini-3.5-flash-*
gemini-3-flash          gemini-3-flash-agent
gemini-3.1-flash-lite   gemini-3.1-flash-image
gemini-3-pro-image      gemini-2.5-flash       gemini-2.5-flash-lite
tab_flash_lite_preview  tab_jump_flash_lite_preview
```

Claude:

```text
claude-sonnet-4-6                       # NOT claude-sonnet-4-6-thinking — that id is invalid
claude-opus-4-6-thinking
claude-sonnet-5-5-high / -medium / -low
claude-opus-5-5-high   / -medium / -low
claude-fable-5
```

Other:

```text
gpt-oss-120b-medium
```

**[V]** All of the above are asserted in Antigravity-Manager
`src-tauri/src/proxy/common/model_mapping.rs` (tests) and / or listed in
`src-tauri/src/proxy/upstream/client.rs` & `official_alignment_tests.rs`. Corroborating (older,
therefore incomplete) lists: axonhub `DefaultModels()` =
`claude-sonnet-4-5, claude-sonnet-4-5-thinking, claude-opus-4-5-thinking, gemini-2.5-flash,
gemini-2.5-flash-lite, gemini-3-pro-low, gemini-3-pro-high, gemini-3-pro-medium, gemini-3-flash,
gemini-3-pro-image, gpt-oss-120b-medium`; npdk spec = `claude-sonnet-4-6`, `claude-sonnet-4-6-thinking`,
`claude-opus-4-5-thinking`, `claude-opus-4-6-thinking`, `gemini-3.1-pro-high`, `gemini-3.1-pro-low`,
`gpt-oss-120b-medium`.

Notable *invalid* ids to avoid:

- `claude-sonnet-4-6-thinking` — explicitly filtered out by Antigravity-Manager's baseline check
  ("claude-sonnet 系列…不存在带有 -thinking 后缀的衍生版本"). **[V]**
- `gemini-3-pro` (bare) — rejected by the baseline filter; must be tiered. **[V]**
- `gemini-2.5-pro`, `gemini-1.5-*`, `gemini-2.0-*`, `gemini-2.5-flash*` — retired; some are
  auto-forwarded by Antigravity-Manager to healthy replacements. **[V]**

⚠ The **tier suffix is part of the model id**, not a separate parameter, for Pro: `gemini-3-pro` must
be sent as `gemini-3-pro-low` / `-high`. Flash instead takes the bare id plus `thinkingBudget`. **[V]**
(NoeFabris `resolveModelWithTier`: "Antigravity API: gemini-3-pro requires tier suffix
(gemini-3-pro-low/high); gemini-3-flash uses bare name + thinkingLevel param"; default Pro tier is
`-low`.)

### 6.3 Quota shapes

`fetchAvailableModels` → per-model `quotaInfo.remainingFraction` (0..1, missing ⇒ treat as 0) and
`resetTime` (RFC3339). NoeFabris aggregates the **minimum** remainingFraction per family
(`claude`, `gemini-pro`, `gemini-flash`). **[V]**

`retrieveUserQuota` → token buckets: **[V]** (NoeFabris `RetrieveUserQuotaResponse`)

```json
{ "buckets": [
    { "modelId": "gemini-3-pro-preview",
      "tokenType": "REQUESTS",           // or tokens/input/output variants
      "remainingAmount": "1234",
      "remainingFraction": 0.42,
      "resetTime": "2026-10-19T00:00:00Z" } ] }
```

`retrieveUserQuotaSummary` → described in PR titles as **5-hour and weekly "quota summary buckets"**;
shape not read. **[U]**

`loadCodeAssist` → tier + credits:
`currentTier.id` (e.g. `free-tier`, `legacy-tier`, a paid id), `allowedTiers[].{id,isDefault,
userDefinedCloudaicompanionProject}`, `paidTier.{id, availableCredits[]}`. **[V]**

### 6.4 "Daily free requests"

**There is no documented per-day request counter in this API.** What exists:

- `currentTier.id == "free-tier"` for accounts without a paid plan, and `allowedTiers` carrying
  `free-tier` / `legacy-tier` with `isDefault`. **[V]** (Mirrowel's onboarding branch keys off
  `current_tier_id == "free-tier"` and passes `cloudaicompanionProject: None` so the server provisions
  a project; CLIProxyAPI's `defaultAntigravityTierID` returns `"free-tier"` as its last resort).
- Quota is expressed as **fractional buckets with a reset time**, not a counter: `remainingFraction` +
  `resetTime`, and (per the sub2api PRs) 5-hour and weekly windows. **[L]**
- Google One AI **credits** (`paidTier.availableCredits`, `creditType: GOOGLE_ONE_AI`) with a minimum
  spend threshold. **[V]**

Practical consequence: build the quota model as "N buckets, each with `remainingFraction` and
`resetTime`", not as "requests remaining today". Do not invent a daily counter in the UI.

---

## 7. Practical pitfalls

### 7.1 Project id resolution (and caching)

Order that works **[V]** (converged from Mirrowel `_discover_project_id` and CLIProxyAPI
`FetchProjectID`):

1. In-memory cache / persisted metadata (`project_id` on the credential).
2. Explicit override (`ANTIGRAVITY_PROJECT_ID`, `GOOGLE_CLOUD_PROJECT`, per-auth attribute `base_url` /
   `project_id`).
3. `loadCodeAssist` → `cloudaicompanionProject` (string or `{id}`). If present, use it — **server wins
   over any configured value**.
4. If `currentTier` exists but no project: for `free-tier`, fall through to onboarding; for paid tiers
   that require a user-defined project (`userDefinedCloudaicompanionProject: true`), **fail loudly**
   and demand a configured project. Mirrowel raises
   `Paid tier '<id>' requires setting ANTIGRAVITY_PROJECT_ID environment variable.`
5. Otherwise `onboardUser` with `allowedTiers[isDefault].id` (fallback `legacy-tier`, then
   `allowedTiers[0]`, then `"free-tier"`), polling until `done`, and read
   `response.cloudaicompanionProject.id`.
6. Last-resort: `GET https://cloudresourcemanager.googleapis.com/v1/projects`, pick the first
   `lifecycleState == "ACTIVE"`.

Both implementations **cache aggressively**: Mirrowel keyed by credential path, NoeFabris keyed by
**refresh token**, and both persist `project_id` next to the credential. Do the same — `loadCodeAssist`
is slow and rate-limited.

Hardcoded fallback project used when the server returns nothing (business/Workspace accounts):

```text
rising-fact-p41fc
```

**[V]** axonhub `DefaultProjectID`, NoeFabris `ANTIGRAVITY_DEFAULT_PROJECT_ID`. This is Google's
"Cloud Code default" project string; it appears in real traffic. Use only as a last resort, and note
that Antigravity-Manager treats `"test-project"` and `"project-id"` as sentinels to *omit* rather than
send. **[V]**

### 7.2 Region gating

**Real and load-bearing.** **[V]** Antigravity-Manager `upstream/client.rs`, verbatim:

> `// [FIX Issue #3525 / PR #3526] ... Sandbox 对部分地区返回终止性 400`
> `// 'User location is not supported for the API use.'`

and, in `should_try_next_endpoint_for_url`:

```rust
// 400 Bad Request: only on the Sandbox endpoint is this retryable (to fall back)
|| (status == StatusCode::BAD_REQUEST && endpoint_url.contains("sandbox"))
```

So: **the sandbox host returns `400 FAILED_PRECONDITION: User location is not supported for the API
use.` for non-US IPs**, and 400 is normally *not* retryable, which is why sandbox must never be first.
Design implication: treat `400 + location message` as an endpoint problem (rotate host), not a request
problem (do not blame the payload).

### 7.3 Endpoint fallback rules

**[V]** (both implementations agree):

```text
retry next endpoint on: 408 REQUEST_TIMEOUT | 404 NOT_FOUND | any 5xx | (400 && host contains "sandbox")
do NOT retry on:         other 4xx, including 429 and 403
```

429 gets *credential/model* rotation instead, not endpoint rotation. Antigravity-Manager also tracks
per-**host** health with a 30–60 s cooldown and reorders endpoints dynamically, sharing that health
across `generateContent` / `loadCodeAssist` / `fetchAvailableModels`.

### 7.4 Token expiry

- Access token lifetime ~3600 s; refresh at <5 min remaining (CLIProxyAPI window) or 30 min lead.
- On 401 `UNAUTHENTICATED`: refresh once, retry once, then mark the credential dead. **[L]**
- Do not cancel an in-flight refresh when the downstream client disconnects. **[V]** (CLIProxyAPI
  detaches the refresh context.)
- Single-flight refreshes per refresh token or you will burn the refresh quota / trip Google's
  "too many refresh tokens" behaviour. **[V]**

### 7.5 429 / 403 / other status semantics

Error body shape **[V]**:

```json
{
  "error": {
    "code": 429,
    "message": "You have exhausted your capacity on this model. Your quota will reset after 3s.",
    "status": "RESOURCE_EXHAUSTED",
    "details": [
      { "@type": "type.googleapis.com/google.rpc.ErrorInfo",
        "reason": "RATE_LIMIT_EXCEEDED" },
      { "@type": "type.googleapis.com/google.rpc.RetryInfo",
        "retryDelay": "3.957525076s" }
    ]
  }
}
```

CLIProxyAPI's classifier (`decideAntigravity429`) is the most complete public decision table — copy it:

| Condition | Meaning | Action |
|---|---|---|
| `error.status != RESOURCE_EXHAUSTED` | not a quota problem | soft retry |
| `ErrorInfo.reason == QUOTA_EXHAUSTED` | bucket empty | rotate credential/model, long cooldown |
| `ErrorInfo.reason == RATE_LIMIT_EXCEEDED` && no `retryDelay` | soft | short retry |
| `... && retryDelay < 3s` | transient | retry **same** credential immediately |
| `... && retryDelay < 5min` | short throttle | switch credential, cooldown = retryDelay |
| `... && retryDelay >= 5min` | effectively exhausted | treat as full exhaustion |
| body contains `quota_exhausted` / `quota exhausted` | bucket empty | full exhaustion |
| `ErrorInfo.reason == INSUFFICIENT_G1_CREDITS_BALANCE` | Google One credits gone | disable credits for this credential |

**[V]** (`antigravity_executor_credits.go`; thresholds `antigravityInstantRetryThreshold = 3s`,
`antigravityShortQuotaCooldownThreshold = 5min`.)

`403 PERMISSION_DENIED` has three distinct causes documented in Mirrowel's error text:

1. `cloudaicompanion.googleapis.com` not enabled for the project,
2. wrong project id for a paid tier,
3. account lacks permission.

plus the `x-goog-user-project` variant (§4.4). **[V]**/**[L]**

Other codes: `400 INVALID_ARGUMENT` (payload/schema), `401 UNAUTHENTICATED` (token), `403
PERMISSION_DENIED`, `404 NOT_FOUND` (model or endpoint), `412 Precondition Failed` — Mirrowel notes
412 "often means the project id is incompatible with the selected tier". **[L]**

### 7.6 Is `alt=sse` needed?

**Yes for streaming.** **[V]** every implementation appends `?alt=sse` to
`/v1internal:streamGenerateContent`. Without it you get a JSON array stream (or an error), not SSE.

⚠ Oddity: CLIProxyAPI's `buildRequest` writes `"?$alt="` (**with a `$`**) when an explicit `alt` is
supplied, and `?alt=sse` when it is not:

```go
if alt != "" { requestURL.WriteString("?$alt="); requestURL.WriteString(url.QueryEscape(alt)) }
else         { requestURL.WriteString("?alt=sse") }
```

That `$alt` looks like a typo/legacy artifact; the default path (`?alt=sse`) is the one exercised in
production. **Do not copy `?$alt=`.** **[V]**

### 7.7 `?key=` API key instead of Bearer?

**No.** **[V]** Every implementation uses `Authorization: Bearer <oauth access token>` for every
`v1internal` call, including the quota endpoints. No `key=` query parameter appears anywhere in any
implementation, and the scopes requested (`cloud-platform`) are OAuth-only. There is no API-key path
for Antigravity quota — the whole point is that it bills the user's Google account.

(For contrast, *Gemini API* proper uses `?key=`; that is a different product with a different quota
pool. Antigravity's `retrieveUserQuota` with a Gemini-CLI UA returns *Gemini CLI* buckets, but still
authenticated by Bearer.)

### 7.8 Thinking signatures must be replayed

If you send back `thought` parts without their `thoughtSignature`, or with a signature minted by a
different model/provider, you get `400 Invalid signature in thinking block`. **[V]** — CLIProxyAPI
carries a whole signature replay subsystem (`internal/signature`, `prepareAntigravityGeminiReasoningReplayPayload`,
`StripInvalidGeminiSignatureThinkingBlocks`), and NoeFabris uses the documented bypass sentinel:

```ts
export const SKIP_THOUGHT_SIGNATURE = "skip_thought_signature_validator";
```

which is an **officially supported Google feature** (documented at
<https://ai.google.dev/gemini-api/docs/thought-signatures> and used by gemini-cli and the Google .NET
SDK). **[V]** Practical rule for a stateless proxy: either replay signatures verbatim, or send
`skip_thought_signature_validator`, or drop unsigned thinking blocks before forwarding. Do not send a
*different* valid-looking signature — that is the one path that reliably 400s.

### 7.9 Wire fingerprint (HTTP/2 vs HTTP/1.1)

CLIProxyAPI deliberately forces **HTTP/1.1 with no ALPN** for Antigravity:

```go
clone.ForceAttemptHTTP2 = false
clone.TLSNextProto = make(map[string]func(authority string, c *tls.Conn) http.RoundTripper)
clone.TLSClientConfig.NextProtos = nil   // "Native Antigravity sends no ALPN extension"
```

with `IdleConnTimeout` capped well under Google Frontend's 240 s cutoff. **[V]**

⚠ **CONFLICT:** Antigravity-Manager does the opposite — it uses `rquest` with
`Emulation::Chrome123` and *enables* HTTP/2, and comments at length about GFE dropping HTTP/2 PING
frames. Both work in production. Conclusion: **protocol version is not gated**; do not spend time on
it. Python `httpx` defaults are fine. **[L]**

---

## 8. Anthropic-style (`messages`) requests — possible?

**No. Only Gemini `generateContent`.** **[V]**, with three independent confirmations:

1. The upstream body is `{"project","requestId","request":{contents,...},"model","userAgent",
   "requestType"}` — there is no `messages`, `system`, `max_tokens`, or `anthropic_version` field
   anywhere in the envelope.
2. npdk's API spec has an explicit "NOT supported" table: `anthropic_version` → unknown field,
   `messages` array → unknown field, `max_tokens` → unknown field, snake_case `system_instruction` →
   unknown field. It also has a "Antigravity vs Vertex AI Anthropic" comparison table making the same
   point.
3. Every implementation that fronts Antigravity with a Claude-shaped API does a **translation**: e.g.
   CLIProxyAPI ships `internal/translator/antigravity/claude/` and calls
   `sdktranslator.TranslateNonStream(ctx, to, responseFormat, ...)` with
   `to := sdktranslator.FromString("antigravity")`; Antigravity-Manager has
   `src-tauri/src/proxy/mappers/{gemini,claude,...}` and a `Transform` pipeline.

Note the subtlety: Claude **models** are reachable through Antigravity, and Antigravity routes them to
Anthropic-on-Vertex internally — but the *wire protocol you speak to Antigravity* is always Gemini
`contents`. `claude-*` is just a model id string. The response even comes back Gemini-shaped
(`candidates[]`), with `responseId` values like `msg_vrtx_...` hinting at the backend.

**So the plan in the brief is correct and is the only option:** translate Anthropic Messages ⇄ Gemini
`generateContent`. Points to nail in that translation:

- `assistant` → `model`; `tool_result` blocks → `functionResponse` parts in a **`model`-role** turn;
  `tool_use` blocks → `functionCall` parts.
- Anthropic `system` string/array → `systemInstruction.parts[].text`.
- `max_tokens` → `maxOutputTokens`; `stop_sequences` → `stopSequences`; `temperature`/`top_p`/`top_k`
  pass through.
- Anthropic `thinking.budget_tokens` → `generationConfig.thinkingConfig.thinkingBudget`, and you must
  enforce `maxOutputTokens > thinkingBudget`.
- Anthropic tool `input_schema` → `functionDeclarations[].parameters`, and you must strip
  `$ref`/`$defs`/`const`/`default`/`examples`/`$schema`/`$id`.
- Reverse direction: `candidates[].content.parts[]` → Anthropic `content[]` blocks, `usageMetadata` →
  `usage.{input_tokens, output_tokens, cache_read_input_tokens}`, `finishReason` → `stop_reason`
  (`STOP`→`end_turn`, `MAX_TOKENS`→`max_tokens`, `OTHER`→`tool_use` when a functionCall is present).
- Stream: no `[DONE]` terminator from upstream; you must emit Anthropic's `message_stop` yourself, and
  you should emit `message_start` before the first upstream byte.

---

## 9. Reference implementations (repo + file)

All URLs are `raw.githubusercontent.com` permalinks or stable `main` paths that I actually read while
writing this document.

### 9.1 CLIProxyAPI — `router-for-me/CLIProxyAPI` (Go, most complete production proxy)

- `internal/auth/antigravity/constants.go` — client id/secret, port 51121, scopes, endpoints
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/auth/antigravity/constants.go>
- `internal/auth/antigravity/auth.go` — auth URL, code exchange, `loadCodeAssist`, `onboardUser` polling
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/auth/antigravity/auth.go>
- `internal/misc/antigravity_version.go` — UA format, version floor `2.23.0`, auto-updater manifest
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/misc/antigravity_version.go>
- `sdk/auth/antigravity.go` — full login flow, callback server on `/oauth-callback`
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/sdk/auth/antigravity.go>
- `internal/runtime/executor/antigravity_executor.go` — base URLs, paths, client id again, code-role
  normalization, thinking-signature logic
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/runtime/executor/antigravity_executor.go>
- `internal/runtime/executor/antigravity_executor_request.go` — **the envelope** (`geminiToAntigravity`),
  URL building, `?alt=sse`, project injection, requestId/sessionId, schema sanitization
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/runtime/executor/antigravity_executor_request.go>
- `internal/runtime/executor/antigravity_executor_execute.go` — non-stream + SSE handling, Claude path
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/runtime/executor/antigravity_executor_execute.go>
- `internal/runtime/executor/antigravity_executor_stream.go` — streaming, usage filtering, terminal chunk
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/runtime/executor/antigravity_executor_stream.go>
- `internal/runtime/executor/antigravity_executor_auth.go` — **refresh** (form fields, `Go-http-client/2.0`)
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/runtime/executor/antigravity_executor_auth.go>
- `internal/runtime/executor/antigravity_executor_credits.go` — 429 taxonomy, credits, `loadCodeAssist` quota read
  <https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/runtime/executor/antigravity_executor_credits.go>
- `internal/registry/` — model catalog (`models.json`, `model_registry.go`)
  <https://github.com/router-for-me/CLIProxyAPI/tree/main/internal/registry>

### 9.2 Antigravity-Manager — `lbjlaq/Antigravity-Manager` (Rust/Tauri, ~32k stars, most faithful to the official wire format)

- `src-tauri/src/constants.rs` — UA (`antigravity/hub/2.23.1 (aidev_client; ...; cl=998881661)`),
  version floor `4.3.0`, 403-without-"antigravity" and 404-below-3.0.0 rules, `sanitize_egress_user_agent`
  <https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/main/src-tauri/src/constants.rs>
- `src-tauri/src/proxy/upstream/client.rs` — `build_url`, header set, `x-goog-user-project` + 403 retry,
  endpoint fallback rules, region-gating comment, anthropic-beta injection
  <https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/main/src-tauri/src/proxy/upstream/client.rs>
- `src-tauri/src/proxy/project_resolver.rs` — `loadCodeAssist` with 3-endpoint fallback
  <https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/main/src-tauri/src/proxy/project_resolver.rs>
- `src-tauri/src/models/official_model.rs` — `fetchAvailableModels` response struct (field-by-field)
  <https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/main/src-tauri/src/models/official_model.rs>
- `src-tauri/src/proxy/common/model_mapping.rs` — model ids, aliases, baseline filtering, tier routing
  <https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/main/src-tauri/src/proxy/common/model_mapping.rs>
- `src-tauri/src/proxy/pipeline/official_alignment_tests.rs` — **the official envelope assertions**
  (key order, `labels`, `model_enum`, `maxOutputTokens` 64000/65536, `maxOutputTokens > thinkingBudget`)
  <https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/main/src-tauri/src/proxy/pipeline/official_alignment_tests.rs>
- `src-tauri/src/proxy/model_specs.rs` — per-model limits/budgets
  <https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/main/src-tauri/src/proxy/model_specs.rs>

### 9.3 opencode-antigravity-auth — `NoeFabris/opencode-antigravity-auth` (TypeScript)

- `src/constants.ts` — client id/secret, scopes, redirect, endpoint sets, header builders, thought-signature sentinel
  <https://raw.githubusercontent.com/NoeFabris/opencode-antigravity-auth/main/src/constants.ts>
- `src/antigravity/oauth.ts` — OAuth flow
  <https://raw.githubusercontent.com/NoeFabris/opencode-antigravity-auth/main/src/antigravity/oauth.ts>
- `src/plugin/project.ts` — `loadCodeAssist` / `onboardUser` with exact headers, project caching per refresh token
  <https://raw.githubusercontent.com/NoeFabris/opencode-antigravity-auth/main/src/plugin/project.ts>
- `src/plugin/quota.ts` — **quota endpoints** (`fetchAvailableModels`, `retrieveUserQuota`) + UA switching
  <https://raw.githubusercontent.com/NoeFabris/opencode-antigravity-auth/main/src/plugin/quota.ts>
- `src/plugin/transform/model-resolver.ts` — model aliases, tier suffixes, thinking budgets
  <https://raw.githubusercontent.com/NoeFabris/opencode-antigravity-auth/main/src/plugin/transform/model-resolver.ts>
- `src/plugin/request.ts` — request building (73 KB; the full translation layer)
  <https://raw.githubusercontent.com/NoeFabris/opencode-antigravity-auth/main/src/plugin/request.ts>
- `src/plugin/config/models.ts` — model definitions with context/output limits
  <https://raw.githubusercontent.com/NoeFabris/opencode-antigravity-auth/main/src/plugin/config/models.ts>
- `docs/ANTIGRAVITY_API_SPEC.md` — the "Verified by Direct API Testing" spec (also mirrored, with
  additions, at `npdkdev/opencode-agy-auth` and `tccpc/opencode-antigravity-auth`)
  <https://raw.githubusercontent.com/NoeFabris/opencode-antigravity-auth/main/docs/ANTIGRAVITY_API_SPEC.md>

### 9.4 Others worth reading

- `looplj/axonhub` — `llm/transformer/antigravity/constants.go` (cleanest constant list; Go)
  <https://raw.githubusercontent.com/looplj/axonhub/9acca6be/llm/transformer/antigravity/constants.go>
- `Mirrowel/LLM-API-Key-Proxy` — `src/rotator_library/providers/antigravity_auth_base.py` (Python! closest
  to your stack; project/tier discovery, endpoint fallback, 403/404/412 diagnostics)
  <https://raw.githubusercontent.com/Mirrowel/LLM-API-Key-Proxy/bf7ab7e7d85595c0b85b67ea2b62522747201213/src/rotator_library/providers/antigravity_auth_base.py>
- `Wei-Shaw/sub2api` — PRs on quota-summary buckets:
  <https://github.com/Wei-Shaw/sub2api/pull/7558>, <https://github.com/Wei-Shaw/sub2api/pull/8157>
- `npdkdev/opencode-agy-auth` / `tccpc/opencode-antigravity-auth` —
  `docs/ANTIGRAVITY_API_SPEC.md` (the most readable single-document protocol writeup, incl. JSON Schema
  support matrix and error table)
  <https://raw.githubusercontent.com/npdkdev/opencode-agy-auth/HEAD/docs/ANTIGRAVITY_API_SPEC.md>
- `Draculabo/AntigravityManager` (TS/Electron) and `n2ns/antigravity-panel` (quota UI) — useful for
  quota-display semantics, not read in depth here.

---

## 10. Open questions / risks

Ordered by "probability × cost of being wrong".

1. **Model ids are a moving target, and this doc will rot.**
   Between the older sources (axonhub: `gemini-3-pro-high`; npdk spec dated 2025-12) and the current
   Antigravity-Manager (Oct 2026: `gemini-pro-agent`, `gemini-3.8-flash-*`, `claude-opus-5-5-*`) the
   lineup has been renamed **twice**. Any hard-coded list is wrong within weeks.
   *Mitigation:* resolve models at runtime from `v1internal:fetchAvailableModels`, cache with a TTL,
   and keep a small allow-list fallback. **[Risk: high]**

2. **User-Agent version gating is real and undocumented publicly.**
   Only Antigravity-Manager states it (UA without `antigravity` → 403; version `<3.0.0` → 404 on
   segmented models; therefore clamp to `4.3.0`), and CLIProxyAPI independently states a `2.23.0` floor
   for "newer models". These two floors disagree (`2.23.0` vs `3.0.0` vs `4.3.0`) and neither is
   confirmed by Google. If you pin too low you get unexplained 404/429 on new models; too high is
   currently safe.
   *Mitigation:* treat UA version as configuration, default high (`4.3.0+`), and make it a one-line
   change. **[Risk: high]**

3. **The `redirect_uri` path and port may have changed / may be one of two spellings.**
   `/oauth-callback` on 51121 is majority-reported but Mirrowel says `/oauthcallback`. Google's loopback
   matching is exact, so one of them 400s with `redirect_uri_mismatch`. Also, if Google ever moves
   Antigravity to ephemeral ports (as some desktop clients do), a hard-coded 51121 breaks.
   *Mitigation:* make port and path configurable; surface the `redirect_uri_mismatch` error verbatim so
   the operator can flip the path. **[Risk: medium-high, but fails loudly and immediately]**

4. **`Client-Metadata` / `X-Goog-Api-Client` requirement is genuinely unknown.**
   Two production proxies omit both on content requests; two others send both. Nobody documents a
   failure caused by their absence *or* presence. Similarly the "`google-api-nodejs-client` UA is
   required for standard-tier detection" claim is a single-source assertion.
   *Mitigation:* start minimal (no `Client-Metadata`), add it if you observe free-tier-only quota on an
   account that should be paid. **[Risk: medium]**

5. **`labels` / `model_enum` may matter for quota attribution.**
   Antigravity-Manager reconstructs the official `request.labels` (including `model_enum` →
   `MODEL_PLACEHOLDER_*`, `used_claude`, `trajectory_id`, `request_id`, `last_step_index`) from captures,
   implying the official client always sends it. CLIProxyAPI omits it and works — but "works" here means
   "gets answers", not necessarily "is accounted identically". If Google starts gating on labels, a
   label-less proxy could be mis-billed or throttled.
   *Mitigation:* implement behind a flag; the mapping table (`model_enum` per model) must be refreshed
   from the catalog. **[Risk: medium, latent]**

6. **Thinking-signature replay is the most common 400 in practice.**
   Any conversation that reuses thinking blocks across credentials/models needs either verbatim
   signature replay, a signature cache, or the `skip_thought_signature_validator` sentinel. A naive
   stateless translator will 400 on multi-turn thinking conversations, and the error text
   ("Invalid signature in thinking block") does not say which turn.
   *Mitigation:* default to *dropping* unsigned thinking blocks for the translated path, or send the
   sentinel. **[Risk: medium-high for a Claude-Messages front end]**

7. **`maxOutputTokens > thinkingBudget` is a hard 400 and easy to violate.**
   A downstream Claude client can legitimately send `max_tokens=1024` with `budget_tokens=8192`.
   Forwarding both verbatim fails. Antigravity-Manager's behaviour (raise `maxOutputTokens` to
   `budget + 1024`, cap at the model's max, otherwise shrink the budget) is the reference.
   **[Risk: medium, but trivially fixable once known]**

8. **Region gating makes "it works for me" unreliable.**
   Sandbox hosts 400 for non-US IPs; a proxy deployed in the EU/APAC that defaults to sandbox will look
   broken while the developer's laptop works. This is exactly the bug class that produced
   Antigravity-Manager's "daily before sandbox" ordering.
   *Mitigation:* never put a sandbox host first; distinguish `400 + "User location is not supported"`
   from other 400s. **[Risk: medium]**

9. **Endpoint preference will drift.** Today: daily for content, prod for `loadCodeAssist`. Earlier
   sources: prod for everything. Antigravity-Manager claims prod 429s all content traffic; CLIProxyAPI
   defaults content to daily too, but keeps prod as a fallback. Nobody explains *why* prod 429s.
   *Mitigation:* health-tracked, configurable ordering. **[Risk: low-medium]**

10. **Quota/billing semantics for a shared-credential proxy are unverified.**
    Antigravity quota is per Google account; a gateway that multiplexes many downstream users onto one
    Antigravity credential will hit one shared bucket, and `remainingFraction` is the *only* signal
    (no per-request accounting endpoint that I could read). Terms-of-service risk is out of scope here
    but should be a conscious decision for MultiGateway. **[Risk: product-level]**

11. **`retrieveUserQuotaSummary` and the "5h/weekly" buckets are second-hand only.**
    Seen in PR titles, never in source. If the UI is supposed to show a "weekly limit", that shape is
    **[U]** and must be captured from a live account first.

12. **`?$alt=`** in CLIProxyAPI's explicit-`alt` branch is either a typo or a real (undocumented)
    parameter. Untested here. Use `?alt=sse` only.

---

## Appendix A — copy-paste constant block

Everything below is **[V]** unless noted; `#` comments cite the source repo.

```python
CLIENT_ID     = "<GOOGLE_OAUTH_CLIENT_ID>"
CLIENT_SECRET = "<GOOGLE_OAUTH_CLIENT_SECRET>"

AUTHORIZE_URL = "https://accounts.google.com/o/oauth2/v2/auth"
TOKEN_URL     = "https://oauth2.googleapis.com/token"
USERINFO_URL  = "https://www.googleapis.com/oauth2/v1/userinfo"   # ?alt=json on v2 also works
REDIRECT_URI  = "http://localhost:51121/oauth-callback"            # port is FIXED

SCOPES = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
]

# hosts
HOST_PROD    = "https://cloudcode-pa.googleapis.com"
HOST_DAILY   = "https://daily-cloudcode-pa.googleapis.com"
HOST_SANDBOX = "https://daily-cloudcode-pa.sandbox.googleapis.com"
HOST_AUTOPUSH= "https://autopush-cloudcode-pa.sandbox.googleapis.com"   # usually dead

# paths  (note the ':' — gRPC transcoding style)
P_LOAD_CODE_ASSIST = "/v1internal:loadCodeAssist"
P_ONBOARD_USER     = "/v1internal:onboardUser"
P_GENERATE         = "/v1internal:generateContent"
P_STREAM           = "/v1internal:streamGenerateContent"   # always append ?alt=sse
P_FETCH_MODELS     = "/v1internal:fetchAvailableModels"
P_USER_QUOTA       = "/v1internal:retrieveUserQuota"
P_COUNT_TOKENS     = "/v1internal:countTokens"

# content: daily first, then sandbox, then prod  (health-tracked, see §7.3)
CONTENT_HOSTS    = [HOST_DAILY, HOST_SANDBOX, HOST_PROD]
# loadCodeAssist: prod first
LOAD_HOSTS       = [HOST_PROD, HOST_DAILY, HOST_SANDBOX]

USER_AGENT = ("antigravity/hub/2.23.1 "
              "(aidev_client; os_type=darwin; arch=arm64; cl=998881661)")
# ^ must contain 'antigravity'; version must be >= 2.23.0 and ideally >= 4.3.0

DEFAULT_PROJECT_ID = "rising-fact-p41fc"    # last-resort fallback only

# envelope defaults
BODY_USER_AGENT  = "antigravity"
BODY_REQUEST_TYPE = "agent"                  # "image_gen" for image models
```

## Appendix B — minimal request, complete

```http
POST /v1internal:streamGenerateContent?alt=sse HTTP/1.1
Host: daily-cloudcode-pa.googleapis.com
Authorization: Bearer ya29....
Content-Type: application/json
User-Agent: antigravity/hub/2.23.1 (aidev_client; os_type=darwin; arch=arm64; cl=998881661)
Accept: text/event-stream

{
  "project": "my-project-123456",
  "requestId": "agent/3f2b1c0e-1111-2222-3333-444455556666/1790530802878/0181971f-961c-4ba9-a320-2ae44361bc1d/1",
  "request": {
    "contents": [
      { "role": "user", "parts": [ { "text": "Explain this stack trace" } ] }
    ],
    "systemInstruction": {
      "parts": [ { "text": "You are a helpful coding assistant." } ]
    },
    "generationConfig": {
      "maxOutputTokens": 8192,
      "temperature": 0.7,
      "thinkingConfig": { "thinkingBudget": 2048, "includeThoughts": true }
    },
    "sessionId": "-1234567890123456789"
  },
  "model": "gemini-3.8-flash-high",
  "userAgent": "antigravity",
  "requestType": "agent"
}
```

Streamed reply (one frame per `data:` line; **no `[DONE]`** — you synthesize the terminator):

```text
data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"The"}]}}],"usageMetadata":{"promptTokenCount":16,"candidatesTokenCount":1,"totalTokenCount":17}},"traceId":"abc"}

data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":" issue"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":16,"candidatesTokenCount":2,"totalTokenCount":18,"thoughtsTokenCount":37},"modelVersion":"gemini-3.8-flash-high","responseId":"ypM9abPqFKWl0-kPvamgqQw"},"traceId":"abc"}
```
