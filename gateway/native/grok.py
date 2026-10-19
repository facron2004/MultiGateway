"""xAI Grok OAuth provider (Grok Build / ``grok`` CLI grant).

Flow
----
Login is a standard OAuth 2.0 authorization-code exchange with PKCE S256 against
``https://auth.x.ai`` using the public Grok CLI client id (no client secret). The
browser is sent to the consent page and redirected to the fixed loopback URI
``http://127.0.0.1:56121/callback``, which the gateway listens on only while a
login is pending. ``offline_access`` yields a rotating refresh token.

Why the Responses API
---------------------
An OAuth bearer is only accepted on ``POST {base}/responses``: xAI marks
``/v1/chat/completions`` deprecated and the Grok CLI proxy has no Chat Completions
surface at all (docs/research/grok.md §3.1, §6). This provider therefore always
sends a Responses request and translates the streaming events back into OpenAI
chat chunks (see :mod:`gateway.native.grok_responses`).

Two transports, routed per account
----------------------------------
xAI serves the same bearer from two hosts. The resolution order, mirroring
CLIProxyAPI's tier router (research §3.2), is:

1. an explicit ``base_url`` on the credential, or ``base_url`` in the provider's
   ``config.json`` (operator override);
2. an API-key credential (``kind: "api_key"``, e.g. for the documented
   entitlement workaround) → ``api.x.ai``;
3. a ``grok-composer-*`` model → the CLI proxy (only that host serves them);
4. OAuth with JWT ``tier`` > 0 → ``api.x.ai``;
5. anything else, including missing/unknown ``tier`` → the CLI proxy (fail closed).

On ``402`` (or an entitlement-flavoured ``403``) from ``api.x.ai`` the request is
retried once through the CLI proxy for ``grok-4.5``, the model the free OAuth
entitlement exposes. Other models surface a typed "subscription does not include
API access" error instead of looping.

The CLI proxy requires the Grok Build client header set; it enforces a minimum
client version with HTTP 426, so :data:`CLIENT_VERSION` is the single value to
bump when upstream drifts (observed: 0.2.93 → 0.2.101 → 1.0.13+).

An existing ``~/.grok/auth.json`` grant can be imported through the panel's
import action instead of running the browser flow.
"""
import base64
import contextvars
import json
import platform
import time
import uuid
from datetime import datetime

from ..transport import ProviderError, json_request, open_request, sse_events
from .base import NativeProvider
from .grok_responses import chat_to_responses, responses_frames
from .oauth import add_query, random_state
from .quota import quota_number, quota_summary

ISSUER = "https://auth.x.ai"
AUTHORIZE_URL = ISSUER + "/oauth2/authorize"
TOKEN_URL = ISSUER + "/oauth2/token"
#: Public desktop/CLI client id of the official Grok CLI; not a client secret.
CLIENT_ID = "b1a00492-073a-47ea-816f-4c329264a828"
SCOPES = "openid profile email offline_access grok-cli:access api:access"
#: Non-standard xAI query parameters the official client sends (plan/referrer are
#: telemetry hints and are not validated upstream).
PLAN = "generic"
REFERRER = "multigateway"

REDIRECT_HOST = "127.0.0.1"
REDIRECT_PORT = 56121
REDIRECT_PATH = "/callback"

#: Public inference API (entitled OAuth tokens and API keys).
API_BASE = "https://api.x.ai/v1"
#: Grok Build / ``grok`` CLI internal proxy (OAuth only, HTTP POST only).
CLI_BASE = "https://cli-chat-proxy.grok.com/v1"
#: Models the CLI proxy serves; ``grok-composer-*`` is CLI-proxy-only.
COMPOSER_PREFIX = "grok-composer-"
#: Model exposed by the free OAuth entitlement; the only one worth retrying when
#: ``api.x.ai`` answers with an entitlement wall.
FREE_OAUTH_MODEL = "grok-4.5"

#: Grok Build client version sent on the CLI proxy. The server rejects stale
#: versions with HTTP 426 (it required 1.0.13+ as of 2026-10-01); bump this value
#: when that happens instead of treating 426 as fatal.
CLIENT_VERSION = "1.0.13"
CLIENT_IDENTIFIER = "grok-shell"
CLIENT_MODE = "headless"
TOKEN_AUTH = "xai-grok-cli"
AUTHENTICATE_RESPONSE = "authenticate-response"
SESSION_HEADER = "x-grok-conv-id"

#: Refresh this many seconds before the access token expires (research §2.3).
REFRESH_SKEW = 300
#: Token lifetime used when neither ``expires_in`` nor the JWT ``exp`` is usable.
DEFAULT_EXPIRES_IN = 21600
#: API keys do not expire; a far-future stamp keeps the account shown as ready.
API_KEY_LIFETIME = 10 * 365 * 86400
#: ``invalid_grant``/``invalid_client`` mean the grant is dead: re-login required.
TERMINAL_GRANT_ERRORS = ("invalid_grant", "invalid_client", "unauthorized_client")

#: Shown when ``GET /models`` is unavailable; kept in sync with docs/research/grok.md
#: §4.1. ``grok-code-fast-1`` is deliberately absent: it was retired on 2026-05-15
#: and now silently redirects to ``grok-build-0.1`` at its billing rates.
FALLBACK_MODELS = ("grok-4.7", "grok-4.6", "grok-4.5", "grok-4.3", "grok-build-0.1")

ENTITLEMENT_MARKERS = ("billing", "payment", "subscription", "entitlement",
                       "insufficient credit", "credit balance", "credits required",
                       "api tier", "tier_required", "tier-gated", "paid plan",
                       "plan access", "upgrade your plan", "supergrok")
NOT_ENTITLEMENT_MARKERS = ("blocked", "policy", "safety", "abuse", "mtls", "certificate")

#: Per-request transport decisions. Context variables (not instance state) keep
#: two concurrent requests on different accounts from clobbering each other.
_FORCED_BASE = contextvars.ContextVar("grok_forced_base", default="")
_LAST_ROUTE = contextvars.ContextVar("grok_last_route", default=None)

DONE_FRAME = b"data: [DONE]\n\n"


def redirect_uri():
    return "http://%s:%s%s" % (REDIRECT_HOST, REDIRECT_PORT, REDIRECT_PATH)


def frame_bytes(payload):
    return b"data: " + json.dumps(payload, ensure_ascii=False).encode("utf-8") + b"\n\n"


def _platform_tags():
    system = (platform.system() or "").lower()
    system = {"darwin": "macos", "win32": "windows"}.get(system, system or "windows")
    machine = (platform.machine() or "").lower()
    machine = {"amd64": "x86_64", "x86_64": "x86_64", "arm64": "aarch64"}.get(machine,
                                                                              machine or "x86_64")
    return system, machine


def user_agent(version=None):
    system, machine = _platform_tags()
    return "grok-shell/%s (%s; %s)" % (version or CLIENT_VERSION, system, machine)


def tier_allows_api(tier):
    """True when the JWT ``tier`` claim entitles the account to ``api.x.ai``.

    Missing or unparseable values fail closed to the CLI proxy; ``"free"`` is the
    free entitlement and also uses the CLI proxy (research §3.2).
    """
    if isinstance(tier, bool):
        return False
    if isinstance(tier, (int, float)):
        return tier > 0
    if isinstance(tier, str):
        text = tier.strip().lower()
        if text.isdigit():
            return int(text) > 0
        return False
    return False


def entitlement_phrase(text):
    low = (text or "").lower()
    if any(marker in low for marker in NOT_ENTITLEMENT_MARKERS):
        return False
    return any(marker in low for marker in ENTITLEMENT_MARKERS)


class Grok(NativeProvider):
    name = "grok"
    label = "Grok"
    region = "intl"
    login_kind = "loopback"
    capabilities = {"login": True, "import": True, "credits": True, "regions": False,
                    "checkin": False, "credentials": True}

    # --------------------------------------------------------------------- login
    def start_login(self, region=None):
        return self.loopback_start(REDIRECT_PORT, redirect_uri(), self._authorize_url,
                                   self._exchange_code)

    def poll_login(self, login_id):
        # The loopback listener records a denied consent as ``callback.error`` and
        # never produces a result; surface it now instead of waiting for expiry.
        with self.lock:
            login = self.pending.get(login_id)
            if login and login["callback"].error:
                callback = login["callback"]
                callback.stop()
                self.pending.pop(login_id, None)
                detail = str(callback.error or "").strip()
                raise ProviderError("Grok 授权未完成" + ("：" + detail if detail else ""),
                                    400, "access_denied")
        return self._loopback_poll(login_id, self._exchange_code)

    def _authorize_url(self, state, challenge):
        return add_query(AUTHORIZE_URL, {
            "response_type": "code",
            "client_id": CLIENT_ID,
            "redirect_uri": redirect_uri(),
            "scope": SCOPES,
            "state": state,
            "nonce": random_state(),
            "code_challenge": challenge,
            "code_challenge_method": "S256",
            "plan": PLAN,
            "referrer": REFERRER,
        })

    def _exchange_code(self, query, login):
        if login.get("state") and query.get("state") != login["state"]:
            raise ProviderError("授权回调校验失败（state 不匹配），请重新发起授权", 400,
                                "invalid_state")
        code = str(query.get("code") or "").strip()
        if not code:
            raise ProviderError("授权回调缺少 code 参数，请重新发起授权", 400, "invalid_request")
        grant = self._token_request({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "redirect_uri": login.get("redirect_uri") or redirect_uri(),
            "code_verifier": login.get("verifier") or "",
        })
        return self._credentials_from_grant(grant)

    # ------------------------------------------------------------------- refresh
    def fetch_quota(self, account_id):
        row = self._access(account_id)
        if row.get("kind") == "api_key" or row.get("using_api"):
            raise ProviderError("Grok API Key 不提供订阅额度，请使用 Grok OAuth 账号查询", 400)
        headers = {"Authorization": "Bearer " + row["access_token"], "Accept": "application/json"}
        headers.update(self.extra_headers(row["access_token"], row, CLI_BASE))
        data = json_request((self._base_override(row) or CLI_BASE) + "/billing?format=credits",
                            headers=headers, timeout=20)
        config = data.get("config") if isinstance(data, dict) else None
        if not isinstance(config, dict):
            raise ProviderError("Grok 额度响应格式无效", 502)
        period = config.get("currentPeriod") or {}
        if not isinstance(period, dict):
            raise ProviderError("Grok 额度周期格式无效", 502)
        period_type = period.get("type")
        usage = quota_number(config.get("creditUsagePercent"))
        if "creditUsagePercent" not in config and period_type in (
                "USAGE_PERIOD_TYPE_WEEKLY", "USAGE_PERIOD_TYPE_MONTHLY"):
            usage = 0  # Protobuf JSON omits the scalar when usage is zero.
        if usage is None:
            limit = config.get("monthlyLimit")
            used = config.get("used") or {}
            total = quota_number(limit.get("val", 0)) if isinstance(limit, dict) else None
            spent = quota_number(used.get("val", 0)) if isinstance(used, dict) else None
            if total is not None and total > 0 and spent is not None:
                usage = spent / total * 100
        if usage is None:
            raise ProviderError("Grok 上游未返回订阅额度数据", 502)
        label = "本周额度" if period_type == "USAGE_PERIOD_TYPE_WEEKLY" else "本月额度"
        return quota_summary([{"label": label, "remaining": max(0, min(100, 100 - usage)),
                               "unit": "%", "reset_at": period.get("end")
                               or config.get("billingPeriodEnd")}])

    def refresh_grant(self, row):
        refresh = str(row.get("refresh_token") or "")
        if not refresh:
            raise ProviderError("Grok 账号缺少 refresh_token，请重新授权", 401, "reauth_required")
        grant = self._token_request({"grant_type": "refresh_token", "client_id": CLIENT_ID,
                                     "refresh_token": refresh})
        # Rotation: a response without a new refresh_token keeps the previous one
        # (research §2.3), and the account id never changes.
        return self._credentials_from_grant(grant, previous=row)

    def _token_request(self, payload):
        try:
            return json_request(TOKEN_URL, payload, headers=self._oauth_headers(), form=True,
                                timeout=30)
        except ProviderError as exc:
            if self._terminal(exc):
                raise ProviderError("Grok 授权已失效，请重新授权（%s）" % exc.code, 401,
                                    exc.code) from exc
            raise ProviderError("Grok 授权请求失败（HTTP %s）：%s" % (exc.status, exc),
                                exc.status, exc.code) from exc

    @staticmethod
    def _oauth_headers():
        return {"User-Agent": user_agent(), "x-grok-client-version": CLIENT_VERSION,
                "Accept": "application/json"}

    @staticmethod
    def _terminal(exc):
        if str(exc.code or "").lower() in TERMINAL_GRANT_ERRORS:
            return True
        text = str(exc).lower()
        return any(marker in text for marker in TERMINAL_GRANT_ERRORS)

    # --------------------------------------------------------------- credentials
    def _access(self, account_id=None, exclude=()):
        """Pick a credential and refresh it ~300 s before expiry.

        Mirrors :meth:`NativeProvider._access` with the provider's wider refresh
        skew (the base class refreshes 120 s ahead, too late for a 6 h token that
        the CLI proxy may reject early).
        """
        row = self.pool.choose(account_id, exclude)
        expiring = float(row.get("expires_at") or 0) <= time.time() + REFRESH_SKEW
        if row.get("refresh_token") and expiring:
            row = self._refresh(row)
        if not row.get("access_token"):
            raise ProviderError("%s 账号需要重新授权" % self.label, 401)
        return row

    def _credentials_from_grant(self, grant, previous=None):
        if not isinstance(grant, dict):
            raise ProviderError("Grok 授权响应格式无效", 502)
        access = grant.get("access_token")
        if not isinstance(access, str) or not access:
            raise ProviderError("Grok 授权响应缺少 access_token", 502)
        previous = previous or {}
        id_claims = self._claims(grant.get("id_token"))
        token_claims = self._claims(access)
        email = id_claims.get("email") or token_claims.get("email") or previous.get("email") or ""
        sub = id_claims.get("sub") or token_claims.get("sub") or previous.get("sub") or ""
        name = id_claims.get("name") or token_claims.get("name") or previous.get("name") or ""
        principal_type = (token_claims.get("principal_type") or previous.get("principal_type")
                          or "")
        principal_id = (token_claims.get("principal_id") or token_claims.get("team_id")
                        or previous.get("principal_id") or "")
        credentials = {
            "access_token": access,
            "expires_at": self._expiry(access, grant.get("expires_in")),
            "id": previous.get("id") or email or sub or uuid.uuid4().hex,
            "kind": previous.get("kind") or "oauth",
            "token_type": grant.get("token_type") or "Bearer",
            "scope": grant.get("scope") or previous.get("scope") or SCOPES,
        }
        refresh = grant.get("refresh_token") or previous.get("refresh_token")
        if refresh:
            credentials["refresh_token"] = refresh
        tier = token_claims.get("tier", previous.get("tier"))
        if tier is not None:
            credentials["tier"] = tier
        for key, value in (("email", email), ("sub", sub), ("name", name),
                           ("principal_type", principal_type), ("principal_id", principal_id)):
            if value:
                credentials[key] = value
        credentials["label"] = email or name or previous.get("label") or credentials["id"]
        if previous.get("base_url"):
            credentials["base_url"] = previous["base_url"]
        return credentials

    @staticmethod
    def _claims(token):
        """Decode an unsigned JWT payload.

        Only identity/``exp``/``tier`` hints are read from tokens the gateway itself
        obtained over TLS; the signature is never used as a trust boundary here.
        """
        if not isinstance(token, str) or token.count(".") < 2:
            return {}
        payload = token.split(".")[1]
        try:
            raw = base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4))
            value = json.loads(raw.decode("utf-8"))
        except (ValueError, TypeError):
            return {}
        return value if isinstance(value, dict) else {}

    @staticmethod
    def _expiry(access_token, expires_in):
        """Absolute expiry: JWT ``exp`` wins, then ``expires_in``, then a default.

        ``expires_in`` is not stable upstream (research §2.1), so the JWT claim is
        preferred whenever it parses.
        """
        now = time.time()
        exp = Grok._claims(access_token).get("exp")
        if isinstance(exp, (int, float)) and not isinstance(exp, bool) and exp > now:
            return float(exp)
        try:
            seconds = float(expires_in or 0)
        except (TypeError, ValueError):
            seconds = 0.0
        return now + (seconds if seconds > 0 else DEFAULT_EXPIRES_IN)

    # -------------------------------------------------------------------- import
    def import_credentials(self, data):
        rows = self._import_rows(data)
        count = 0
        for item in rows:
            self.pool.save(self.credentials_from_import(item))
            count += 1
        return {"ok": True, "count": count}

    def _import_rows(self, data):
        if isinstance(data, dict) and not self._looks_like_grant(data):
            # ``~/.grok/auth.json`` is a map keyed by ``"{issuer}::{client_id}"``.
            rows = []
            for value in data.values():
                if not isinstance(value, dict) or not self._looks_like_grant(value):
                    continue
                issuer = value.get("oidc_issuer")
                client = value.get("oidc_client_id")
                if issuer and issuer != ISSUER:
                    continue
                if client and client != CLIENT_ID:
                    continue
                rows.append(value)
            if rows:
                return rows
            raise ProviderError("导入内容中没有可用的 Grok 授权", 400)
        if isinstance(data, list):
            rows = [item for item in data if isinstance(item, dict)]
        else:
            accounts = data.get("accounts") if isinstance(data, dict) else None
            rows = [item for item in accounts if isinstance(item, dict)] \
                if isinstance(accounts, list) else [data]
        if not rows:
            raise ProviderError("导入内容中没有可用的 Grok 授权", 400)
        return rows

    @staticmethod
    def _looks_like_grant(value):
        return isinstance(value, dict) and bool(
            value.get("access_token") or value.get("key") or value.get("refresh_token"))

    def credentials_from_import(self, item):
        token = item.get("access_token") or item.get("key")
        if not isinstance(token, str) or not token.strip():
            raise ProviderError("导入内容缺少 access_token", 400)
        token = token.strip()
        # An xAI API key is the documented escape hatch for accounts whose OAuth
        # grant is entitlement-gated; it is routed to api.x.ai with no CLI headers.
        kind = item.get("kind") or ("oauth" if token.count(".") >= 2 else "api_key")
        claims = self._claims(token) if kind == "oauth" else {}
        email = claims.get("email") or item.get("email") or ""
        sub = claims.get("sub") or item.get("sub") or item.get("user_id") or ""
        credentials = {
            "access_token": token,
            "kind": kind,
            "id": email or sub or item.get("id") or uuid.uuid4().hex,
        }
        refresh = item.get("refresh_token")
        if isinstance(refresh, str) and refresh.strip():
            credentials["refresh_token"] = refresh.strip()
        expires_at = self._timestamp(item.get("expires_at"))
        if not expires_at:
            expires_at = (self._expiry(token, 0) if kind == "oauth"
                          else time.time() + API_KEY_LIFETIME)
        credentials["expires_at"] = expires_at
        for key, value in (("email", email), ("sub", sub),
                           ("name", item.get("name") or claims.get("name") or ""),
                           ("principal_type", item.get("principal_type")
                            or claims.get("principal_type") or ""),
                           ("principal_id", item.get("principal_id") or item.get("team_id")
                            or claims.get("principal_id") or "")):
            if value:
                credentials[key] = value
        tier = item.get("tier", claims.get("tier"))
        if tier is not None:
            credentials["tier"] = tier
        credentials["label"] = email or item.get("label") or credentials["id"]
        if isinstance(item.get("base_url"), str) and item["base_url"].strip():
            credentials["base_url"] = item["base_url"].strip()
        if item.get("using_api"):
            credentials["using_api"] = True
        return credentials

    @staticmethod
    def _timestamp(value):
        if isinstance(value, (int, float)) and not isinstance(value, bool) and value > 0:
            return float(value)
        if isinstance(value, str) and value.strip():
            text = value.strip().replace("Z", "+00:00")
            try:
                return datetime.fromisoformat(text).timestamp()
            except ValueError:
                return 0.0
        return 0.0

    # ----------------------------------------------------------------- transports
    def url(self, path):
        """Upstream URL for ``path`` on the default transport.

        Real requests route per credential/model in :meth:`_send`; this is the
        contract entry point and resolves the operator override or the CLI proxy
        (the fail-closed default).
        """
        return (self._base_override({}) or CLI_BASE) + path

    def _base_override(self, row):
        for value in (row.get("base_url"), self.load_config().get("base_url")):
            if isinstance(value, str) and value.strip():
                return value.strip().rstrip("/")
        return ""

    def _route(self, row, model):
        base = self._base_override(row)
        if base:
            return {"base": base, "oauth": row.get("kind") != "api_key"}
        if row.get("kind") == "api_key" or row.get("using_api"):
            return {"base": API_BASE, "oauth": False}
        if str(model or "").startswith(COMPOSER_PREFIX):
            return {"base": CLI_BASE, "oauth": True}
        if tier_allows_api(row.get("tier")):
            return {"base": API_BASE, "oauth": True}
        return {"base": CLI_BASE, "oauth": True}

    def extra_headers(self, token, row=None, base=None):
        """Grok Build headers for the CLI proxy; ``api.x.ai`` needs only auth."""
        row = row or {}
        if (base or CLI_BASE) == API_BASE:
            return {}
        headers = {
            "x-grok-client-version": CLIENT_VERSION,
            "x-grok-client-identifier": CLIENT_IDENTIFIER,
            "x-grok-client-mode": CLIENT_MODE,
            "X-XAI-Token-Auth": TOKEN_AUTH,
            "x-authenticateresponse": AUTHENTICATE_RESPONSE,
            "User-Agent": user_agent(),
        }
        principal_id = str(row.get("principal_id") or "")
        if principal_id:
            headers["x-userid"] = principal_id
            headers["x-grok-user-id"] = principal_id
        email = str(row.get("email") or "")
        if email and str(row.get("principal_type") or "").lower() not in ("team", "organization"):
            headers["x-email"] = email
        return headers

    def _send(self, path, payload, token, row, headers=None, method=None, timeout=600):
        model = payload.get("model") if isinstance(payload, dict) else ""
        route = self._route(row, model)
        base = _FORCED_BASE.get() or route["base"]
        _LAST_ROUTE.set({"base": base, "oauth": route["oauth"]})
        outgoing = {"Authorization": "Bearer " + token, "Content-Type": "application/json"}
        outgoing.update(self.extra_headers(token, row, base))
        outgoing["Accept"] = ("text/event-stream"
                              if isinstance(payload, dict) and payload.get("stream")
                              else "application/json")
        outgoing.update(headers or {})
        body = None if payload is None else json.dumps(payload, ensure_ascii=False).encode("utf-8")
        try:
            return open_request(base + path, body, outgoing, method=method, timeout=timeout)
        except ProviderError as exc:
            if exc.status == 426:
                raise ProviderError(
                    "Grok CLI 通道要求更新的客户端版本（当前 %s，HTTP 426）。请更新 MultiGateway，"
                    "或将 gateway/native/grok.py 的 CLIENT_VERSION 调整为当前 Grok CLI 版本。"
                    % CLIENT_VERSION, 426, "client_version_outdated") from exc
            raise

    # --------------------------------------------------------------------- models
    def request(self, path, payload, headers=None, account_id=None, method=None, timeout=600):
        """Send one upstream request, rotating accounts on auth/quota failures.

        Mirrors :meth:`NativeProvider.request` with one difference that matters for
        this provider: when every account fails, the *last upstream error* is
        re-raised instead of the pool's "no accounts left" error, so a 402/403
        entitlement wall can trigger the transport fallback (and reach the user as
        a typed error) rather than being masked by a 503.
        """
        excluded = []
        last = None
        for _ in range(self.max_attempts):
            try:
                row = self._access(account_id, excluded)
            except ProviderError:
                if last is None:
                    raise
                raise last
            try:
                response = self._send(path, payload, row["access_token"], row, headers,
                                      method, timeout)
            except ProviderError as exc:
                last = exc
                if account_id or exc.status not in (401, 402, 403, 429):
                    raise
                self._set_error(row["id"], str(exc)[:200])
                if exc.status == 401 and row.get("refresh_token"):
                    # A 401 is often a stale access token: refresh and retry the
                    # same credential before moving on to the next account.
                    try:
                        self._refresh(row)
                    except ProviderError:
                        excluded.append(row["id"])
                    continue
                excluded.append(row["id"])
                continue
            if row.get("error"):
                self._clear_error(row["id"])
            response.gateway_account = row["id"]
            return response
        raise last or ProviderError("%s 上游不可用" % self.label, 503)

    def fetch_models(self):
        """Live catalog, with the research fallback when the account cannot list it."""
        try:
            response = self.request("/models", None, method="GET", timeout=20)
        except ProviderError:
            return list(FALLBACK_MODELS)
        with response:
            try:
                data = json.load(response).get("data") or []
            except (ValueError, AttributeError):
                return list(FALLBACK_MODELS)
        rows = []
        for item in data:
            if isinstance(item, str) and item:
                rows.append(item)
            elif isinstance(item, dict) and item.get("id"):
                rows.append(str(item["id"]))
        return rows or list(FALLBACK_MODELS)

    # ------------------------------------------------------------------ inference
    def chat(self, payload, session=None):
        """Yield OpenAI chat SSE frames for one chat.completions request."""
        body = chat_to_responses(payload)
        return self._stream(body, session)

    def _stream(self, body, session):
        response = self._open_responses(body, session)
        try:
            for chunk in responses_frames(sse_events(response), body.get("model") or ""):
                yield frame_bytes(chunk)
        finally:
            response.close()
        yield DONE_FRAME

    def _open_responses(self, body, session):
        headers = self.session_headers(session, SESSION_HEADER)
        try:
            return self.request("/responses", body, headers=headers)
        except ProviderError as exc:
            route = _LAST_ROUTE.get() or {}
            if not self._entitlement_wall(exc, route):
                raise
            if (body.get("model") or "") != FREE_OAUTH_MODEL:
                raise self._entitlement_error(exc) from exc
        # The free entitlement is served only by the CLI proxy: retry once there.
        try:
            forced = _FORCED_BASE.set(CLI_BASE)
            try:
                return self.request("/responses", body, headers=headers)
            finally:
                _FORCED_BASE.reset(forced)
        except ProviderError as exc:
            if exc.status in (402, 403):
                raise self._entitlement_error(exc) from exc
            raise

    @staticmethod
    def _entitlement_wall(exc, route):
        if route.get("base") != API_BASE or not route.get("oauth"):
            return False
        if exc.status == 402:
            return True
        return exc.status == 403 and entitlement_phrase(str(exc))

    @staticmethod
    def _entitlement_error(exc):
        status = exc.status if exc.status in (402, 403) else 403
        return ProviderError(
            "当前 Grok 订阅不包含该模型的 API 访问权限（HTTP %s）。可改用 grok-4.5"
            "（CLI 通道），或在面板导入 xAI API Key。" % exc.status, status,
            "entitlement_required")
