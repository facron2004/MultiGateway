"""Small HTTP helpers shared by the account integrations."""
import json
import urllib.error
import urllib.parse
import urllib.request


class ProviderError(Exception):
    def __init__(self, message, status=502, code="provider_error"):
        super().__init__(message)
        self.status = status
        self.code = code


def open_request(url, body=None, headers=None, method=None, timeout=60):
    request = urllib.request.Request(url, data=body, headers=headers or {}, method=method)
    try:
        return urllib.request.urlopen(request, timeout=timeout)
    except urllib.error.HTTPError as exc:
        detail = exc.read(65536).decode("utf-8", "replace")
        try:
            data = json.loads(detail)
            error = data.get("error") or data
            code = error if isinstance(error, str) else error.get("type", "provider_error")
            message = data.get("error_description") or (
                error.get("message") if isinstance(error, dict) else error)
        except (ValueError, AttributeError):
            code, message = "provider_error", "upstream HTTP %s" % exc.code
        raise ProviderError(message or "upstream HTTP %s" % exc.code, exc.code, code) from exc
    except (OSError, urllib.error.URLError) as exc:
        raise ProviderError("upstream connection failed", 502) from exc


def json_request(url, data=None, headers=None, form=False, timeout=30):
    headers = dict(headers or {})
    body = None
    if data is not None:
        if form:
            body = urllib.parse.urlencode(data).encode("ascii")
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        else:
            body = json.dumps(data, ensure_ascii=False).encode("utf-8")
            headers["Content-Type"] = "application/json"
    with open_request(url, body, headers, timeout=timeout) as response:
        try:
            return json.loads(response.read(4 * 1024 * 1024))
        except ValueError as exc:
            raise ProviderError("upstream returned invalid JSON") from exc


def sse_events(response):
    """Parse complete SSE events, including multi-line data and CRLF."""
    lines = []
    for raw in response:
        line = raw.decode("utf-8", "replace").rstrip("\r\n")
        if line.startswith("data:"):
            lines.append(line[5:].lstrip(" "))
        elif not line and lines:
            data = "\n".join(lines)
            lines = []
            if data == "[DONE]":
                return
            try:
                yield json.loads(data)
            except ValueError as exc:
                raise ProviderError("upstream returned invalid SSE data") from exc
    if lines:
        yield json.loads("\n".join(lines))
