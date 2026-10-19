"""Shared numeric and summary shape for upstream quota buckets."""
import math

from ..transport import ProviderError


def quota_number(value):
    if value is None or isinstance(value, bool):
        return None
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) else None


def quota_summary(buckets, label=None):
    if not buckets:
        raise ProviderError("上游未返回可用的额度数据", 502)
    primary = buckets[0]
    return {"remaining": primary["remaining"], "unit": primary["unit"],
            "label": label or primary["label"], "reset_at": primary.get("reset_at"),
            "buckets": buckets}
