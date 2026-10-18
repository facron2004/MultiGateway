"""Alibaba Qwen Code account provider.

Qwen Code ships a public OAuth client for its CLI, so the gateway can reuse the
vendor's own free coding quota (~2,000 requests/day per account, per the
research note; subject to change) without an API key. The flow is RFC 8628
device authorization with PKCE S256:

1. ``POST https://chat.qwen.ai/api/v1/oauth2/device/code`` (form-encoded) with
   the public client id, scope ``openid profile email model.completion`` and the
   PKCE challenge. The response carries ``user_code``,
   ``verification_uri(_complete)``, ``expires_in`` and usually ``interval``.
2. The dashboard shows the code and opens the verification page; the gateway
   polls ``POST .../oauth2/token`` with
   ``grant_type=urn:ietf:params:oauth:grant-type:device_code``. RFC 8628
   ``authorization_pending`` means "keep polling", ``slow_down`` means "poll less
   often" (we grow the interval by 50%, capped at 10s), and ``expired_token`` /
   ``access_denied`` are terminal.
3. The grant (``access_token`` + ``refresh_token`` + ``resource_url``) authenticates
   the OpenAI-compatible portal at ``https://portal.qwen.ai/v1``. Refreshes reuse
   the same token endpoint with ``grant_type=refresh_token``.

The portal is OpenAI ``chat.completions`` end to end (including ``usage`` and
``reasoning_content``), so the provider only contributes authentication,
headers and a model catalog; streaming is relayed by
:meth:`gateway.native.base.NativeProvider.relay`.

Credentials follow the Qwen Code CLI convention: the token is single-session per
authorization, so a user should import ``~/.qwen/oauth_creds.json`` **or** run
this flow, not both at once.
"""
import base64
import hashlib
import json
import time
import uuid

from ..transport import ProviderError, json_request
from .base import OpenAIProvider
from .oauth import expires_at_of, new_login_id, pkce_pair

#: OAuth host (device authorization + token exchange).
QWEN_AUTH_BASE = "https://chat.qwen.ai"
#: API origin of the OpenAI-compatible portal. Paths add their own ``/v1``.
QWEN_API_ORIGIN = "https://portal.qwen.ai"

DEVICE_CODE_PATH = "/api/v1/oauth2/device/code"
TOKEN_PATH = "/api/v1/oauth2/token"

#: Installed-application client shipped in the public Qwen Code CLI.
CLIENT_ID = "f0304373b74a44d2b584a3fb70ca9e56"
SCOPE = "openid profile email model.completion"
DEVICE_GRANT = "urn:ietf:params:oauth:grant-type:device_code"
USER_AGENT = "QwenCode/gateway"

#: RFC 8628 allows the server to omit ``interval``; the CLI defaults to 2s and
#: the gateway stays conservative so the first poll never races the user.
DEFAULT_INTERVAL = 5.0
SLOW_DOWN_FACTOR = 1.5
MAX_INTERVAL = 10.0

#: Catalog served when no account exists (or the portal catalog is unreachable).
#: ``coder-model``/``vision-model`` are the documented portal aliases; the
#: ``qwen3*`` ids are the underlying names clients request directly.
FALLBACK_MODELS = ("coder-model", "vision-model", "qwen3-coder-plus",
                   "qwen3-coder-flash", "qwen3-max", "qwen3.5-plus")


def decode_id_token(token):
    """Decode a JWT payload without signature verification.

    The id_token is only used for display (email/name) and account identity, so
    verification is unnecessary and would need the vendor's keys. Any malformed
    token yields an empty mapping instead of failing the login.
    """
    try:
        _, payload, _ = str(token).split(".", 2)
        payload += "=" * (-len(payload) % 4)
        value = json.loads(base64.urlsafe_b64decode(payload.encode("ascii")))
    except (AttributeError, UnicodeError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


class Qwen(OpenAIProvider):
    """Qwen Code OAuth provider (device flow, OpenAI-compatible portal)."""

    name = "qwen"
    label = "Qwen Code"
    region = "intl"
    login_kind = "device"
    capabilities = {"login": True, "import": True, "credits": False,
                    "regions": False, "checkin": False, "credentials": True}
    chat_path = "/v1/chat/completions"
    models_path = "/v1/models"

    # ------------------------------------------------------------------ endpoints
    def url(self, path):
        return QWEN_API_ORIGIN + path

    @staticmethod
    def auth_url(path):
        return QWEN_AUTH_BASE + path

    # --------------------------------------------------------------------- oauth
    def start_login(self, region=None):
        """Create an RFC 8628 device authorization bound to a PKCE verifier."""
        verifier, challenge = pkce_pair()
        grant = self._oauth_request(self.auth_url(DEVICE_CODE_PATH), {
            "client_id": CLIENT_ID,
            "scope": SCOPE,
            "code_challenge": challenge,
            "code_challenge_method": "S256",
        })
        device_code = str(grant.get("device_code") or "")
        user_code = str(grant.get("user_code") or "")
        url = (grant.get("verification_uri_complete") or grant.get("verification_uri")
               or grant.get("verification_url"))
        expires_in = _number(grant.get("expires_in"))
        if not device_code or not user_code or not url or expires_in <= 0:
            raise ProviderError("Qwen 设备授权响应不完整，请稍后重试", 502)
        interval = max(1.0, _number(grant.get("interval"), DEFAULT_INTERVAL))
        now = time.time()
        login_id = new_login_id()
        with self.lock:
            self._prune_pending(now)
            self.pending[login_id] = {
                "verifier": verifier, "device_code": device_code,
                "interval": interval, "next_poll": now + interval,
                "expires_at": now + expires_in, "created_at": now,
            }
        return {"login_id": login_id, "url": url, "user_code": user_code,
                "interval": interval, "expires_at": now + expires_in}

    def poll_login(self, login_id):
        """Poll once for the device grant; the dashboard calls this per interval."""
        with self.lock:
            login = self.pending.get(login_id)
            if not login:
                raise ProviderError("授权会话已失效，请重新发起授权", 400, "expired_token")
            now = time.time()
            if login["expires_at"] <= now:
                self.pending.pop(login_id, None)
                raise ProviderError("Qwen 授权已过期，请重新发起授权", 400, "expired_token")
            if login.get("polling") or now < login["next_poll"]:
                return {"status": "pending", "interval": login["interval"]}
            login["polling"] = True
            login["next_poll"] = now + login["interval"]
            device_code, verifier = login["device_code"], login["verifier"]
        try:
            grant = self._oauth_request(self.auth_url(TOKEN_PATH), {
                "grant_type": DEVICE_GRANT,
                "client_id": CLIENT_ID,
                "device_code": device_code,
                "code_verifier": verifier,
            })
        except ProviderError as exc:
            return self._poll_failure(login_id, exc)
        try:
            credentials = self._save_grant(grant)
            account_id = self.pool.save(credentials)
        except ProviderError:
            with self.lock:
                login = self.pending.get(login_id)
                if login is not None:
                    login["polling"] = False
            raise
        with self.lock:
            self.pending.pop(login_id, None)
        return {"status": "authenticated", "account_id": account_id}

    def _poll_failure(self, login_id, exc):
        """Translate one failed poll into either a pending result or a terminal error."""
        with self.lock:
            login = self.pending.get(login_id)
            if login is not None:
                login["polling"] = False
            if exc.code == "authorization_pending":
                return {"status": "pending",
                        "interval": login["interval"] if login else DEFAULT_INTERVAL}
            if exc.code == "slow_down" and login is not None:
                login["interval"] = min(login["interval"] * SLOW_DOWN_FACTOR, MAX_INTERVAL)
                login["next_poll"] = time.time() + login["interval"]
                return {"status": "pending", "interval": login["interval"]}
            if login is not None:
                self.pending.pop(login_id, None)
        if exc.code == "access_denied":
            raise ProviderError("Qwen 授权已被拒绝，请重新发起授权", 400, "access_denied")
        if exc.code in ("expired_token", "invalid_grant"):
            raise ProviderError("Qwen 授权已过期，请重新发起授权", 400, "expired_token")
        raise ProviderError("Qwen 授权失败：%s" % exc, exc.status, exc.code) from exc

    def refresh_grant(self, row):
        """Exchange the stored refresh token for a new access token."""
        refresh_token = row.get("refresh_token")
        if not refresh_token:
            raise ProviderError("Qwen 账号缺少刷新凭证，请重新授权登录", 401, "invalid_grant")
        try:
            grant = self._oauth_request(self.auth_url(TOKEN_PATH), {
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": refresh_token,
            })
        except ProviderError as exc:
            if exc.status in (400, 401):
                raise ProviderError("Qwen 登录凭据已失效，请在面板重新授权",
                                    401, "invalid_grant") from exc
            raise
        return self._save_grant(grant, row)

    # ------------------------------------------------------------------ accounts
    def _save_grant(self, grant, previous=None):
        """Build the credential record stored in the account pool."""
        token = grant.get("access_token")
        if not token:
            raise ProviderError("Qwen 授权未返回 access_token", 502)
        previous = previous or {}
        claims = decode_id_token(grant.get("id_token")) if grant.get("id_token") else {}
        email = str(claims.get("email") or claims.get("preferred_username")
                    or grant.get("email") or grant.get("user_id")
                    or previous.get("email") or "")
        credentials = {
            "access_token": token,
            "refresh_token": grant.get("refresh_token") or previous.get("refresh_token", ""),
            "expires_at": expires_at_of(grant),
            "email": email,
            "label": str(claims.get("name") or email or previous.get("label", "")),
            "subject": str(claims.get("sub") or grant.get("user_id") or previous.get("subject", "")),
            "resource_url": grant.get("resource_url") or previous.get("resource_url", ""),
        }
        credentials["id"] = previous.get("id") or email or _identity(credentials)
        return credentials

    def extra_headers(self, token, row=None):
        """Headers the Qwen Code client sends; the portal gates on some of them."""
        return {
            "User-Agent": USER_AGENT,
            "X-DashScope-UserAgent": USER_AGENT,
            "X-DashScope-CacheControl": "enable",
            "X-DashScope-AuthType": "qwen-oauth",
        }

    # -------------------------------------------------------------------- models
    def fetch_models(self):
        """Read the portal catalog, falling back to the static list on any failure."""
        if not self.pool.records():
            return list(FALLBACK_MODELS)
        try:
            rows = super().fetch_models()
        except ProviderError:
            return list(FALLBACK_MODELS)
        return rows or list(FALLBACK_MODELS)

    # -------------------------------------------------------------------- helpers
    def _oauth_request(self, url, payload):
        headers = {"Accept": "application/json", "User-Agent": USER_AGENT,
                   "x-request-id": uuid.uuid4().hex}
        return json_request(url, payload, headers=headers, form=True, timeout=30)


def _number(value, default=0.0):
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _identity(credentials):
    """Stable account id when the grant carries no email/sub claim.

    The portal does not always return an ``id_token``; in that case the refresh
    token (or access token) is hashed so repeated polls of one grant resolve to
    the same account file instead of spawning duplicates.
    """
    seed = credentials.get("refresh_token") or credentials.get("access_token") or ""
    return "qwen-" + hashlib.sha256(seed.encode("utf-8")).hexdigest()[:16]
