"""Lifecycle shared by the native OAuth providers (Antigravity, Grok, …).

A native provider lives inside the gateway process: it owns its OAuth grants, a
multi-account pool, a model catalog and a streaming translator, and exposes the
same management contract the process-backed providers (Trae, Qoder) expose via
HTTP. The gateway handler only needs one entry point:

``chat(payload, session)`` -> iterator of OpenAI chat SSE frames

from which chat.completions, Responses and Anthropic Messages are all served
(:meth:`messages` is generated from ``chat`` unless a provider overrides it).
"""
import json
import threading
import time
import uuid
from pathlib import Path

from ..files import atomic_json
from ..transport import ProviderError, open_request, sse_events
from .oauth import LoopbackCallback, new_login_id, pkce_pair, random_state
from .pool import AccountPool
from .translate import anthropic_to_chat, chat_frames_to_anthropic


class NativeProvider:
    name = ""
    label = ""
    region = "intl"
    #: Upstream dialect. ``openai`` providers only implement :meth:`chat`.
    kind = "openai"
    login_kind = "loopback"
    capabilities = {"login": True, "import": True, "credits": False, "regions": False,
                    "checkin": False, "credentials": True}
    model_ttl = 300
    max_attempts = 3

    def __init__(self, directory):
        self.directory = Path(directory)
        self.directory.mkdir(parents=True, exist_ok=True)
        self.pool = AccountPool(self)
        self.catalog = None
        self.lock = threading.RLock()
        self.pending = {}
        self.error = ""
        #: Account that served the request on this thread; the handler lifts it
        #: into the activity row so the usage board can name the account.
        self._local = threading.local()

    # ------------------------------------------------------------------ lifecycle
    def start(self):
        """Nothing to start; the provider is ready as soon as credentials exist."""

    def stop(self):
        with self.lock:
            for login in self.pending.values():
                callback = login.get("callback")
                if callback is not None:
                    callback.stop()
            self.pending.clear()

    def status(self):
        return {"running": True, "accounts": len(self.pool.records()), "region": self.region}

    # ----------------------------------------------------------- management contract
    def account_views(self):
        now = time.time()
        rows = []
        for _, row in self.pool.records():
            enabled = row.get("enabled", True)
            usable = bool(row.get("refresh_token")) or float(row.get("expires_at") or 0) > now
            rows.append({
                "id": row["id"], "name": row.get("label") or row.get("email") or row["id"],
                "region": row.get("region", self.region), "enabled": enabled,
                "ready": bool(enabled and usable), "remaining": row.get("remaining"),
                "expires_at": row.get("expires_at"), "error": row.get("error") or "",
                "daily_tokens": row.get("daily_tokens"), "weight": row.get("weight", 1),
                "quota": row.get("quota"),
            })
        return rows

    def mutate(self, account_id, enabled=None, delete=False):
        result = self.pool.mutate(account_id, enabled, delete)
        if not delete:
            self._clear_error(account_id)
        return result

    def refresh_account(self, account_id):
        with self.lock:
            _, row = self.pool.find(account_id)
            self._refresh(row)
        return {"ok": True, "message": "凭证已刷新"}

    def import_credentials(self, data):
        rows = data if isinstance(data, list) else data.get("accounts", [data])
        count = 0
        for item in rows:
            if not isinstance(item, dict) or not item.get("access_token"):
                raise ProviderError("导入内容缺少 access_token", 400)
            self.pool.save(self.credentials_from_import(item))
            count += 1
        return {"ok": True, "count": count}

    def credentials_from_import(self, item):
        credentials = dict(item)
        credentials.setdefault("id", credentials.get("email") or uuid.uuid4().hex)
        credentials.setdefault("expires_at", time.time() + 3600)
        return credentials

    # ----------------------------------------------------------------------- login
    def start_login(self, region=None):
        raise ProviderError("%s 暂不支持在网关内授权" % self.label, 501)

    def poll_login(self, login_id):
        raise ProviderError("%s 暂不支持在网关内授权" % self.label, 501)

    def _prune_pending(self, now=None):
        now = time.time() if now is None else now
        for key, login in list(self.pending.items()):
            if login.get("expires_at", 0) <= now:
                callback = login.get("callback")
                if callback is not None:
                    callback.stop()
                self.pending.pop(key, None)

    def _loopback_login(self, callback, login_id, url, interval=2, timeout=300):
        """Store one loopback login and expose the management payload."""
        now = time.time()
        with self.lock:
            self._prune_pending(now)
            self.pending[login_id] = {"callback": callback, "expires_at": now + timeout,
                                      "interval": interval, "created_at": now}
        return {"login_id": login_id, "url": url, "interval": interval,
                "expires_at": now + timeout}

    def loopback_start(self, port, redirect_uri, build_url, exchange, interval=2, timeout=300,
                       pkce=True):
        """Run a PKCE authorization-code login through a loopback listener.

        ``build_url(state, challenge)`` returns the consent URL and
        ``exchange(query, login)`` turns the captured redirect into credentials.
        Providers whose client is not registered for PKCE pass ``pkce=False``.
        """
        with self.lock:
            self._prune_pending()
        verifier, challenge = pkce_pair()
        state = random_state()
        callback = LoopbackCallback(port, redirect_uri).start()
        try:
            url = build_url(state, challenge if pkce else "")
        except Exception:
            callback.stop()
            raise
        login_id = new_login_id()
        payload = self._loopback_login(callback, login_id, url, interval, timeout)
        with self.lock:
            self.pending[login_id].update({"verifier": verifier if pkce else "",
                                           "state": state, "redirect_uri": redirect_uri})
        payload["login_id"] = login_id
        return payload

    def _loopback_poll(self, login_id, exchange):
        with self.lock:
            login = self.pending.get(login_id)
            if not login:
                raise ProviderError("授权会话已失效，请重新发起授权", 400, "expired_token")
            callback = login["callback"]
            if callback.error:
                callback.stop()
                self.pending.pop(login_id, None)
                raise ProviderError("授权被拒绝：%s" % callback.error, 400, "access_denied")
            if callback.result is None:
                if time.time() > login["expires_at"]:
                    callback.stop()
                    self.pending.pop(login_id, None)
                    raise ProviderError("授权已超时，请重新发起授权", 400, "expired_token")
                return {"status": "pending", "interval": login["interval"]}
            result = callback.result
            callback.stop()
            try:
                credentials = exchange(result, login)
            except Exception:
                self.pending.pop(login_id, None)
                raise
            account_id = self.pool.save(credentials)
            self.pending.pop(login_id, None)
        return {"status": "authenticated", "account_id": account_id}

    # ---------------------------------------------------------------------- models
    def models(self):
        cached = self.catalog
        if cached and cached[0] > time.monotonic():
            return cached[1]
        try:
            rows = self.fetch_models()
        except ProviderError:
            if cached:
                return cached[1]
            raise
        if rows:
            self.catalog = (time.monotonic() + self.model_ttl, rows)
        return rows

    def fetch_models(self):
        raise NotImplementedError

    # --------------------------------------------------------------- credentials
    def note_account(self, account):
        """Record the account serving the current thread (request attribution)."""
        if account:
            self._local.account = account

    def current_account(self):
        return getattr(self._local, "account", "")

    def _access(self, account_id=None, exclude=()):
        row = self.pool.choose(account_id, exclude)
        if float(row.get("expires_at") or 0) <= time.time() + 120 and row.get("refresh_token"):
            row = self._refresh(row)
        if not row.get("access_token"):
            raise ProviderError("%s 账号需要重新授权" % self.label, 401)
        self.note_account(row["id"])
        return row

    def _refresh(self, row):
        try:
            grant = self.refresh_grant(row)
        except ProviderError as exc:
            self._set_error(row["id"], str(exc)[:200])
            raise
        credentials = dict(grant)
        credentials.setdefault("refresh_token", row.get("refresh_token"))
        credentials.setdefault("expires_at", time.time() + 3600)
        self.pool.save(credentials, account_id=row["id"])
        self._clear_error(row["id"])
        _, merged = self.pool.find(row["id"])
        return merged

    def refresh_grant(self, row):
        raise NotImplementedError

    def _set_error(self, account_id, message):
        path, row = self.pool.find(account_id)
        if row.get("error") == message:
            return
        row["error"] = message
        self.pool.save(row, account_id=account_id)

    def _clear_error(self, account_id):
        try:
            path, row = self.pool.find(account_id)
        except ProviderError:
            return
        if not row.get("error"):
            return
        row["error"] = ""
        self.pool.save(row, account_id=account_id)

    # -------------------------------------------------------------------- requests
    def url(self, path):
        raise NotImplementedError

    def extra_headers(self, token, row=None):
        return {}

    def _send(self, path, payload, token, row, headers=None, method=None, timeout=600):
        outgoing = {"Authorization": "Bearer " + token, "Content-Type": "application/json"}
        outgoing.update(self.extra_headers(token, row))
        outgoing.update(headers or {})
        body = None if payload is None else json.dumps(payload, ensure_ascii=False).encode("utf-8")
        return open_request(self.url(path), body, outgoing, method=method, timeout=timeout)

    def request(self, path, payload, headers=None, account_id=None, method=None, timeout=600):
        """Send one upstream request, rotating accounts on auth/quota failures."""
        excluded = []
        last = None
        for _ in range(self.max_attempts):
            row = self._access(account_id, excluded)
            try:
                response = self._send(path, payload, row["access_token"], row, headers,
                                      method, timeout)
            except ProviderError as exc:
                last = exc
                if account_id or exc.status not in (401, 403, 429):
                    raise
                self._set_error(row["id"], str(exc)[:200])
                excluded.append(row["id"])
                if exc.status == 401 and row.get("refresh_token"):
                    try:
                        self._refresh(row)
                    except ProviderError:
                        pass
                continue
            response.gateway_account = row["id"]
            return response
        raise last or ProviderError("%s 上游不可用" % self.label, 503)

    @staticmethod
    def relay(response):
        """Yield an upstream OpenAI SSE stream as whole ``data:`` frames."""
        try:
            for event in sse_events(response):
                yield b"data: " + json.dumps(event, ensure_ascii=False).encode("utf-8") + b"\n\n"
        finally:
            response.close()
        yield b"data: [DONE]\n\n"

    # ------------------------------------------------------------------ protocol
    def chat(self, payload, session=None):
        """Yield OpenAI chat SSE frames for one chat.completions request."""
        raise NotImplementedError

    def messages(self, payload, session=None):
        """Yield Anthropic SSE frames for one messages request."""
        return chat_frames_to_anthropic(self.chat(anthropic_to_chat(payload), session),
                                        payload.get("model"))

    # ------------------------------------------------------------ shared helpers
    @staticmethod
    def session_headers(session, header=None):
        return {header: session} if session and header else {}

    def load_config(self):
        path = self.directory / "config.json"
        if not path.exists():
            return {}
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except ValueError:
            return {}

    def save_config(self, value):
        atomic_json(self.directory / "config.json", value)

    @staticmethod
    def pick_model(rows, name):
        """Return the catalog entry id for a model alias, falling back to the name."""
        return name


class OpenAIProvider(NativeProvider):
    """Provider whose upstream speaks OpenAI ``chat.completions``.

    Subclasses set :attr:`chat_path` and may override :meth:`extra_headers` and
    :meth:`normalize` (for upstreams that ship a different chunk shape).
    """

    kind = "openai"
    chat_path = "/v1/chat/completions"
    models_path = "/v1/models"
    session_header = None

    def chat(self, payload, session=None):
        body = dict(payload)
        body["stream"] = True
        body.setdefault("stream_options", {"include_usage": True})
        headers = self.session_headers(session, self.session_header)
        response = self.request(self.chat_path, body, headers=headers)
        return self.relay(response)

    def fetch_models(self):
        response = self.request(self.models_path, None, method="GET", timeout=20)
        with response:
            try:
                data = json.load(response).get("data") or []
            except ValueError as exc:
                raise ProviderError("%s 模型目录返回无效 JSON" % self.label) from exc
        rows = []
        for item in data:
            if isinstance(item, str):
                rows.append(item)
            elif isinstance(item, dict) and item.get("id"):
                rows.append(item["id"])
        return rows
