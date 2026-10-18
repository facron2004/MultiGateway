"""One management contract for all four account pools and gateway ports."""
import json
import time
from urllib.parse import quote

import wb_proxy as wb

from .native import NAMES as NATIVE_NAMES
from .transport import ProviderError, json_request
from .operations import batch, checkin_state

BOARD_PROVIDERS = ("workbuddy", "trae", "minimax", "qoder") + NATIVE_NAMES
BOARD_LIMIT = 600
# A backend usage row and the gateway request that produced it are the same call
# when they finish within this many seconds of each other; the backend writes its
# row at completion, the gateway logs start time plus elapsed milliseconds.
MATCH_WINDOW = 4.0
BACKEND_STATUS = {"completed": "success", "error": "failed", "failed": "failed",
                  "cancelled": "cancelled", "processing": "processing"}


def _number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value


def _first(row, *keys):
    for key in keys:
        value = _number(row.get(key))
        if value is not None:
            return value
    return None


MODELSDEV_RATES = {
    "deepseek-v4.1-flash": (0.03, 0.01, 0.50),
    "deepseek-v3": (0.14, 0.014, 0.28),
    "deepseek-r1": (0.55, 0.14, 2.19),
    "glm-5.3-flash": (0.15, 0.03, 0.50),
    "step-5-preview": (0.20, 0.05, 1.00),
    "claude-3-5-sonnet": (3.00, 0.30, 15.00),
    "claude-3-7-sonnet": (3.00, 0.30, 15.00),
    "claude-3-5-haiku": (0.80, 0.08, 4.00),
    "gpt-4o": (2.50, 1.25, 10.00),
    "gpt-4o-mini": (0.15, 0.075, 0.60),
    "qwen-2.5-coder": (0.20, 0.05, 0.60),
    "minimax-text-01": (0.20, 0.05, 1.00),
}


def estimate_row_cost_usd(row):
    """Estimate API equivalent cost in USD per models.dev / OpenRouter per 1M tokens."""
    if not row:
        return 0.0
    prompt = float(row.get("input_tokens") or 0.0)
    cached = min(float(row.get("cached_tokens") or 0.0), prompt)
    fresh = max(prompt - cached, 0.0)
    output = float(row.get("output_tokens") or 0.0)
    if not prompt and not output:
        return 0.0

    model = str(row.get("model") or "").strip()
    try:
        import wb_pricing
        pricing_data = wb_pricing.compute_row({
            "model": model,
            "prompt_tokens": prompt,
            "cached_tokens": cached,
            "completion_tokens": output,
            "at": row.get("timestamp") or row.get("finished_at")
        })
        if pricing_data.get("known") and pricing_data.get("cny") is not None:
            usd_rate = wb_pricing.usd_cny() or 7.1
            return round(pricing_data["cny"] / usd_rate, 6)
    except Exception:
        pass

    low = model.lower()
    rates = None
    for key, val in MODELSDEV_RATES.items():
        if key in low or low in key:
            rates = val
            break

    if not rates:
        if "deepseek" in low:
            rates = (0.14, 0.014, 0.28)
        elif "glm" in low:
            rates = (0.15, 0.03, 0.50)
        elif "step" in low:
            rates = (0.20, 0.05, 1.00)
        elif "claude" in low and ("sonnet" in low or "opus" in low):
            rates = (3.00, 0.30, 15.00)
        elif "claude" in low:
            rates = (0.80, 0.08, 4.00)
        elif "gpt-4o-mini" in low:
            rates = (0.15, 0.075, 0.60)
        elif "gpt-4" in low or "gpt-5" in low:
            rates = (2.50, 1.25, 10.00)
        elif "qwen" in low:
            rates = (0.20, 0.05, 0.60)
        else:
            rates = (0.20, 0.05, 0.80)

    miss_rate, hit_rate, out_rate = rates
    cost = (fresh * miss_rate + cached * hit_rate + output * out_rate) / 1000000.0
    return round(cost, 6)


def account_view(name, row):
    if name in NATIVE_NAMES:
        return {"id": row["id"], "name": row.get("name") or row.get("email") or row["id"],
                "region": row.get("region", "intl"), "enabled": row.get("enabled", True),
                "ready": row.get("ready", False), "remaining": row.get("remaining"),
                "expires_at": row.get("expires_at"), "error": row.get("error") or "",
                "daily_tokens": row.get("daily_tokens"), "weight": row.get("weight", 1),
                "quota": row.get("quota"),
                "checkin": {"supported": False, "done": None, "updated_at": None}}
    if name == "minimax":
        return dict(row, checkin=checkin_state(name, row))
    if name == "trae":
        credits = row.get("account_credits") or {}
        return {"id": row["id"], "name": row.get("label") or row.get("user_id") or row["id"],
                "region": "cn", "enabled": row.get("model_enabled", True),
                "ready": row.get("model_eligible", row.get("is_valid", False)),
                "remaining": "unlimited" if credits.get("unlimited") else credits.get("remaining"),
                "expires_at": row.get("expires"), "error": "" if row.get("is_valid") else "会话需要更新",
                "checkin": checkin_state(name, row)}
    credits = row.get("credits") or {}
    enabled = row.get("enabled", True)
    blocked = any(row.get(key) for key in (
        "inCooldown", "reserveBlocked", "dailyLimitBlocked", "creditLimitReached", "balanceParked"))
    return {"id": row["uid"], "name": row.get("nickname") or row["uid"],
            "region": row.get("realm", "cn"), "enabled": enabled,
            "ready": row.get("ready", enabled and not blocked), "remaining": credits.get("remain"),
            "expires_at": row.get("expiresAt"), "error": row.get("lastError") or "",
            "daily_tokens": row.get("dailyTokensToday"), "weight": row.get("weight", 1),
            "checkin": checkin_state(name, row)}


class Control:
    def __init__(self, runtime):
        self.runtime = runtime
        self.name_cache = {}

    def call(self, name, path, payload=None, token=None):
        if name == "workbuddy":
            return json_request(self.runtime.origin + path, payload, {"X-Panel-Token": token}, timeout=60)
        body = None if payload is None else json.dumps(payload, ensure_ascii=False).encode("utf-8")
        with getattr(self.runtime, name).open(path, body, {"Content-Type": "application/json"}, timeout=60) as response:
            return json.load(response)

    def native(self, name):
        return getattr(self.runtime, "native", {}).get(name)

    def snapshot(self, name):
        error, rows, region = "", [], "cn"
        capabilities = {"login": True, "import": name != "minimax",
                        "credits": name != "minimax", "regions": name != "trae",
                        "checkin": name != "minimax", "credentials": True}
        provider = self.native(name)
        if provider is not None:
            capabilities = dict(provider.capabilities)
        try:
            if name == "workbuddy":
                wb.apply_daily_guards()
                rows, region = wb.account_views(), wb.CURRENT_REALM
            elif name == "minimax":
                with self.runtime.minimax.lock:
                    rows = self.runtime.minimax.pool.public()
                    region = self.runtime.minimax.region
            elif provider is not None:
                rows, region = provider.account_views(), provider.region
            elif name == "qoder":
                native = getattr(self.runtime.qoder, "native", None)
                if native is None:
                    raise ProviderError(self.runtime.qoder.error or "Qoder 后端正在启动", 503)
                rows, region = native.account_views(), native.CURRENT_REALM
            else:
                result = self.call(name, "/api/accounts")
                rows = result.get("accounts") or []
        except ProviderError as exc:
            error = str(exc)
        accounts = [dict(row) for row in rows] if provider is not None else [
            account_view(name, row) for row in rows]
        scoped = [row for row in accounts if row["region"] == region]
        return {"provider": name, "gateway": self.runtime.ports.info(name), "region": region,
                "accounts": accounts, "capabilities": capabilities, "error": error,
                "pool": {"total": len(accounts), "enabled": sum(row["enabled"] for row in scoped),
                         "ready": sum(bool(row["ready"] and row["enabled"]) for row in scoped),
                         "region_total": len(scoped)},
                "activity": self.runtime.activity.view(name)}

    def read(self, name, resource):
        if resource == "snapshot":
            return self.snapshot(name)
        if resource == "models":
            rows = self.runtime.provider_models(name)
            return {"provider": name, "data": [{"id": row["id"], "object": "model",
                    "context": row.get("context_window") or row.get("max_context"),
                    "owned_by": name, **{key: row[key] for key in (
                        "credits", "price_factor", "price_factor_peak", "price_factor_valley",
                        "off_peak", "off_peak_active_now", "display_name",
                        "context_window", "max_context", "max_mode") if key in row}} for row in rows]}
        raise ProviderError("not found", 404)

    @staticmethod
    def _model(name, model):
        """The unified entry prefixes models with the group; backends log bare names."""
        model = str(model or "")
        prefix, separator, rest = model.partition("/")
        return rest if separator and prefix == name and rest else model

    @staticmethod
    def _row(provider, record):
        """Fold one log record into the shape the usage board renders."""
        prompt = _first(record, "input_tokens", "prompt_tokens")
        cached = _first(record, "cached_tokens", "cache_read_input_tokens")
        output = _first(record, "output_tokens", "completion_tokens")
        status = record.get("status")
        return {"provider": provider,
                "model": Control._model(provider, record.get("model")),
                "session_id": str(record.get("session_id") or ""),
                "session_source": str(record.get("session_source") or ""),
                "protocol": str(record.get("protocol") or ""),
                "port": record.get("port"),
                "status": BACKEND_STATUS.get(status, status) or "success",
                "http_status": record.get("http_status"),
                "error": str(record.get("error") or ""),
                "timestamp": _number(record.get("timestamp")),
                "finished_at": None,
                "duration_ms": _first(record, "elapsed_ms", "duration_ms"),
                "ttft_ms": _first(record, "ttft_ms"),
                "input_tokens": prompt,
                "new_input_tokens": None if prompt is None else max(prompt - (cached or 0), 0),
                "cached_tokens": cached,
                "output_tokens": output,
                "reasoning_tokens": _first(record, "reasoning_tokens"),
                "tokens_per_sec": _first(record, "tokens_per_sec"),
                "credits": _first(record, "credit", "credits", "credits_consumed"),
                "cost_usd": _number(record.get("cost_usd")),
                "account": str(record.get("account") or record.get("account_id") or "")}
        if row["cost_usd"] is None:
            row["cost_usd"] = estimate_row_cost_usd(row)
        return row

    def gateway_rows(self, limit):
        """Requests this gateway served; carries the session and in-flight state."""
        rows = []
        for record in self.runtime.activity.records(limit):
            row = self._row(record["provider"], record)
            if row["timestamp"] is not None and row["duration_ms"] is not None:
                row["finished_at"] = row["timestamp"] + row["duration_ms"] / 1000
            rows.append(row)
        return rows

    def backend_rows(self, name, limit):
        """Per-request rows the provider backend writes, including the served account."""
        try:
            if name == "workbuddy":
                raw = wb.recent_usage(limit)
            elif name == "qoder":
                native = getattr(self.runtime.qoder, "native", None)
                raw = None if native is None else native.recent_usage(limit)
            elif name == "trae":
                raw = self.call(name, "/api/usage/records")
            else:
                return []
        except (ProviderError, OSError, ValueError):
            return []
        rows = raw if isinstance(raw, list) else (raw or {}).get("rows", (raw or {}).get("records", []))
        result = []
        for record in list(rows)[:limit]:
            if not isinstance(record, dict):
                continue
            row = self._row(name, record)
            row["finished_at"] = _first(record, "at", "ts", "time")
            if row["finished_at"] is None:
                row["finished_at"] = _number(record.get("timestamp"))
            if row["finished_at"] is not None:
                row["timestamp"] = row["finished_at"] - (row["duration_ms"] or 0) / 1000
            result.append(row)
        return result

    def board(self, limit=BOARD_LIMIT):
        """Every agent's requests in one table: gateway rows enriched with backend detail."""
        rows = self.gateway_rows(limit)
        backend = [row for name in BOARD_PROVIDERS for row in self.backend_rows(name, limit)]
        buckets = {}
        for row in backend:
            buckets.setdefault((row["provider"], row["model"]), []).append(row)
        matched = set()
        for row in rows:
            candidates = buckets.get((row["provider"], row["model"])) or []
            if not candidates or row["finished_at"] is None:
                continue
            best, gap = None, MATCH_WINDOW + 1
            for item in candidates:
                if id(item) in matched or item["finished_at"] is None:
                    continue
                distance = abs(item["finished_at"] - row["finished_at"])
                if distance < gap:
                    best, gap = item, distance
            if best is None:
                continue
            matched.add(id(best))
            for key in ("input_tokens", "cached_tokens", "output_tokens", "reasoning_tokens",
                        "ttft_ms", "tokens_per_sec", "credits", "account"):
                if best[key] is not None and best[key] != "":
                    row[key] = best[key]
            if row["input_tokens"] is not None:
                row["new_input_tokens"] = max(row["input_tokens"] - (row["cached_tokens"] or 0), 0)
        rows += [row for row in backend if id(row) not in matched]
        for row in rows:
            if row["tokens_per_sec"] is None and row["output_tokens"] and row["duration_ms"]:
                window = (row["duration_ms"] - (row["ttft_ms"] or 0)) / 1000
                if window > 0.2:
                    row["tokens_per_sec"] = round(row["output_tokens"] / window, 2)
        rows.sort(key=lambda row: row["timestamp"] or 0, reverse=True)
        rows = rows[:limit]
        providers = {name: 0 for name in BOARD_PROVIDERS}
        models, accounts = {}, {}
        for row in rows:
            providers[row["provider"]] = providers.get(row["provider"], 0) + 1
            if row["model"]:
                models[(row["provider"], row["model"])] = models.get((row["provider"], row["model"]), 0) + 1
            if row["account"]:
                key = (row["provider"], row["account"])
                accounts[key] = accounts.get(key, 0) + 1
        names = {name: self._account_names(name) for name in {key[0] for key in accounts}}
        for row in rows:
            row["account_name"] = names.get(row["provider"], {}).get(row["account"], "")
            row["cost_usd"] = estimate_row_cost_usd(row)
        by_count = lambda items: sorted(items.items(), key=lambda item: -item[1])
        return {"rows": rows, "generated_at": time.time(),
                "facets": {"providers": [{"id": key, "count": value} for key, value in providers.items()],
                           "models": [{"provider": key[0], "model": key[1], "count": value}
                                      for key, value in by_count(models)],
                           "accounts": [{"provider": key[0], "id": key[1],
                                         "name": names.get(key[0], {}).get(key[1]) or key[1],
                                         "count": value} for key, value in by_count(accounts)]}}

    def _account_names(self, name, ttl=60):
        cached = self.name_cache.get(name)
        if cached and cached[0] > time.monotonic():
            return cached[1]
        try:
            provider = self.native(name)
            if name == "workbuddy":
                records = wb.account_views()
            elif name == "minimax":
                with self.runtime.minimax.lock:
                    records = self.runtime.minimax.pool.public()
            elif provider is not None:
                records = provider.account_views()
            elif name == "qoder":
                native = getattr(self.runtime.qoder, "native", None)
                records = [] if native is None else native.account_views()
            else:
                records = self.call(name, "/api/accounts").get("accounts") or []
            if provider is not None:
                mapping = {row["id"]: row.get("name") or row["id"] for row in records}
            else:
                mapping = {row["id"]: row["name"] for row in (account_view(name, record) for record in records)}
        except (ProviderError, OSError, ValueError, KeyError, TypeError, AttributeError):
            mapping = {}
        self.name_cache[name] = (time.monotonic() + ttl, mapping)
        return mapping

    def action(self, name, action, payload, token, local=True):
        provider = self.native(name)
        if action == "batch":
            return batch(self, name, payload, token)
        if action == "gateway":
            return self.runtime.ports.configure(name, payload.get("port"), payload.get("enabled"), host=payload.get("host"))
        if action == "region":
            region = payload.get("region")
            if provider is not None:
                raise ProviderError("%s 没有区域切换" % provider.label, 400)
            if name == "minimax":
                self.runtime.minimax.set_region(region)
                return {"ok": True}
            if name == "trae" or region not in ("cn", "intl"):
                raise ProviderError("区域不受支持", 400)
            result = self.call(name, "/realm", {"realm": region}, token)
            if name == "qoder":
                self.runtime.qoder.catalog = None
            return result
        if action == "login":
            if provider is not None:
                return provider.start_login(payload.get("region"))
            if name == "minimax":
                return self.runtime.minimax.start_login(payload.get("region", "cn"))
            if name == "trae":
                if not local:
                    raise ProviderError("请在网关电脑上启动 Trae 登录", 403)
                return self.runtime.trae.start_login(self.runtime.origin + "/trae")
            return self.call(name, "/accounts/login/start", {"realm": payload.get("region", "cn")}, token)
        if action == "poll":
            if provider is not None:
                return provider.poll_login(str(payload.get("login_id") or payload.get("state") or ""))
            if name == "minimax":
                return self.runtime.minimax.poll_login(payload.get("login_id", ""))
            if name == "trae":
                return {"status": "pending"}
            return self.call(name, "/accounts/login/poll?state=" + quote(payload.get("state", "")), token=token)
        identifier = payload.get("id")
        if action in ("enable", "delete", "credits", "credentials", "checkin") and not isinstance(identifier, str):
            raise ProviderError("账号 id 必填", 400)
        if provider is not None:
            if action == "credentials":
                return provider.refresh_account(identifier)
            if action in ("enable", "delete"):
                return provider.mutate(identifier, payload.get("enabled"), action == "delete")
            if action == "import":
                data = payload.get("data")
                if not isinstance(data, (dict, list)):
                    raise ProviderError("导入内容必须为 JSON 对象或数组", 400)
                return provider.import_credentials(data)
            raise ProviderError("%s 不支持该操作" % provider.label, 400)
        if action == "credentials" and name in ("minimax", "trae"):
            return getattr(self.runtime, name).refresh_account(identifier)
        if name == "minimax":
            with self.runtime.minimax.lock:
                if action in ("enable", "delete"):
                    return self.runtime.minimax.pool.mutate(identifier, payload.get("enabled"), action == "delete")
            raise ProviderError("MiniMax 使用官方设备授权添加账号", 400)
        if action == "import":
            data = payload.get("data")
            if not isinstance(data, (dict, list)):
                raise ProviderError("导入内容必须为 JSON 对象或数组", 400)
            if name == "trae":
                rows = data if isinstance(data, list) else data.get("accounts", [data])
                for row in rows:
                    self.call(name, "/api/web-auth", row)
                return {"ok": True, "count": len(rows)}
            return self.call(name, "/accounts/import", {"data": data}, token)
        if action == "enable" and not isinstance(payload.get("enabled"), bool):
            raise ProviderError("enabled must be a boolean", 400)
        if name == "trae":
            routes = {"enable": "/api/accounts/model-enabled", "delete": "/api/accounts/remove",
                      "credits": "/api/checkin/credits/" + quote(identifier or "", safe=""),
                      "checkin": "/api/checkin/account/" + quote(identifier or "", safe="")}
            if action not in routes:
                raise ProviderError("not found", 404)
            return self.call(name, routes[action], None if action == "credits" else
                             {"account_id": identifier, "enabled": payload.get("enabled")})
        routes = {"enable": "/accounts/set", "delete": "/accounts/delete", "credits": "/accounts/credits/fetch",
                  "credentials": "/accounts/refresh",
                  "checkin": "/accounts/checkin"}
        if action not in routes:
            raise ProviderError("not found", 404)
        return self.call(name, routes[action], {"uid": identifier, "enabled": payload.get("enabled")}, token)
