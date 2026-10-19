"""Google Antigravity account provider (OAuth + Cloud Code Assist).

The provider talks to the same upstream the Antigravity IDE talks to, so the
whole flow is a three-step dance:

1. **OAuth** — a Google "installed application" loopback login on the fixed
   registered port 51121 (``http://localhost:51121/oauth-callback``), with
   ``access_type=offline`` + ``prompt=consent`` so Google always returns a
   refresh token. This client is not registered for PKCE, so no challenge is
   sent. The browser is redirected to a short-lived listener
   (:class:`gateway.native.oauth.LoopbackCallback`, which accepts any path), the
   authorization code is exchanged at ``https://oauth2.googleapis.com/token``
   and the account email is read from the userinfo endpoint.
2. **Project discovery** — every content call needs a Cloud Code project id;
   ``v1internal:loadCodeAssist`` returns it, and ``v1internal:onboardUser``
   provisions one for accounts that have none. The value is cached on the
   credential record (``extra.project_id``) and only resolved when missing,
   because ``loadCodeAssist`` is slow and rate-limited.
3. **Content** — the OpenAI chat body is translated into the Gemini
   ``generateContent`` envelope (``{"project", "requestId", "request", "model",
   "userAgent", "requestType"}``), POSTed to
   ``/v1internal:streamGenerateContent?alt=sse`` and the ``{"response": ...}``
   SSE chunks are unwrapped into OpenAI chat frames, including
   ``reasoning_content`` from thought parts, ``tool_calls`` from
   ``functionCall`` parts and a final usage chunk.

Research notes and the constants below live in docs/research/antigravity.md.
Two choices worth knowing about while reading this module:

* The documented callback path is ``/oauth-callback`` (three of the four
  reference implementations use it). One implementation claims
  ``/oauthcallback``, so the login *alternates* to that spelling after an
  attempt expires without ever reaching the listener — a rejected redirect
  surfaces only as a browser error, never as an OAuth response we could read.
* Content requests go to the "daily" host first, ``loadCodeAssist`` to "prod",
  and an unhealthy host falls back per research §7.3. Sandbox is never tried
  first because it answers 400 ``User location is not supported`` outside the
  US.
* A missing project is not fatal: the envelope tolerates an omitted
  ``project`` field (the reference proxies delete it when unknown), so a
  project-discovery hiccup never fails a chat that would otherwise work.
"""
import hashlib
import json
import os
import platform
import secrets
import sys
import threading
import time
import uuid
from pathlib import Path

from dotenv import dotenv_values

from ..transport import ProviderError, json_request, sse_events
from . import gemini, oauth
from .base import NativeProvider
from .quota import quota_number, quota_summary

# --------------------------------------------------------------------------- OAuth
# Read local OAuth settings beside the source launcher or installed executable.
_CLIENT_BASE = Path(sys.executable).parent if getattr(sys, "frozen", False) else Path(__file__).resolve().parents[2]
_CLIENT_CONFIG = dotenv_values(_CLIENT_BASE / ".env")
CLIENT_ID = os.environ.get("ANTIGRAVITY_OAUTH_CLIENT_ID") or _CLIENT_CONFIG.get("ANTIGRAVITY_OAUTH_CLIENT_ID", "")
CLIENT_SECRET = os.environ.get("ANTIGRAVITY_OAUTH_CLIENT_SECRET") or _CLIENT_CONFIG.get("ANTIGRAVITY_OAUTH_CLIENT_SECRET", "")

AUTHORIZE_URL = "https://accounts.google.com/o/oauth2/v2/auth"
TOKEN_URL = "https://oauth2.googleapis.com/token"
USERINFO_URL = "https://www.googleapis.com/oauth2/v1/userinfo"

#: Token refresh is performed by Go's default HTTP client in the official
#: binary; matching the fingerprint costs nothing (research §2.2).
TOKEN_USER_AGENT = "Go-http-client/2.0"

#: Fixed, registered loopback port. Google matches the redirect URI exactly, so
#: this cannot be ephemeral without a redirect_uri_mismatch from the consent page.
CALLBACK_PORT = 51121
REDIRECT_BASE = "http://localhost"
REDIRECT_PATHS = ("/oauth-callback", "/oauthcallback")

SCOPES = ("https://www.googleapis.com/auth/cloud-platform",
          "https://www.googleapis.com/auth/userinfo.email",
          "https://www.googleapis.com/auth/userinfo.profile",
          "https://www.googleapis.com/auth/cclog",
          "https://www.googleapis.com/auth/experimentsandconfigs")

LOGIN_TIMEOUT = 300
LOGIN_INTERVAL = 2

# --------------------------------------------------------------- Cloud Code Assist
HOST_PROD = "https://cloudcode-pa.googleapis.com"
HOST_DAILY = "https://daily-cloudcode-pa.googleapis.com"
HOST_SANDBOX = "https://daily-cloudcode-pa.sandbox.googleapis.com"

LOAD_CODE_ASSIST = "/v1internal:loadCodeAssist"
ONBOARD_USER = "/v1internal:onboardUser"
STREAM_GENERATE = "/v1internal:streamGenerateContent?alt=sse"
FETCH_MODELS = "/v1internal:fetchAvailableModels"

#: ``loadCodeAssist``/``onboardUser`` are polling endpoints; the interval is a
#: module constant so tests do not have to sleep.
ONBOARD_INTERVAL = 0.5
ONBOARD_ATTEMPTS = 5

#: Header the official client sends with model catalogs and onboarding.
API_CLIENT = "google-cloud-sdk vscode_cloudshelleditor/0.1"
IDE_METADATA = {"ideType": "ANTIGRAVITY"}

#: Upstream privilege header Antigravity adds for Claude model ids (research §4.6).
ANTHROPIC_BETA = "claude-code-20250219"

#: User-Agent floors: a UA without "antigravity" is rejected with 403 and a
#: version below 3.0.0 gets 404 on tiered models, so the version is pinned high.
HUB_VERSION = "4.3.0"
HUB_CLIENT = "998881661"

#: Fallback catalog used while ``fetchAvailableModels`` is unavailable: the
#: upstream catalog churns (it was renamed twice in 2026) and is resolved at
#: runtime whenever an account exists, so this list only has to be plausible.
DEFAULT_MODELS = (
    {"id": "gemini-3.1-pro-high", "display_name": "Gemini 3.1 Pro (High)",
     "context_window": 1048576, "max_output_tokens": 65536},
    {"id": "gemini-3.1-pro-low", "display_name": "Gemini 3.1 Pro (Low)",
     "context_window": 1048576, "max_output_tokens": 65536},
    {"id": "gemini-3-pro-high", "display_name": "Gemini 3 Pro (High)",
     "context_window": 1048576, "max_output_tokens": 65536},
    {"id": "gemini-3-pro-medium", "display_name": "Gemini 3 Pro (Medium)",
     "context_window": 1048576, "max_output_tokens": 65536},
    {"id": "gemini-3-pro-low", "display_name": "Gemini 3 Pro (Low)",
     "context_window": 1048576, "max_output_tokens": 65536},
    {"id": "gemini-3-flash", "display_name": "Gemini 3 Flash",
     "context_window": 1048576, "max_output_tokens": 65536},
    {"id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6",
     "context_window": 200000, "max_output_tokens": 64000},
    {"id": "claude-opus-4-6-thinking", "display_name": "Claude Opus 4.6 (Thinking)",
     "context_window": 200000, "max_output_tokens": 64000},
    {"id": "gpt-oss-120b-medium", "display_name": "GPT-OSS 120B (Medium)",
     "context_window": 131072, "max_output_tokens": 32768},
)


def require_oauth_client():
    if not CLIENT_ID or not CLIENT_SECRET:
        raise ProviderError("请在 .env 或环境变量中设置 ANTIGRAVITY_OAUTH_CLIENT_ID 和 ANTIGRAVITY_OAUTH_CLIENT_SECRET", 503)


def user_agent():
    """Return the ``antigravity/<hub version>`` User-Agent the upstream expects."""
    system = platform.system().lower() or "windows"
    os_type = "macos" if system == "darwin" else system
    machine = platform.machine().lower()
    arch = "arm64" if machine in ("arm64", "aarch64") else "amd64"
    return "antigravity/hub/%s (aidev_client; os_type=%s; arch=%s; cl=%s)" % (
        HUB_VERSION, os_type, arch, HUB_CLIENT)


def endpoint_hosts(path):
    """Return the host preference list for one ``v1internal`` method (§7.3)."""
    if any(method in path for method in (":loadCodeAssist", ":onboardUser")):
        return [HOST_PROD, HOST_DAILY, HOST_SANDBOX]
    return [HOST_DAILY, HOST_SANDBOX, HOST_PROD]


class Antigravity(NativeProvider):
    """Antigravity provider: OAuth login, project discovery and Gemini streaming."""

    name = "antigravity"
    label = "Antigravity"
    region = "intl"
    login_kind = "loopback"
    capabilities = {"login": True, "import": True, "credits": True, "regions": False,
                    "checkin": False, "credentials": True}
    model_ttl = 300

    def __init__(self, directory):
        super().__init__(directory)
        #: One resolution lock per account so concurrent chats do not all call
        #: ``loadCodeAssist`` for the same credential.
        self._project_locks = {}

    # ---------------------------------------------------------------- endpoints
    def url(self, path):
        """Full upstream URL; absolute URLs (host fallbacks) pass through."""
        if "://" in path:
            return path
        return endpoint_hosts(path)[0] + path

    def extra_headers(self, token, row=None):
        return {"User-Agent": user_agent(), "Accept": "*/*"}

    # -------------------------------------------------------------------- login
    def start_login(self, region=None):
        """Open the loopback listener and return the Google consent URL."""
        require_oauth_client()
        # A second click must not fail on the busy callback port: drop the
        # previous attempt first.
        self.stop()
        redirect_path = self._redirect_path()
        redirect_uri = "%s:%s%s" % (REDIRECT_BASE, CALLBACK_PORT, redirect_path)

        def build_url(state, challenge):
            # ``challenge`` is always empty: this client is not registered for PKCE.
            return oauth.add_query(AUTHORIZE_URL, {
                "client_id": CLIENT_ID,
                "redirect_uri": redirect_uri,
                "response_type": "code",
                "scope": " ".join(SCOPES),
                "state": state,
                "access_type": "offline",
                "prompt": "consent",
                "include_granted_scopes": "true",
            })

        return self.loopback_start(CALLBACK_PORT, redirect_uri, build_url, self._exchange,
                                   interval=LOGIN_INTERVAL, timeout=LOGIN_TIMEOUT, pkce=False)

    def poll_login(self, login_id):
        """Finish the login once the browser hits the loopback listener."""
        with self.lock:
            login = self.pending.get(login_id)
            callback = None if login is None else login.get("callback")
        if callback is not None and callback.error:
            # The base poll only reports "pending" until the login expires;
            # a denial is terminal and must surface immediately.
            self._drop_login(login_id)
            raise ProviderError("Antigravity 授权被拒绝：%s" % callback.error, 400, "access_denied")
        try:
            return self._loopback_poll(login_id, self._exchange)
        except ProviderError as exc:
            if exc.code == "expired_token" and login is not None:
                try:
                    self._redirect_path_index(advance=True)
                except OSError:
                    pass  # the callback retry is a nicety; never mask the login error
            raise
        except OSError as exc:
            raise ProviderError("Antigravity 授权凭证保存失败，请检查磁盘空间后重试", 500) from exc

    def _drop_login(self, login_id):
        with self.lock:
            login = self.pending.pop(login_id, None)
        callback = (login or {}).get("callback")
        if callback is not None:
            callback.stop()

    def _redirect_path(self):
        """Return the callback path for the next attempt (documented one first)."""
        return REDIRECT_PATHS[self._redirect_path_index()]

    def _redirect_path_index(self, advance=False):
        """Read — or advance — the persisted callback-path preference."""
        config = self.load_config()
        index = config.get("redirect_path_index")
        if not isinstance(index, int) or not 0 <= index < len(REDIRECT_PATHS):
            index = 0
        if not advance:
            return index
        index = (index + 1) % len(REDIRECT_PATHS)
        config["redirect_path_index"] = index
        self.save_config(config)
        return index

    def _exchange(self, query, login):
        """Turn the captured ``code`` into a stored credential record."""
        code = (query or {}).get("code")
        if not code:
            raise ProviderError("Antigravity 授权回调缺少授权码", 400, "access_denied")
        grant = self._token_request({
            "code": code,
            "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET,
            "redirect_uri": login.get("redirect_uri", ""),
            "grant_type": "authorization_code",
        })
        access_token = grant.get("access_token")
        if not access_token:
            raise ProviderError("Antigravity 授权未返回访问令牌，请重试", 502)
        profile = self._profile(access_token)
        email = str(profile.get("email") or "")
        credentials = {
            "id": str(profile.get("sub") or email or uuid.uuid4().hex),
            "access_token": access_token,
            "refresh_token": str(grant.get("refresh_token") or ""),
            "expires_at": oauth.expires_at_of(grant),
            "email": email,
            "label": str(profile.get("name") or email or "Antigravity 账号"),
            "extra": {},
        }
        if not credentials["refresh_token"]:
            credentials["error"] = "Google 未返回刷新令牌，访问令牌过期后需要重新授权"
        return credentials

    def _profile(self, access_token):
        """Read the Google profile used for the account label; never fatal."""
        try:
            data = json_request(USERINFO_URL, None, timeout=20,
                                headers={"Authorization": "Bearer " + access_token,
                                         "User-Agent": user_agent()})
        except ProviderError:
            return {}
        return data if isinstance(data, dict) else {}

    def _token_request(self, payload):
        """POST a form-encoded grant to Google's token endpoint."""
        require_oauth_client()
        try:
            grant = oauth.refresh_request(TOKEN_URL, payload, form=True, timeout=30,
                                          headers={"User-Agent": TOKEN_USER_AGENT})
        except ProviderError as exc:
            status = exc.status if 400 <= exc.status < 500 else 502
            raise ProviderError("Antigravity 令牌请求失败：%s" % exc, status, exc.code) from exc
        if not isinstance(grant, dict):
            raise ProviderError("Antigravity 令牌响应格式无效", 502)
        return grant

    # ------------------------------------------------------------------ refresh
    def refresh_grant(self, row):
        """Exchange the stored refresh token for a new access token."""
        refresh_token = str(row.get("refresh_token") or "")
        if not refresh_token:
            raise ProviderError("Antigravity 账号缺少刷新令牌，请重新登录授权", 401, "invalid_grant")
        grant = self._token_request({
            "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        })
        access_token = grant.get("access_token")
        if not access_token:
            raise ProviderError("Antigravity 刷新令牌未返回访问令牌，请重新登录授权", 401,
                                "invalid_grant")
        return {
            "access_token": access_token,
            # Google normally does not rotate the refresh token; keep the old
            # one when the response omits it.
            "refresh_token": str(grant.get("refresh_token") or refresh_token),
            "expires_at": oauth.expires_at_of(grant),
        }

    # ------------------------------------------------------------------ project
    def project_id(self, row):
        """Return the account's Cloud Code project, resolving it at most once."""
        cached = str((row.get("extra") or {}).get("project_id") or "")
        if cached:
            return cached
        lock = self._project_lock(row["id"])
        with lock:
            try:
                _, current = self.pool.find(row["id"])
            except ProviderError:
                return ""
            cached = str((current.get("extra") or {}).get("project_id") or "")
            if cached:
                return cached
            project = self._discover_project(row["id"])
            if project:
                self._store_project(row["id"], project)
            return project

    def _project_lock(self, account_id):
        with self.lock:
            return self._project_locks.setdefault(account_id, threading.Lock())

    def _discover_project(self, account_id=None):
        """``loadCodeAssist`` then ``onboardUser``; empty when unknown."""
        try:
            info = self._post(LOAD_CODE_ASSIST, {"metadata": IDE_METADATA}, account_id=account_id)
            project = project_of(info)
            if project:
                return project
            tier = tier_of(info)
            if not tier:
                return ""
            return self._onboard(tier, account_id=account_id)
        except ProviderError:
            # A missing project is valid (the envelope omits it), so a discovery
            # failure must not fail a request that could still succeed.
            return ""

    def _onboard(self, tier, account_id=None):
        """Poll ``onboardUser`` until the long-running operation completes."""
        payload = {"tierId": tier, "metadata": IDE_METADATA}
        headers = {"X-Goog-Api-Client": API_CLIENT}
        for attempt in range(ONBOARD_ATTEMPTS):
            if attempt and ONBOARD_INTERVAL:
                time.sleep(ONBOARD_INTERVAL)
            result = self._post(ONBOARD_USER, payload, headers=headers, timeout=60,
                                account_id=account_id)
            if result.get("done"):
                return project_of(result.get("response") or {})
        return ""

    def _store_project(self, account_id, project):
        """Persist ``extra.project_id`` on the credential record."""
        try:
            _, row = self.pool.find(account_id)
        except ProviderError:
            return
        extra = dict(row.get("extra") or {})
        if extra.get("project_id") == project:
            return
        extra["project_id"] = project
        row["extra"] = extra
        try:
            self.pool.save(row, account_id=account_id)
        except OSError:
            # Caching is an optimisation: a read-only or full disk must not turn
            # a working chat into a failure. The value is retried on the next call.
            pass

    # ------------------------------------------------------------------- models
    def fetch_quota(self, account_id):
        row = self._access(account_id)
        project = self.project_id(row)
        data = self._post(FETCH_MODELS, {"project": project} if project else {}, account_id=account_id)
        if isinstance(data, dict) and isinstance(data.get("response"), dict):
            data = data["response"]
        catalog = data.get("models") if isinstance(data, dict) else None
        buckets = []
        for identifier, info in (catalog.items() if isinstance(catalog, dict) else ()):
            quota = info.get("quotaInfo") if isinstance(info, dict) else None
            fraction = quota_number(quota.get("remainingFraction")) if isinstance(quota, dict) else None
            if fraction is None:
                continue
            buckets.append({"label": info.get("displayName") or identifier,
                            "remaining": max(0, min(100, fraction * 100)), "unit": "%",
                            "reset_at": quota.get("resetTime")})
        buckets.sort(key=lambda bucket: bucket["remaining"])
        return quota_summary(buckets, label="最低模型剩余")

    def fetch_models(self):
        """Return the server catalog, falling back to :data:`DEFAULT_MODELS`."""
        try:
            row = self._access()
            project = self.project_id(row)
            data = self._post(FETCH_MODELS, {"project": project} if project else {})
            rows = model_rows(data)
            if rows:
                return rows
        except ProviderError:
            pass
        return [dict(model) for model in DEFAULT_MODELS]

    # --------------------------------------------------------------------- chat
    def chat(self, payload, session=None):
        """Yield OpenAI chat SSE frames for one chat.completions request."""
        return self._stream(payload, session)

    def _stream(self, payload, session):
        """Lazy worker for :meth:`chat`; nothing is sent until the first frame."""
        model = str(payload.get("model") or "").strip()
        if model.startswith(self.name + "/"):
            model = model.split("/", 1)[1]
        if not model:
            raise ProviderError("缺少模型名称", 400)
        row = self._access()
        project = self.project_id(row)
        body = self._envelope(payload, model, project, session)
        headers = {"Accept": "text/event-stream"}
        if "claude" in model.lower():
            headers["anthropic-beta"] = ANTHROPIC_BETA
        response = self._open(STREAM_GENERATE, body, headers=headers)
        yield from self._frames(response, model)

    def _envelope(self, payload, model, project, session):
        """Build the Cloud Code Assist request wrapper (research §5.1)."""
        request = gemini.build_request(dict(payload, model=model))
        if "tools" in request and "claude" in model.lower():
            # Claude rejects multi-branch tool schemas unless they are validated.
            request["toolConfig"] = {"functionCallingConfig": {"mode": "VALIDATED"}}
        request["sessionId"] = session_id(session)
        body = {}
        if project:
            body["project"] = project
        body["requestId"] = request_id()
        body["request"] = request
        body["model"] = model
        body["userAgent"] = "antigravity"
        body["requestType"] = "agent"
        return body

    def _frames(self, response, model):
        """Unwrap ``{"response": ...}`` SSE chunks into OpenAI chat frames."""
        identifier = "chatcmpl-" + uuid.uuid4().hex[:24]
        created = int(time.time())
        finish_reason = "stop"
        usage = None
        tool_index = 0
        emitted_tool = False
        try:
            yield delta_frame(identifier, created, model, {"role": "assistant"})
            for event in events_of(response):
                payload = event.get("response") if isinstance(event, dict) else None
                if payload is None:
                    payload = event
                error = payload.get("error") if isinstance(payload, dict) else None
                if error:
                    raise ProviderError("Antigravity 服务返回错误：%s" % error_text(error), 502)
                if isinstance(payload, dict):
                    report = gemini.usage(payload.get("usageMetadata"))
                    # Usage is cumulative; keep the largest report so a trailing
                    # zero-filled chunk cannot wipe the real totals.
                    if report and (usage is None or report["total_tokens"]
                                   >= usage["total_tokens"]):
                        usage = report
                candidate = first_candidate(payload)
                if candidate is not None:
                    parts = (candidate.get("content") or {}).get("parts") or []
                    for part in parts:
                        if not isinstance(part, dict):
                            continue
                        text = part.get("text")
                        if isinstance(text, str) and text:
                            if part.get("thought"):
                                yield delta_frame(identifier, created, model,
                                                  {"reasoning_content": text})
                            else:
                                yield delta_frame(identifier, created, model, {"content": text})
                        call = part.get("functionCall")
                        if isinstance(call, dict) and call.get("name"):
                            emitted_tool = True
                            yield delta_frame(identifier, created, model, {"tool_calls": [{
                                "index": tool_index,
                                "id": str(call.get("id") or "call_" + uuid.uuid4().hex[:16]),
                                "type": "function",
                                "function": {"name": str(call["name"]),
                                             "arguments": call_arguments(call.get("args"))},
                            }]})
                            tool_index += 1
                    reason = candidate.get("finishReason")
                    if isinstance(reason, str) and reason:
                        finish_reason = gemini.FINISH_REASONS.get(reason, "stop")
        finally:
            response.close()
        if emitted_tool and finish_reason == "stop":
            finish_reason = "tool_calls"
        yield delta_frame(identifier, created, model, {}, finish_reason)
        yield usage_frame(identifier, created, model,
                          usage or {"prompt_tokens": 0, "completion_tokens": 0,
                                    "total_tokens": 0})
        yield b"data: [DONE]\n\n"

    # ----------------------------------------------------------------- transport
    def _open(self, path, payload, headers=None, timeout=600, account_id=None):
        """POST one ``v1internal`` method, falling back across unhealthy hosts."""
        last = None
        for host in endpoint_hosts(path):
            try:
                return self.request(host + path, payload, headers=headers, timeout=timeout,
                                    account_id=account_id)
            except ProviderError as exc:
                last = exc
                if not retry_other_endpoint(exc, host):
                    raise
        raise last or ProviderError("Antigravity 上游不可用", 503)

    def _post(self, path, payload, headers=None, timeout=60, account_id=None):
        """Like :meth:`_open` but reads and decodes the JSON body."""
        response = self._open(path, payload, headers=headers, timeout=timeout, account_id=account_id)
        with response:
            try:
                return json.load(response)
            except (OSError, ValueError) as exc:
                raise ProviderError("Antigravity 返回了无法解析的响应", 502) from exc


# ------------------------------------------------------------------ envelope ids
def request_id():
    """``agent/<uuid>/<epoch_ms>/<uuid>/<step>``: the official-looking form."""
    return "agent/%s/%d/%s/1" % (uuid.uuid4(), int(time.time() * 1000), uuid.uuid4())


def session_id(session):
    """Negative decimal session id; stable per gateway session, else random."""
    if session:
        digest = hashlib.sha256(str(session).encode("utf-8")).digest()
        value = int.from_bytes(digest[:8], "big") % 9000000000000000000
    else:
        value = secrets.randbelow(9000000000000000000)
    return "-%d" % value


# --------------------------------------------------------------------- parsing
def project_of(info):
    """Extract a project id from a ``loadCodeAssist``/``onboardUser`` payload.

    The field is a string in one place and ``{"id": ...}`` in another, and the
    three libraries disagree on the key name, so all of them are accepted.
    """
    if not isinstance(info, dict):
        return ""
    for key in ("cloudaicompanionProject", "projectId", "project"):
        value = info.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
        if isinstance(value, dict) and value.get("id"):
            return str(value["id"])
    return ""


def tier_of(info):
    """Pick the tier id to onboard with, preferring the default tier."""
    if not isinstance(info, dict):
        return ""
    current = info.get("currentTier")
    if isinstance(current, dict) and current.get("id"):
        return str(current["id"])
    tiers = [tier for tier in info.get("allowedTiers") or [] if isinstance(tier, dict)]
    for tier in tiers:
        if tier.get("isDefault") and tier.get("id"):
            return str(tier["id"])
    for preferred in ("legacy-tier", "free-tier"):
        for tier in tiers:
            if tier.get("id") == preferred:
                return preferred
    return str(tiers[0]["id"]) if tiers and tiers[0].get("id") else ""


def model_rows(data):
    """Return dashboard rows from a ``fetchAvailableModels`` response."""
    if isinstance(data, dict) and not isinstance(data.get("models"), dict):
        data = data.get("response") if isinstance(data.get("response"), dict) else data
    catalog = data.get("models") if isinstance(data, dict) else None
    if not isinstance(catalog, dict):
        return []
    rows = []
    for identifier, info in catalog.items():
        if not isinstance(identifier, str) or not identifier:
            continue
        info = info if isinstance(info, dict) else {}
        row = {"id": identifier, "object": "model"}
        name = info.get("displayName")
        if isinstance(name, str) and name:
            row["display_name"] = name
        context = info.get("maxTokens") or info.get("maxContextWindowTokens")
        if isinstance(context, int):
            row["context_window"] = context
        output = info.get("maxOutputTokens")
        if isinstance(output, int):
            row["max_output_tokens"] = output
        rows.append(row)
    return rows


def first_candidate(payload):
    """Return the first candidate of a Gemini response, if any."""
    if not isinstance(payload, dict):
        return None
    candidates = payload.get("candidates")
    if isinstance(candidates, list) and candidates and isinstance(candidates[0], dict):
        return candidates[0]
    return None


def call_arguments(value):
    """Serialize a ``functionCall.args`` object into OpenAI argument JSON."""
    if isinstance(value, str):
        return value
    return json.dumps(value or {}, ensure_ascii=False)


def error_text(error):
    """Extract a readable message from a Google error object."""
    if isinstance(error, dict):
        return str(error.get("message") or error.get("status") or error.get("code")
                   or "未知错误")
    return str(error)


def retry_other_endpoint(exc, host):
    """Research §7.3: 408/404/5xx retry elsewhere; 400 only on sandbox."""
    if exc.status in (408, 404) or exc.status >= 500:
        return True
    if exc.status == 400 and ("sandbox" in host or "location is not supported" in str(exc).lower()):
        return True
    return False


# ------------------------------------------------------------------- SSE frames
def events_of(response):
    """Yield decoded SSE events, turning transport failures into ProviderError."""
    try:
        yield from sse_events(response)
    except OSError as exc:
        raise ProviderError("Antigravity 流式响应中断", 502) from exc


def frame(body):
    """Encode one OpenAI SSE frame."""
    return b"data: " + json.dumps(body, ensure_ascii=False).encode("utf-8") + b"\n\n"


def delta_frame(identifier, created, model, delta, finish_reason=None):
    """One ``chat.completion.chunk`` carrying a delta and/or finish reason."""
    return frame({"id": identifier, "object": "chat.completion.chunk", "created": created,
                  "model": model,
                  "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})


def usage_frame(identifier, created, model, usage):
    """The terminal usage chunk (empty ``choices``, as OpenAI sends it)."""
    return frame({"id": identifier, "object": "chat.completion.chunk", "created": created,
                  "model": model, "choices": [], "usage": usage})
