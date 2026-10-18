"""MiniMax accounts, including the existing per-region credential files."""
import json
import time
import uuid

from .transport import ProviderError


class MiniMaxPool:
    def __init__(self, owner):
        self.owner = owner
        self.next_index = 0

    def records(self):
        paths = [self.owner.directory / (region + ".json") for region in self.owner.regions]
        paths += sorted((self.owner.directory / "accounts").glob("*.json"))
        rows = []
        for path in paths:
            if not path.exists():
                continue
            value = json.loads(path.read_text(encoding="utf-8"))
            if value.get("access_token"):
                value.setdefault("region", path.stem)
                value.setdefault("id", value["region"] + "-primary")
                value.setdefault("enabled", True)
                rows.append((path, value))
        return rows

    def save(self, region, credentials, account_id=None, new_account=False):
        from .minimax import atomic_json
        rows = self.records()
        target = next(((path, row) for path, row in rows if row["id"] == account_id), None)
        if new_account:
            target = next(((path, row) for path, row in rows if row["region"] == region
                           and row["access_token"] == credentials["access_token"]), None)
        if target:
            path, previous = target
        elif new_account and any(row["region"] == region for _, row in rows):
            identifier = uuid.uuid4().hex
            path, previous = self.owner.directory / "accounts" / (identifier + ".json"), {"id": identifier}
        else:
            path, previous = self.owner.directory / (region + ".json"), {"id": region + "-primary"}
        value = {**previous, **credentials, "region": region, "enabled": previous.get("enabled", True)}
        atomic_json(path, value)
        return value["id"]

    def public(self):
        return [{"id": row["id"], "name": row.get("label") or "MiniMax · " + row["id"][:10],
                 "region": row["region"], "enabled": row["enabled"],
                 "ready": row["enabled"] and bool(row.get("refresh_token") or row.get("expires_at", 0) > time.time()),
                 "expires_at": row.get("expires_at"), "remaining": None, "error": ""}
                for _, row in self.records()]

    def choose(self, region, account_id=None):
        rows = [(path, row) for path, row in self.records() if row["region"] == region
                and (row["id"] == account_id if account_id else row["enabled"] and
                     (row.get("refresh_token") or row.get("expires_at", 0) > time.time()))]
        if not rows:
            raise ProviderError("当前 MiniMax 区域没有启用的账号，请登录或启用账号", 503)
        item = rows[self.next_index % len(rows)] if not account_id else rows[0]
        if not account_id:
            self.next_index += 1
        return item

    def mutate(self, account_id, enabled=None, delete=False):
        from .minimax import atomic_json
        target = next(((path, row) for path, row in self.records() if row["id"] == account_id), None)
        if not target:
            raise ProviderError("账号不存在", 404)
        path, row = target
        if delete:
            path.unlink()
        else:
            if not isinstance(enabled, bool):
                raise ProviderError("enabled must be a boolean", 400)
            row["enabled"] = enabled
            atomic_json(path, row)
        return {"ok": True}
