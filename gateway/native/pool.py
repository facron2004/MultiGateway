"""Multi-account credential pool shared by the native OAuth providers.

Each account is one JSON file under ``<provider>/accounts/<id>.json`` so a user
can back up, copy or delete a single account, matching how the WorkBuddy pool
behaves. Rotation is round-robin over usable accounts; a request that fails with
an authentication or quota error retries the next account instead of failing.
"""
import threading
import time
import uuid
from pathlib import Path

from ..files import atomic_json
from ..transport import ProviderError


class AccountPool:
    def __init__(self, provider):
        self.provider = provider
        self.lock = threading.RLock()
        self.index = 0

    @property
    def directory(self):
        return Path(self.provider.directory) / "accounts"

    def records(self):
        """``[(path, row)]`` for every stored account, oldest filename first."""
        self.directory.mkdir(parents=True, exist_ok=True)
        rows = []
        for path in sorted(self.directory.glob("*.json")):
            try:
                value = __import__("json").loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                continue
            if not isinstance(value, dict) or not value.get("access_token"):
                continue
            value.setdefault("id", path.stem)
            value.setdefault("enabled", True)
            value.setdefault("region", self.provider.region)
            rows.append((path, value))
        return rows

    def find(self, account_id):
        for path, row in self.records():
            if row["id"] == account_id:
                return path, row
        raise ProviderError("账号不存在", 404)

    def save(self, credentials, account_id=None):
        """Insert or update one account; returns the stable account id."""
        credentials = dict(credentials)
        if account_id:
            _, previous = self.find(account_id)
            value = {**previous, **credentials, "id": account_id}
            target = self.directory / (account_id + ".json")
        else:
            value = {**credentials, "id": credentials.get("id") or uuid.uuid4().hex}
            target = self.directory / (value["id"] + ".json")
            if target.exists():
                previous = __import__("json").loads(target.read_text(encoding="utf-8"))
                value = {**previous, **value}
        value.setdefault("enabled", True)
        value.setdefault("region", self.provider.region)
        value.setdefault("created_at", time.time())
        value["updated_at"] = time.time()
        with self.lock:
            atomic_json(target, value)
        self.provider.catalog = None
        return value["id"]

    def choose(self, account_id=None, exclude=()):
        excluded = set(exclude)
        rows = [(path, row) for path, row in self.records()
                if row["id"] not in excluded and (
                    account_id is not None or row.get("enabled", True))]
        if account_id is not None:
            for path, row in rows:
                if row["id"] == account_id:
                    self._note(row["id"])
                    return row
            raise ProviderError("账号不存在或已停用", 404)
        if not rows:
            raise ProviderError("当前 %s 账号池没有可用账号，请先在面板完成 OAuth 授权"
                                % self.provider.label, 503)
        with self.lock:
            row = rows[self.index % len(rows)][1]
            self.index = (self.index + 1) % max(len(rows), 1)
        self._note(row["id"])
        return row

    def _note(self, account_id):
        """Tell the provider which account served the current thread."""
        note = getattr(self.provider, "note_account", None)
        if note is not None:
            note(account_id)

    def public(self):
        return [dict(row) for _, row in self.records()]

    def mutate(self, account_id, enabled=None, delete=False):
        path, row = self.find(account_id)
        if delete:
            with self.lock:
                path.unlink()
            self.provider.catalog = None
            return {"ok": True}
        if not isinstance(enabled, bool):
            raise ProviderError("enabled must be a boolean", 400)
        row["enabled"] = enabled
        with self.lock:
            atomic_json(path, row)
        return {"ok": True}
