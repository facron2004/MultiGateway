"""MiniMax Code OAuth device flow, based on the official MIT client.

No CLI installation or API key is required. Credentials stay outside the exe.
"""
import base64
import hashlib
import json
import os
import secrets
import threading
import time
import uuid
from pathlib import Path

from .minimax_pool import MiniMaxPool
from .transport import ProviderError, json_request, open_request

REGIONS = {
    "cn": ("https://account.minimax.cn", "https://agent.minimax.cn"),
    "global": ("https://account.minimax.io", "https://agent.minimax.io"),
}
OAUTH = {"client_id": "mcode-public", "scope": "agent.default", "audience": "agent-backend"}


def atomic_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + "." + uuid.uuid4().hex + ".tmp")
    try:
        with open(temporary, "w", encoding="utf-8") as handle:
            json.dump(value, handle, ensure_ascii=False, indent=2)
        os.replace(temporary, path)
    finally:
        if temporary.exists():
            temporary.unlink()


class MiniMax:
    def __init__(self, directory, regions=None):
        self.directory = Path(directory)
        self.regions = regions or REGIONS
        self.lock = threading.RLock()
        self.pending = {}
        self.catalog = {}
        self.region = "cn"
        self.pool = MiniMaxPool(self)
        config = self.directory / "config.json"
        if config.exists():
            self.region = json.loads(config.read_text(encoding="utf-8")).get("region", "cn")
        if self.region not in self.regions:
            self.region = "cn"

    def _load(self, region):
        path = self.directory / (region + ".json")
        return json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}

    def _save_grant(self, region, grant, previous=None, account_id=None, new_account=False):
        token = grant.get("access_token")
        if not token:
            raise ProviderError("MiniMax OAuth did not return an access token")
        credentials = {
            "access_token": token,
            "refresh_token": grant.get("refresh_token") or (previous or {}).get("refresh_token", ""),
            "expires_at": time.time() + float(grant.get("expires_in") or 3600),
        }
        return self.pool.save(region, credentials, account_id, new_account)

    def status(self):
        with self.lock:
            sessions = {}
            for region in self.regions:
                credentials = next((row for _, row in self.pool.records() if row["region"] == region), {})
                sessions[region] = {
                    "signed_in": bool(credentials.get("access_token")),
                    "expires_at": credentials.get("expires_at"),
                }
            return {"region": self.region, "sessions": sessions, "accounts": self.pool.public()}

    def set_region(self, region):
        if region not in self.regions:
            raise ProviderError("unknown MiniMax region", 400)
        with self.lock:
            atomic_json(self.directory / "config.json", {"region": region})
            self.region = region

    def start_login(self, region):
        if region not in self.regions:
            raise ProviderError("unknown MiniMax region", 400)
        verifier = secrets.token_urlsafe(32)
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode("ascii")).digest()).decode().rstrip("=")
        grant = json_request(self.regions[region][0] + "/oauth2/device/code", {
            **OAUTH, "code_challenge": challenge, "code_challenge_method": "S256",
        }, form=True)
        now = time.time()
        legacy = not grant.get("device_code") and bool(grant.get("expired_in"))
        expiry = ((float(grant.get("expired_in", 0)) / 1000 - now) if legacy
                  else float(grant.get("expires_in", 0)))
        device = grant.get("device_code") or grant.get("user_code")
        url = grant.get("verification_uri_complete") or grant.get("verification_uri") or grant.get("verification_url")
        if not device or not grant.get("user_code") or not url or expiry <= 0:
            raise ProviderError("incomplete MiniMax device authorization response")
        interval = max(1, float(grant.get("interval") or 5) / (1000 if legacy else 1))
        login_id = uuid.uuid4().hex
        with self.lock:
            self.pending = {key: value for key, value in self.pending.items() if value["expires_at"] > now}
            self.pending[login_id] = {
                "region": region, "verifier": verifier, "device": device,
                "parameter": "user_code" if legacy else "device_code",
                "expires_at": now + expiry, "interval": interval, "next_poll": now + interval,
            }
        return {"login_id": login_id, "user_code": grant["user_code"], "url": url,
                "interval": interval, "expires_at": now + expiry}

    def poll_login(self, login_id):
        with self.lock:
            pending = self.pending.get(login_id)
            if not pending or pending["expires_at"] <= time.time():
                self.pending.pop(login_id, None)
                raise ProviderError("device authorization expired; start sign-in again", 400, "expired_token")
            if time.time() < pending["next_poll"]:
                return {"status": "pending", "interval": pending["interval"]}
            pending["next_poll"] = time.time() + pending["interval"]
            try:
                result = json_request(self.regions[pending["region"]][0] + "/oauth2/token", {
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                    "client_id": OAUTH["client_id"], "code_verifier": pending["verifier"],
                    pending["parameter"]: pending["device"],
                }, form=True)
                state = result.get("status") or result.get("error")
            except ProviderError as exc:
                if exc.code not in ("authorization_pending", "slow_down"):
                    self.pending.pop(login_id, None)
                    raise
                state, result = exc.code, {}
            if state in ("pending", "authorization_pending", "slow_down"):
                if state == "slow_down":
                    pending["interval"] += 5
                    pending["next_poll"] = time.time() + pending["interval"]
                return {"status": "pending", "interval": pending["interval"]}
            if state in ("denied", "access_denied", "expired", "expired_token"):
                self.pending.pop(login_id, None)
                raise ProviderError("MiniMax authorization " + state, 400, state)
            account_id = self._save_grant(pending["region"], result, new_account=True)
            self.set_region(pending["region"])
            self.pending.pop(login_id, None)
            return {"status": "authenticated", "region": self.region, "account_id": account_id}

    def refresh_account(self, account_id):
        with self.lock:
            _, credentials = self.pool.choose(self.region, account_id)
            if not credentials.get("refresh_token"):
                raise ProviderError("账号缺少刷新凭证，请重新授权登录", 401)
            grant = json_request(self.regions[self.region][0] + "/oauth2/token", {
                **OAUTH, "grant_type": "refresh_token", "refresh_token": credentials["refresh_token"],
            }, form=True)
            self._save_grant(self.region, grant, credentials, credentials["id"])
            return {"ok": True}

    def access_token(self, account_id=None):
        with self.lock:
            _, credentials = self.pool.choose(self.region, account_id)
            if not credentials.get("access_token"):
                raise ProviderError("MiniMax Code is not signed in; sign in on the gateway dashboard", 503)
            if credentials.get("expires_at", 0) <= time.time() + 300:
                if not credentials.get("refresh_token"):
                    raise ProviderError("MiniMax session expired; sign in again", 401)
                grant = json_request(self.regions[self.region][0] + "/oauth2/token", {
                    **OAUTH, "grant_type": "refresh_token", "refresh_token": credentials["refresh_token"],
                }, form=True)
                self._save_grant(self.region, grant, credentials, credentials["id"])
                credentials["access_token"] = grant["access_token"]
            return self.region, credentials["access_token"], credentials["id"]

    def models(self):
        region = self.region
        cached = self.catalog.get(region)
        if cached and cached[0] > time.monotonic():
            return cached[1]
        try:
            result = json_request(self.regions[region][1] + "/mavis/api/v1/models?region=" +
                                  ("cn" if region == "cn" else "en") + "&buildEnv=prod", timeout=10)
            provider = next(p for p in result.get("providers", []) if p.get("providerId") == "minimax")
            config = provider["config"]
            identifiers = config.get("models") or {}
            ordered = [mid for mid in config.get("model_order", []) if mid in identifiers]
            ordered += sorted(set(identifiers) - set(ordered))
            if not ordered:
                raise ValueError("empty model catalog")
            self.catalog[region] = (time.monotonic() + 300, ordered)
            return ordered
        except (ProviderError, KeyError, StopIteration, ValueError):
            if cached:
                return cached[1]
            raise ProviderError("MiniMax model catalog is currently unavailable", 503)

    def open_messages(self, payload, session=None):
        region, token, account = self.access_token()
        if session:
            try:
                session = str(uuid.UUID(str(session)))
            except ValueError:
                session = str(uuid.uuid5(uuid.NAMESPACE_URL, str(session)))
        else:
            session = str(uuid.uuid4())
        headers = {
            "Authorization": "Bearer " + token, "x-api-key": "sk-xxx",
            "anthropic-version": "2023-06-01", "Content-Type": "application/json",
            "User-Agent": "MiniMaxAgent", "X-Mavis-Agent-Id": "main",
            "X-Mavis-Session-Id": session, "X-Mavis-Timezone-Offset": str(-time.timezone),
            "Accept": "text/event-stream" if payload.get("stream") else "application/json",
        }
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        response = open_request(self.regions[region][1] + "/mavis/api/v1/llm/v1/messages",
                                body, headers, timeout=600)
        response.gateway_account = account
        return response
