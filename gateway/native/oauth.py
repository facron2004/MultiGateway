"""OAuth building blocks for the native (in-gateway) provider integrations.

Two login shapes are supported:

* ``LoopbackCallback`` — the browser is sent to the vendor's consent page and
  redirected back to a short-lived listener on this machine, which captures the
  authorization code. This is the flow Google/xAI clients use with PKCE.
* ``DeviceAuthorization`` — the vendor returns a user code that is typed into
  the consent page, and the gateway polls the token endpoint. This is the flow
  GitHub Copilot and Qwen use.

Nothing here knows about a specific vendor; providers supply endpoints.
"""
import base64
import hashlib
import html
import json
import secrets
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlsplit

from ..transport import ProviderError, json_request

CALLBACK_TEMPLATE = """<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>{title}</title>
<style>body{{font-family:system-ui,-apple-system,"Segoe UI",sans-serif;background:#0f1115;color:#e6e8ef;
display:flex;align-items:center;justify-content:center;height:100vh;margin:0}}
main{{text-align:center;max-width:32rem;padding:2rem}}h1{{font-size:1.25rem;font-weight:600}}
p{{color:#a5adba;line-height:1.6}}code{{background:#1b1f27;padding:.15rem .4rem;border-radius:.3rem}}</style>
</head><body><main><h1>{title}</h1><p>{detail}</p></main></body></html>"""


def pkce_pair(verifier_bytes=48):
    """Return a PKCE ``(verifier, challenge)`` pair using S256."""
    verifier = secrets.token_urlsafe(verifier_bytes)
    challenge = base64.urlsafe_b64encode(
        hashlib.sha256(verifier.encode("ascii")).digest()).decode("ascii").rstrip("=")
    return verifier, challenge


def random_state():
    return secrets.token_urlsafe(24)


def new_login_id():
    return uuid.uuid4().hex


def add_query(url, params):
    """Append query parameters, skipping ``None`` values."""
    items = [(key, value) for key, value in params.items() if value is not None]
    if not items:
        return url
    separator = "&" if urlsplit(url).query else "?"
    return url + separator + urlencode(items)


def google_device_login(device_endpoint, client_id, scope, audience=None, extra=None, timeout=30):
    """Start a standard RFC 8628 device authorization."""
    payload = {"client_id": client_id, "scope": scope}
    if audience:
        payload["audience"] = audience
    payload.update(extra or {})
    grant = json_request(device_endpoint, payload, form=True, timeout=timeout)
    device = grant.get("device_code")
    user_code = grant.get("user_code")
    url = (grant.get("verification_uri_complete") or grant.get("verification_uri")
           or grant.get("verification_url"))
    expires_in = float(grant.get("expires_in") or grant.get("expires_in_seconds") or 0)
    if not device or not user_code or not url or expires_in <= 0:
        raise ProviderError("OAuth 设备授权响应不完整，请稍后再试")
    interval = max(1.0, float(grant.get("interval") or 5))
    return {"device_code": device, "user_code": user_code, "url": url,
            "interval": interval, "expires_at": time.time() + expires_in}


def device_token_request(token_endpoint, client_id, device_code, client_secret=None):
    payload = {"client_id": client_id, "device_code": device_code,
               "grant_type": "urn:ietf:params:oauth:grant-type:device_code"}
    if client_secret:
        payload["client_secret"] = client_secret
    return json_request(token_endpoint, payload, form=True)


def refresh_request(token_endpoint, payload, form=True, headers=None, timeout=30):
    return json_request(token_endpoint, payload, headers=headers, form=form, timeout=timeout)


def expires_at_of(grant, default=3600):
    try:
        return time.time() + float(grant.get("expires_in") or default)
    except (TypeError, ValueError):
        return time.time() + default


class LoopbackCallback:
    """One-shot loopback listener that captures an OAuth redirect.

    The vendor's registered redirect URI is a loopback URL with a fixed port and
    path; we bind the port only while a login is in flight. Any query string is
    accepted so providers that append their own parameters still work. Both
    IPv4 and IPv6 loopback are bound when the platform allows it, because
    browsers resolve ``localhost`` to either one.
    """

    def __init__(self, port, redirect_uri, host="127.0.0.1", timeout=300):
        self.port = int(port)
        self.redirect_uri = redirect_uri
        self.host = host
        self.timeout = timeout
        self.result = None
        self.error = ""
        self.event = threading.Event()
        self.servers = []
        self.started_at = 0.0

    def _handler(self):
        callback = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def _reply(self, status, title, detail):
                body = CALLBACK_TEMPLATE.format(title=html.escape(title),
                                                detail=detail).encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                query = parse_qs(urlsplit(self.path).query)
                values = {key: items[0] for key, items in query.items() if items}
                if values.get("error"):
                    callback.error = values.get("error_description") or values["error"]
                    callback.event.set()
                    return self._reply(400, "授权未完成", html.escape(callback.error))
                if not values.get("code"):
                    return self._reply(404, "等待授权", "请在浏览器完成授权后返回本页面。")
                callback.result = values
                callback.event.set()
                return self._reply(200, "授权成功", "可以关闭此页面，返回 MultiGateway。")

            do_POST = do_GET

        return Handler

    def start(self):
        import socket

        class IPv6Server(ThreadingHTTPServer):
            address_family = socket.AF_INET6

        handler = self._handler()
        errors = []
        for server_class, host in ((ThreadingHTTPServer, "127.0.0.1"),
                                   (IPv6Server, "::1")):
            try:
                server = server_class((host, self.port), handler)
            except OSError as exc:
                errors.append(str(exc))
                continue
            server.daemon_threads = True
            self.servers.append(server)
            threading.Thread(target=server.serve_forever, daemon=True).start()
        if not self.servers:
            raise ProviderError("授权回调端口 %s 被占用，请关闭占用该端口的程序后重试"
                                % self.port, 409)
        self.started_at = time.time()
        return self

    def wait(self, timeout=None):
        self.event.wait(self.timeout if timeout is None else timeout)
        if self.error:
            raise ProviderError(self.error, 400, "access_denied")
        return self.result

    def stop(self):
        for server in self.servers:
            try:
                server.shutdown()
            finally:
                server.server_close()
        self.servers = []
