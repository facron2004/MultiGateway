# Native provider contract (gateway/native)

This document is the integration contract for OAuth providers that run
**inside** the MultiGateway process (as opposed to the vendored Trae/Qoder
backends, which are separate HTTP apps). Read it together with
`gateway/native/base.py`, `gateway/native/translate.py` and the research note for
your provider.

## Why this layer exists

The gateway exposes one unified surface:

| Client protocol | Gateway entry | Provider method |
| --- | --- | --- |
| OpenAI chat.completions | `POST /v1/chat/completions` | `chat(payload)` → OpenAI SSE frames |
| OpenAI Responses | `POST /v1/responses` | same, gateway converts |
| Anthropic Messages | `POST /v1/messages` | `messages(payload)` → Anthropic SSE frames |

`chat()` is the only protocol method most providers implement. The base class
derives `messages()` automatically by translating Anthropic → OpenAI
(`translate.anthropic_to_chat`) and OpenAI stream frames → Anthropic events
(`translate.chat_frames_to_anthropic`).

## Provider class

Create `gateway/native/<name>.py` with one subclass of
`gateway.native.base.NativeProvider` or `OpenAIProvider`:

```python
from .base import OpenAIProvider


class MyProvider(OpenAIProvider):
    name = "myprovider"          # lowercase, used in model prefixes and URLs
    label = "My Provider"        # shown in the dashboard
    region = "intl"              # "intl" or "cn"; native providers have one region
    login_kind = "device"        # "device" or "loopback" (diagnostic only)
    capabilities = {"login": True, "import": True, "credits": False,
                    "regions": False, "checkin": False, "credentials": True}
    chat_path = "/v1/chat/completions"
    models_path = "/v1/models"
```

### Must implement

| Member | Contract |
| --- | --- |
| `url(path)` | Full upstream URL for an API path. |
| `fetch_models()` | Return `list[str]` or `list[dict]` with `id` keys. Must raise `ProviderError` (not raw exceptions) on failure. Static fallback list is fine. |
| `start_login(region=None)` | Return `{"login_id", "url", "user_code"?, "interval", "expires_at"}`. `url` must be `https://…` or `http://…`; the dashboard opens it in the system browser. |
| `poll_login(login_id)` | Return `{"status": "pending"|"authenticated", "interval"?}` or raise `ProviderError` for terminal failures (`expired_token`, `access_denied`). |
| `refresh_grant(row)` | Exchange `row["refresh_token"]` for `{"access_token", "refresh_token"?, "expires_at", ...}`. |
| `extra_headers(token, row)` | Vendor-specific headers added to every upstream request. |

### Inherited helpers (use them, don't reimplement)

* `self.pool` — `AccountPool`: `records()`, `save(credentials, account_id=None)`,
  `find(id)`, `choose()`, `mutate()`. One JSON file per account in
  `<gateway-data>/native/<name>/accounts/<id>.json`.
* `self.request(path, payload, headers=None, account_id=None, method=None, timeout=600)`
  — sends an authenticated request, rotating accounts on 401/403/429,
  refreshing expired tokens, and setting `response.gateway_account` so the usage
  board can attribute the request.
* `self.relay(response)` — yields upstream OpenAI SSE as `data: {...}\n\n`
  frames, closes the response, terminates with `data: [DONE]\n\n`.
* `self.loopback_start(port, redirect_uri, build_url, exchange, pkce=True)` and
  `self.poll_login` delegating to `self._loopback_poll(login_id, exchange)` for
  authorization-code providers (see `base.py`). `LoopbackCallback` binds
  `127.0.0.1` and `::1`, and accepts any callback path.
* `oauth.pkce_pair()`, `oauth.random_state()`, `oauth.add_query(url, params)`,
  `oauth.google_device_login(...)`, `oauth.device_token_request(...)`,
  `oauth.refresh_request(...)`, `oauth.expires_at_of(grant)`.
* `files.atomic_json` for any extra local state.
* `translate.anthropic_to_chat` / `translate.chat_frames_to_anthropic` when the
  upstream is not OpenAI-compatible (e.g. Gemini `generateContent`).

### Credential record

Whatever you pass to `pool.save()` is stored verbatim. Required keys:
`id` (stable account identity), `access_token`, and either `refresh_token` or a
future `expires_at`. Recommended: `email`/`label`, `expires_at`, plus any
vendor state needed for refresh (e.g. project id, device id). **Never log or
return tokens**; `account_views()` already exposes only safe fields.

### `chat(payload, session=None)`

`payload` is an OpenAI `chat.completions` body (`messages`, `tools`, `stream`,
sampling fields). You must:

1. Set `stream=True` upstream and ask for usage (`stream_options`), or synthesize
   final usage yourself.
2. Return an **iterator of bytes**, each frame being
   `b"data: " + json.dumps(chunk) + b"\n\n"` (OpenAI chat chunk shape), ending
   with `b"data: [DONE]\n\n"`.
3. Emit `usage` (prompt/completion tokens, cached tokens when known) at least
   once; the usage board and cost estimate depend on it.
4. Map tool calls into `delta.tool_calls` with stable `index` values, and
   reasoning into `delta.reasoning_content` when available.
5. Close the upstream response when the generator is closed (client disconnect).
6. Raise `ProviderError(message, status, code)` for upstream errors; the handler
   turns it into an SSE error frame mid-stream.

The iterator must be lazy: do not read the whole stream before returning.

## Testing requirements

* Add `gateway/tests/test_native_<name>.py` using `unittest`.
* No real network and no real credentials: start a local
  `ThreadingHTTPServer` mock that speaks the vendor protocol and point the
  provider at it (`provider.url = lambda path: origin + path` style override, or
  patch module-level base URL constants).
* Cover: OAuth start/poll happy path + denial/expiry, refresh-on-expiry,
  account rotation on 401, `chat()` frame shape for text + tool call + usage,
  `messages()` translation (via the base class) and `fetch_models()`.
* Tests must run in a temp dir; set `directory` via constructor.
* Run: `py -3.13 -m unittest gateway.tests.test_native_<name> -v` and the full
  suite `py -3.13 -m unittest gateway.tests.test_gateway`.

## Ownership boundaries

The Lead owns and edits: `base.py`, `translate.py`, `oauth.py`, `pool.py`,
`__init__.py`, `runtime.py`, `handler.py`, `control.py`, `ports.py`, dashboard
UI, docs. Do not edit those; message the Lead if a base change is needed.

Your files: `gateway/native/<name>.py`, optional helper module
(`gateway/native/<name>_*.py`), `gateway/tests/test_native_<name>.py`.

## Code style

* Python 3.11+, standard library only (the gateway ships as a single exe).
* Module docstring explaining the vendor flow; short comments for non-obvious
  protocol details.
* No network at import time, no background threads unless required (document
  them and stop them in `stop()`).
* Thread-safety: one provider instance serves concurrent requests; guard
  mutable state with `self.lock` (`threading.RLock`).
* Chinese user-facing error messages, English code/docstrings (match the repo).
