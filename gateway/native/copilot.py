"""GitHub Copilot account provider (device flow + short-lived Copilot JWT).

Flow
----
1. :meth:`Copilot.start_login` asks GitHub's public device endpoint for a user
   code (client id ``Iv1.b507a08c87ecfe98``; no client secret, no PKCE) and keeps
   the device code in :attr:`pending`.
2. The dashboard shows the user code, the user approves it on github.com, and
   :meth:`Copilot.poll_login` polls ``/login/oauth/access_token`` until a GitHub
   token (``gho_…``) is issued. ``slow_down`` widens the poll interval; an
   expired or denied device code ends the session.
3. A GitHub token cannot call the Copilot API by itself: it has to be exchanged
   at ``/copilot_internal/v2/token`` for a JWT that lives ~20 minutes. The JWT is
   cached in memory per account (never written to disk) and refreshed
   single-flight once less than :data:`REFRESH_MARGIN` seconds remain, or once
   when the upstream answers 401.
4. Inference is plain OpenAI ``chat.completions`` against a seat-specific host,
   so :meth:`Copilot.chat` only adds the fingerprint headers the service expects
   and relays the SSE stream unchanged.

The account file therefore holds exactly one secret — the GitHub token in
``access_token``. ``refresh_token`` mirrors it because the shared account pool
uses that key to mark an account as renewable, and ``expires_at`` stays ``0``
(the GitHub token does not expire).

Uncertain values (research snapshot 2025-12; re-check against a live seat before
shipping): the seat base URL is taken from the JWT ``endpoints.api`` field when
present and otherwise inferred from ``sku`` (business/enterprise hosts below);
the editor/plugin versions are fingerprint values that rot with VS Code and
Copilot Chat releases; the static model fallback is a placeholder for a cold
start where ``/models`` is unreachable.
"""
import base64
import json
import threading
import time
import uuid

from ..transport import ProviderError, json_request, open_request
from .base import OpenAIProvider
from .oauth import new_login_id
from .quota import quota_number, quota_summary

GITHUB_CLIENT_ID = "Iv1.b507a08c87ecfe98"
GITHUB_SCOPE = "read:user"
DEVICE_CODE_URL = "https://github.com/login/device/code"
ACCESS_TOKEN_URL = "https://github.com/login/oauth/access_token"
DEVICE_GRANT_TYPE = "urn:ietf:params:oauth:grant-type:device_code"
COPILOT_TOKEN_URL = "https://api.github.com/copilot_internal/v2/token"
COPILOT_USER_URL = "https://api.github.com/copilot_internal/user"
GITHUB_USER_URL = "https://api.github.com/user"

#: Seat-specific inference hosts. Individual seats use :data:`API_BASE`; the JWT
#: payload usually carries the authoritative host in ``endpoints.api``.
API_BASE = "https://api.githubcopilot.com"
BUSINESS_API_BASE = "https://api.business.githubcopilot.com"
ENTERPRISE_API_BASE = "https://api.enterprise.githubcopilot.com"

#: Client fingerprint required by the Copilot service (see module docstring).
INTEGRATION_ID = "vscode-chat"
EDITOR_VERSION = "vscode/1.104.3"
EDITOR_PLUGIN_VERSION = "copilot-chat/0.26.7"
USER_AGENT = "GitHubCopilotChat/0.26.7"
GITHUB_API_VERSION = "2025-04-01"

#: Refresh the JWT when fewer than this many seconds remain (it lives ~20 min).
REFRESH_MARGIN = 300
DEFAULT_JWT_TTL = 1200

#: Keeps the model picker usable when ``/models`` cannot be reached. The real
#: catalog is account/plan dependent, so this list is only a fallback.
FALLBACK_MODELS = ("gpt-4.1", "gpt-4o", "o3-mini", "claude-sonnet-4", "gemini-2.5-pro")

PENDING_STATES = ("authorization_pending", "slow_down")
#: Terminal device-flow errors mapped to user-facing Chinese messages.
DEVICE_ERRORS = {
    "expired_token": ("GitHub 设备授权已超时，请重新发起授权", "expired_token"),
    "access_denied": ("GitHub 授权已被拒绝，请重新发起授权", "access_denied"),
    "incorrect_device_code": ("GitHub 设备授权已失效，请重新发起授权", "expired_token"),
    "device_flow_disabled": ("该 GitHub 应用未启用设备授权", "device_flow_disabled"),
    "unsupported_grant_type": ("GitHub 设备授权请求无效，请重新发起授权", "access_denied"),
    "incorrect_client_credentials": ("GitHub 设备授权客户端配置无效", "access_denied"),
}


def _as_float(value, default=0.0):
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _jwt_payload(token):
    """Decode a JWT payload without verifying it; ``{}`` when it is not a JWT."""
    parts = str(token or "").split(".")
    if len(parts) < 2:
        return {}
    body = parts[1] + "=" * (-len(parts[1]) % 4)
    try:
        payload = json.loads(base64.urlsafe_b64decode(body))
    except (ValueError, TypeError):
        return {}
    return payload if isinstance(payload, dict) else {}


class Copilot(OpenAIProvider):
    """GitHub Copilot behind one GitHub account per pool entry."""

    name = "copilot"
    label = "GitHub Copilot"
    region = "intl"
    login_kind = "device"
    capabilities = {"login": True, "import": True, "credits": True,
                    "regions": False, "checkin": False, "credentials": True}

    # Copilot's OpenAI surface is not versioned under /v1.
    chat_path = "/chat/completions"
    models_path = "/models"

    def __init__(self, directory):
        super().__init__(directory)
        #: Copilot JWT cache, one entry per account id (never persisted).
        self._tokens = {}
        #: Per-account locks so concurrent requests refresh the JWT only once.
        self._token_locks = {}

    def stop(self):
        super().stop()
        with self.lock:
            self._tokens.clear()
            self._token_locks.clear()

    def url(self, path):
        """Default upstream URL (individual seat); business/enterprise use a host
        derived from the JWT in :meth:`_api_base`."""
        return API_BASE + path

    def fetch_quota(self, account_id):
        _, row = self.pool.find(account_id)
        data = json_request(COPILOT_USER_URL, headers=self._github_headers(row["access_token"]),
                            timeout=20)
        if not isinstance(data, dict):
            raise ProviderError("GitHub Copilot 额度响应格式无效", 502)
        snapshots = data.get("quota_snapshots") or {}
        legacy = data.get("limited_user_quotas") or {}
        totals = data.get("monthly_quotas") or {}
        if not all(isinstance(value, dict) for value in (snapshots, legacy, totals)):
            raise ProviderError("GitHub Copilot 额度响应格式无效", 502)
        reset = data.get("quota_reset_date_utc") or data.get("quota_reset_date")
        buckets = []
        for key, label in (("premium_interactions", "高级请求"), ("chat", "对话"),
                           ("completions", "代码补全")):
            info = snapshots.get(key) or (snapshots.get("premium_models")
                                          if key == "premium_interactions" else None)
            if not isinstance(info, dict):
                info = {"remaining": legacy[key], "entitlement": totals.get(key)} if key in legacy else {}
            limit = quota_number(info.get("entitlement"))
            if info.get("has_quota") is False and limit == 0 and not info.get("unlimited"):
                continue  # Free seats report a premium bucket they are not entitled to.
            unit = "额度" if info.get("token_based_billing") and key != "completions" else "次"
            remaining = quota_number(info.get("quota_remaining"))
            if remaining is None:
                remaining = quota_number(info.get("remaining"))
            percent = quota_number(info.get("percent_remaining"))
            if info.get("unlimited"):
                remaining = "unlimited"
            elif remaining is None and percent is not None:
                remaining = limit * percent / 100 if limit is not None else percent
                if limit is None:
                    unit = "%"
            if remaining is None:
                continue
            if remaining != "unlimited":
                remaining = max(0, remaining)
            buckets.append({"label": label, "remaining": remaining, "limit": limit, "unit": unit,
                            "reset_at": info.get("quota_reset_at") or reset})
        return quota_summary(buckets)

    # ----------------------------------------------------------------------- login
    def start_login(self, region=None):
        """Ask GitHub for a device code (no PKCE, no client secret)."""
        payload = {"client_id": GITHUB_CLIENT_ID, "scope": GITHUB_SCOPE}
        try:
            grant = json_request(DEVICE_CODE_URL, payload, form=True,
                                 headers={"Accept": "application/json",
                                          "User-Agent": USER_AGENT}, timeout=30)
        except ProviderError as exc:
            raise ProviderError("无法连接 GitHub 设备授权服务，请稍后再试", 502) from exc
        if not isinstance(grant, dict):
            raise ProviderError("GitHub 设备授权响应无效，请稍后再试", 502)
        device_code = str(grant.get("device_code") or "")
        user_code = str(grant.get("user_code") or "")
        url = str(grant.get("verification_uri_complete") or grant.get("verification_uri")
                  or "https://github.com/login/device")
        expires_in = _as_float(grant.get("expires_in"))
        if not device_code or not user_code or expires_in <= 0:
            raise ProviderError("GitHub 设备授权响应不完整，请稍后再试", 502)
        interval = max(1.0, _as_float(grant.get("interval"), 5.0))
        now = time.time()
        login_id = new_login_id()
        with self.lock:
            self._prune_pending(now)
            self.pending[login_id] = {"device_code": device_code, "interval": interval,
                                      "expires_at": now + expires_in,
                                      "next_poll": now + interval, "created_at": now}
        return {"login_id": login_id, "user_code": user_code, "url": url,
                "interval": interval, "expires_at": now + expires_in}

    def poll_login(self, login_id):
        """Poll GitHub once; the vendor interval is honoured between calls."""
        with self.lock:
            login = self._pending_of(login_id)
            now = time.time()
            if now < login["next_poll"]:
                return {"status": "pending", "interval": login["interval"]}
            # Reserve the slot before the network call so concurrent polls coalesce.
            login["next_poll"] = now + login["interval"]
            device_code = login["device_code"]
            interval = login["interval"]

        grant = self._device_token(device_code)
        state = str(grant.get("error") or "")
        if state == "slow_down":
            with self.lock:
                login = self.pending.get(login_id)
                if login is not None:
                    login["interval"] += 5
                    login["next_poll"] = time.time() + login["interval"]
                    interval = login["interval"]
        if state in PENDING_STATES or state == "transient":
            # Transient failures keep the session alive: the dashboard polls again.
            return {"status": "pending", "interval": interval}
        if state:
            self._forget_pending(login_id)
            message, code = DEVICE_ERRORS.get(
                state, ("GitHub 设备授权失败，请重新发起授权", "access_denied"))
            raise ProviderError(message, 400, code)
        return self._save_login(login_id, grant)

    def _pending_of(self, login_id):
        """Return one live pending login; caller holds :attr:`lock`."""
        login = self.pending.get(login_id)
        if login is None:
            raise ProviderError("授权会话已失效，请重新发起授权", 400, "expired_token")
        if login["expires_at"] <= time.time():
            self.pending.pop(login_id, None)
            raise ProviderError("GitHub 设备授权已超时，请重新发起授权", 400, "expired_token")
        return login

    def _forget_pending(self, login_id):
        with self.lock:
            self.pending.pop(login_id, None)

    def _device_token(self, device_code):
        """One device-token poll; returns the grant or ``{"error": state}``."""
        payload = {"client_id": GITHUB_CLIENT_ID, "device_code": device_code,
                   "grant_type": DEVICE_GRANT_TYPE}
        try:
            grant = json_request(ACCESS_TOKEN_URL, payload, form=True,
                                 headers={"Accept": "application/json",
                                          "User-Agent": USER_AGENT}, timeout=30)
        except ProviderError as exc:
            if exc.code in PENDING_STATES:
                return {"error": exc.code}
            if exc.status >= 500 or exc.status == 429:
                return {"error": "transient"}
            if exc.code in DEVICE_ERRORS:
                return {"error": exc.code}
            raise ProviderError("GitHub 设备授权失败，请重新发起授权", 502) from exc
        if not isinstance(grant, dict):
            raise ProviderError("GitHub 设备授权响应无效，请稍后再试", 502)
        return grant

    def _save_login(self, login_id, grant):
        """Store the GitHub token as an account, then pre-warm its Copilot JWT."""
        token = str(grant.get("access_token") or "")
        if not token:
            self._forget_pending(login_id)
            raise ProviderError("GitHub 授权未返回 access_token，请重新发起授权", 502)
        profile = self._identity(token)
        login = str(profile.get("login") or "")
        credentials = {
            "id": login or uuid.uuid4().hex,
            "access_token": token,
            # The pool treats a row with ``refresh_token`` as renewable; the
            # GitHub token is the only durable secret, so it fills that slot.
            "refresh_token": token,
            # The GitHub token does not expire; the Copilot JWT is memory only.
            "expires_at": 0,
            "label": profile.get("name") or login or "GitHub Copilot",
            "email": profile.get("email") or "",
            # A fresh grant clears any error a previous token left behind.
            "error": "",
        }
        account_id = self.pool.save(credentials)
        self._forget_pending(login_id)
        self._prewarm(account_id, token)
        return {"status": "authenticated", "account_id": account_id}

    def _identity(self, github_token):
        """Best-effort GitHub profile used as the account label; never required."""
        try:
            profile = json_request(GITHUB_USER_URL, None, headers={
                "Authorization": "token " + github_token,
                "Accept": "application/vnd.github+json",
                "User-Agent": USER_AGENT,
                "X-GitHub-Api-Version": GITHUB_API_VERSION,
            }, timeout=20)
        except ProviderError:
            return {}
        return profile if isinstance(profile, dict) else {}

    def _prewarm(self, account_id, github_token):
        """Exchange the JWT right after login so entitlement errors surface early."""
        try:
            entry = self._exchange(github_token)
        except ProviderError as exc:
            if exc.status in (401, 403, 404):
                self._set_error(account_id, str(exc)[:200])
            return
        with self.lock:
            self._tokens[account_id] = entry

    # --------------------------------------------------------------- credentials
    def refresh_grant(self, row):
        """Re-validate the GitHub token by exchanging it for a fresh Copilot JWT.

        GitHub device-flow tokens have no refresh token, so "refresh" only proves
        the stored token still works and repopulates the JWT cache; the account
        file keeps exactly the GitHub token it already had.
        """
        token = row.get("access_token") or ""
        entry = self._exchange(token)
        with self.lock:
            self._tokens[row["id"]] = entry
        return {"access_token": token, "refresh_token": row.get("refresh_token") or token,
                "expires_at": 0}

    def credentials_from_import(self, item):
        """Accept a bare GitHub token row (``{"access_token": "gho_…"}``)."""
        credentials = dict(item)
        token = str(credentials.get("access_token") or "")
        credentials.setdefault("id", credentials.get("login") or uuid.uuid4().hex)
        credentials["refresh_token"] = credentials.get("refresh_token") or token
        credentials["expires_at"] = 0
        credentials["error"] = ""
        return credentials

    # -------------------------------------------------------------------- headers
    def extra_headers(self, token, row=None):
        """Copilot fingerprint headers sent with every upstream call."""
        return {
            "Accept": "application/json",
            "Copilot-Integration-Id": INTEGRATION_ID,
            "Editor-Version": EDITOR_VERSION,
            "Editor-Plugin-Version": EDITOR_PLUGIN_VERSION,
            "User-Agent": USER_AGENT,
            "X-GitHub-Api-Version": GITHUB_API_VERSION,
            "Openai-Intent": "conversation-panel",
            "X-Request-Id": uuid.uuid4().hex,
            "X-Vscode-User-Agent-Library-Version": "electron-fetch",
        }

    @staticmethod
    def _github_headers(token):
        return {"Authorization": "token " + token, "Accept": "application/json",
                "Editor-Version": EDITOR_VERSION,
                "Editor-Plugin-Version": EDITOR_PLUGIN_VERSION,
                "User-Agent": USER_AGENT, "X-GitHub-Api-Version": GITHUB_API_VERSION}

    def _api_base(self, token):
        """Seat-specific API host: JWT ``endpoints.api`` first, then ``sku``."""
        payload = _jwt_payload(token)
        endpoints = payload.get("endpoints")
        if isinstance(endpoints, dict):
            api = str(endpoints.get("api") or "").strip().rstrip("/")
            if api.startswith(("https://", "http://")):
                return api
        sku = str(payload.get("sku") or "").lower()
        if "enterprise" in sku:
            return ENTERPRISE_API_BASE
        if "business" in sku:
            return BUSINESS_API_BASE
        return API_BASE

    # -------------------------------------------------------------------- request
    def _send(self, path, payload, token, row, headers=None, method=None, timeout=600):
        """Send one request to the seat host resolved from ``token``.

        The JWT is decoded per attempt to pick the host; that is a couple of
        microseconds and keeps the routing stateless across concurrent requests.
        """
        outgoing = {"Authorization": "Bearer " + token, "Content-Type": "application/json"}
        outgoing.update(self.extra_headers(token, row))
        outgoing.update(headers or {})
        body = None if payload is None else json.dumps(payload, ensure_ascii=False).encode("utf-8")
        return open_request(self._api_base(token) + path, body, outgoing,
                            method=method, timeout=timeout)

    def request(self, path, payload, headers=None, account_id=None, method=None, timeout=600):
        """Send one Copilot request, refreshing the JWT on 401 and rotating accounts."""
        excluded = []
        last = None
        for _ in range(self.max_attempts):
            try:
                row = self.pool.choose(account_id, excluded)
            except ProviderError:
                # Every account has been tried: report the real upstream cause
                # instead of the generic "pool is empty" message.
                if last is not None:
                    raise last from None
                raise
            try:
                return self._attempt(path, payload, row, headers, method, timeout)
            except ProviderError as exc:
                last = exc
                if account_id or exc.status not in (401, 403, 429):
                    raise
                self._set_error(row["id"], str(exc)[:200])
                excluded.append(row["id"])
        raise last or ProviderError("%s 上游暂不可用，请稍后再试" % self.label, 503)

    def _attempt(self, path, payload, row, headers, method, timeout):
        entry = self._jwt(row)
        try:
            response = self._send(path, payload, entry["token"], row, headers, method, timeout)
        except ProviderError as exc:
            if exc.status != 401:
                raise
            # A 401 can mean an early-revoked JWT: force one refresh, then let the
            # caller rotate to the next account if the retry fails too.
            entry = self._jwt(row, stale=entry["token"])
            response = self._send(path, payload, entry["token"], row, headers, method, timeout)
        response.gateway_account = row["id"]
        return response

    def _jwt(self, row, stale=""):
        """Return ``{"token", "expires_at"}`` for one account, single-flight.

        ``stale`` is a token the upstream just rejected; it is only replaced when
        it is still the cached one, so ten concurrent 401s cause one refresh.
        """
        account_id = row["id"]
        with self.lock:
            entry = self._tokens.get(account_id)
            if self._usable(entry, stale):
                return entry
        with self._account_lock(account_id):
            with self.lock:
                entry = self._tokens.get(account_id)
                if self._usable(entry, stale):
                    return entry
                self._tokens.pop(account_id, None)
            entry = self._exchange(row.get("access_token"))
            with self.lock:
                self._tokens[account_id] = entry
            return entry

    @staticmethod
    def _usable(entry, stale):
        return bool(entry and entry["token"] != stale
                    and entry["expires_at"] - time.time() > REFRESH_MARGIN)

    def _account_lock(self, account_id):
        with self.lock:
            return self._token_locks.setdefault(account_id, threading.Lock())

    def _exchange(self, github_token):
        """Trade the durable GitHub token for the ~20 minute Copilot JWT."""
        github_token = str(github_token or "")
        if not github_token:
            raise ProviderError("账号缺少 GitHub 授权，请重新授权 GitHub Copilot", 401,
                                "unauthorized")
        try:
            grant = json_request(COPILOT_TOKEN_URL, None,
                                 headers=self._github_headers(github_token), timeout=30)
        except ProviderError as exc:
            raise self._exchange_error(exc) from exc
        token = str(grant.get("token") or "") if isinstance(grant, dict) else ""
        if not token:
            raise ProviderError("GitHub Copilot 令牌交换失败，请稍后再试", 502)
        now = time.time()
        expires_at = _as_float(grant.get("expires_at"))
        if expires_at <= 0:
            expires_at = now + _as_float(grant.get("refresh_in"), DEFAULT_JWT_TTL)
        # A token that is already expired is still worth one attempt; never let it
        # look stale on arrival or every request would re-exchange.
        return {"token": token, "expires_at": max(expires_at, now + 60)}

    @staticmethod
    def _exchange_error(exc):
        """Translate a token-exchange failure without echoing upstream details."""
        if exc.status == 401:
            return ProviderError("GitHub 授权已失效，请重新授权 GitHub Copilot", 401,
                                 "unauthorized")
        if exc.status in (403, 404):
            return ProviderError("该 GitHub 账号未开通 GitHub Copilot，请先订阅后再授权", 403,
                                 "no_seat")
        return ProviderError("GitHub Copilot 令牌交换失败，请稍后再试", 502)

    # ------------------------------------------------------------------- protocol
    def chat(self, payload, session=None):
        """Relay one OpenAI chat request; ``session`` is unused (no affinity)."""
        body = dict(payload)
        body["stream"] = True
        options = dict(body.get("stream_options") or {})
        options["include_usage"] = True
        body["stream_options"] = options
        headers = {"Accept": "text/event-stream",
                   "X-Initiator": self._initiator(body.get("messages"))}
        return self.relay(self.request(self.chat_path, body, headers=headers))

    @staticmethod
    def _initiator(messages):
        """Copilot wants ``agent`` for turns continuing an assistant/tool message."""
        for message in messages or []:
            if isinstance(message, dict) and message.get("role") in ("assistant", "tool"):
                return "agent"
        return "user"

    def fetch_models(self):
        """Chat model ids from ``/models``; the static list covers a cold start."""
        try:
            response = self.request(self.models_path, None, method="GET", timeout=20)
            with response:
                data = json.load(response)
        except (ProviderError, ValueError):
            return list(FALLBACK_MODELS)
        items = data.get("data") if isinstance(data, dict) else data
        rows = []
        for item in items or []:
            if isinstance(item, str):
                rows.append(item)
                continue
            if not isinstance(item, dict) or not item.get("id"):
                continue
            if item.get("model_picker_enabled") is False:
                continue
            capabilities = item.get("capabilities")
            if isinstance(capabilities, dict) and capabilities.get("type") not in (None, "chat"):
                continue
            rows.append(str(item["id"]))
        return rows or list(FALLBACK_MODELS)
