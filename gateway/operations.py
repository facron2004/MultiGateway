"""Serial account actions shared by the toolbar and account cards."""
import time

from .transport import ProviderError


def checkin_state(provider, row):
    if provider == "trae":
        return {"supported": True, "done": row.get("checked_in"),
                "updated_at": row.get("checkin_updated_at")}
    if provider == "minimax":
        return {"supported": False, "done": None, "updated_at": None}
    credits = row.get("credits") or {}
    state = credits.get("checkin") or {}
    return {"supported": row.get("canCheckin", row.get("realm") == "cn"),
            "done": state.get("today_checked_in"), "updated_at": row.get("lastCheckin")}


def outcome(result):
    rows = result.get("results")
    if isinstance(rows, list):
        if not rows:
            return False, "后端未执行操作，请检查账号状态"
        result = rows[0]
    ok = result.get("ok", result.get("success", True)) is not False
    message = result.get("error") or result.get("message") or result.get("msg") or ("操作完成" if ok else "后端操作失败")
    return ok, str(message)


def batch(control, provider, payload, token):
    operation = payload.get("operation")
    if operation not in ("checkin", "credits", "credentials", "enable", "disable"):
        raise ProviderError("不支持的账号操作", 400)
    snapshot = control.snapshot(provider)
    if operation in ("checkin", "credits", "credentials") and not snapshot["capabilities"][operation]:
        raise ProviderError("当前后端不支持此操作", 400)
    ids = payload.get("ids")
    if ids is not None and (not isinstance(ids, list) or any(not isinstance(item, str) for item in ids)):
        raise ProviderError("ids 必须为账号 id 数组", 400)
    targets = [row for row in snapshot["accounts"] if row["region"] == snapshot["region"]]
    if ids is not None:
        valid = {row["id"] for row in targets}
        if set(ids) - valid:
            raise ProviderError("所选账号已变化或不在当前区域，请刷新后重试", 409)
        targets = [row for row in targets if row["id"] in ids]
    if not targets:
        raise ProviderError("当前区域没有可操作的账号", 400)
    results = []
    previous_call = None
    for row in targets:
        if operation == "checkin" and (not row.get("checkin", {}).get("supported")
                                       or provider == "qoder" and not row["enabled"]):
            results.append({"id": row["id"], "name": row["name"], "ok": False,
                            "skipped": True, "message": "账号不支持签到或已停用"})
            continue
        if previous_call is not None and operation in ("checkin", "credits", "credentials"):
            time.sleep(max(0, 1.1 - (time.monotonic() - previous_call)))
        try:
            action = "enable" if operation == "disable" else operation
            value = control.action(provider, action, {"id": row["id"], "enabled": operation == "enable"}, token)
            ok, message = outcome(value)
            results.append({"id": row["id"], "name": row["name"], "ok": ok, "message": message})
        except ProviderError as exc:
            results.append({"id": row["id"], "name": row["name"], "ok": False, "message": str(exc)})
        previous_call = time.monotonic()
    return {"ok": True, "results": results, "succeeded": sum(row["ok"] for row in results),
            "failed": sum(not row["ok"] and not row.get("skipped") for row in results),
            "skipped": sum(bool(row.get("skipped")) for row in results)}
