"""Shared HTTP entry point with WorkBuddy's existing authentication and limits."""
import json
import re
import secrets
import time
from http.cookies import SimpleCookie
from pathlib import Path
from urllib.parse import urlsplit

import wb_proxy
import wb_settings

from . import VERSION
from .activity import ObservedWriter
from .native import PROVIDERS as NATIVE_PROVIDERS
from .native.translate import anthropic_aggregate
from .protocols import chat_to_anthropic, minimax_chat_stream
from .transport import ProviderError

PROVIDERS = {"workbuddy", "trae", "minimax", "qoder"} | set(NATIVE_PROVIDERS)
PROVIDER_ROUTE = "(?:" + "|".join(sorted(PROVIDERS)) + ")"
PATHS = {"chat": "/v1/chat/completions", "responses": "/v1/responses", "messages": "/v1/messages"}


class GatewayHandler(wb_proxy.Handler):
    server_version = "MultiGateway/" + VERSION

    @property
    def runtime(self):
        return self.server.gateway

    def handle_one_request(self):
        self._gateway_group = None
        self._gateway_requested_model = None
        self._gateway_observer = None
        self._gateway_account = None
        self._gateway_http_status = None
        return super().handle_one_request()

    def send_response(self, code, message=None):
        self._gateway_http_status = code
        return super().send_response(code, message)

    def _panel_token(self):
        token = super()._panel_token()
        if token and wb_proxy.PANEL.valid(token):
            return token
        try:
            cookie = SimpleCookie(self.headers.get("Cookie") or "")
            value = cookie["gateway_session"].value if "gateway_session" in cookie else ""
            return value if wb_proxy.PANEL.valid(value) else token
        except Exception:
            return token

    def _cookie(self, token):
        if token and re.fullmatch(r"[A-Za-z0-9_\-]+", token):
            self.send_header("Set-Cookie", "gateway_session=" + token + "; HttpOnly; SameSite=Strict; Path=/")

    def _json(self, code, obj):
        if getattr(self, "_gateway_observer", None):
            self._gateway_observer.observe(obj)
        path = self.path.split("?")[0]
        if path in ("/panel/login", "/panel/password") and code == 200 and obj.get("token"):
            self._gateway_cookie = obj["token"]
        elif code == 200 and self.headers.get("X-Panel-Token") and self._panel_ok():
            self._gateway_cookie = self._panel_token()
        if path == "/panel/logout" and code == 200:
            self._gateway_clear_cookie = True
        return super()._json(code, obj)

    def end_headers(self):
        if getattr(self, "_gateway_clear_cookie", False):
            self.send_header("Set-Cookie", "gateway_session=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/")
            self._gateway_clear_cookie = False
        token = getattr(self, "_gateway_cookie", None)
        if token:
            self._cookie(token)
            self._gateway_cookie = None
        return super().end_headers()

    def _key_model_error(self, model):
        target = self._gateway_requested_model or model
        error = super()._key_model_error(target)
        if error and target.startswith("workbuddy/"):
            bare = target.removeprefix("workbuddy/")
            if not super()._key_model_error(bare):
                return ""
        return error

    def _get_dashboard(self):
        if urlsplit(self.path).path == "/workbuddy":
            return super()._get_dashboard()
        self._html_page(Path(__file__).with_name("dashboard.html"))

    def _html_page(self, path):
        body = path.read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        if self._panel_ok():
            self._cookie(self._panel_token())
        self.end_headers()
        self.wfile.write(body)

    def _get_health(self):
        return self._json(200, {"ok": True, "application": "MultiGateway", "version": VERSION,
                                "provider": self._gateway_group,
                                "providers": sorted(PROVIDERS), "api_key_required": wb_proxy.auth_required()})

    def _get_v1_models(self):
        if not self._authorized():
            return
        selected = self._gateway_group
        if selected:
            try:
                data = self.runtime.provider_models(selected)
                data = [row for row in data if not super(GatewayHandler, self)._key_model_error(selected + "/" + row["id"])
                        or selected == "workbuddy" and not super(GatewayHandler, self)._key_model_error(row["id"])]
                return self._json(200, {"object": "list", "data": data})
            except ProviderError as exc:
                return self._error(exc.status, str(exc), exc.code)
        data, errors = self.runtime.models(self._request_realm() or wb_proxy.CURRENT_REALM)
        data = [row for row in data if not self._key_model_error(row["id"])]
        return self._json(200, {"object": "list", "data": data, "provider_errors": errors})

    def do_GET(self):
        path = urlsplit(self.path).path
        asset = path.removeprefix("/gateway/ui/") if path.startswith("/gateway/ui/") else ""
        if asset in ("styles.css", "api.js", "render.js", "app.js", "accounts.js", "access.js", "icons.js",
                     "board.js") or re.fullmatch(r"icons/[A-Za-z0-9][A-Za-z0-9._-]*\.(svg|ico)", asset or ""):
            body = (Path(__file__).with_name("ui") / asset).read_bytes()
            self.send_response(200)
            kind = ("image/x-icon" if path.endswith(".ico") else "image/svg+xml" if path.endswith(".svg")
                    else "text/css" if path.endswith(".css") else "text/javascript")
            self.send_header("Content-Type", kind + "; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            return self.wfile.write(body)
        match = re.fullmatch(r"/gateway/providers/(" + PROVIDER_ROUTE + r")/(snapshot|models)", path)
        if match:
            if not self._panel_ok():
                return self._error(401, "panel password required")
            try:
                return self._json(200, self.runtime.control.read(*match.groups()))
            except ProviderError as exc:
                return self._error(exc.status, str(exc), exc.code)
        if path == "/gateway/usage/board":
            if not self._panel_ok():
                return self._error(401, "panel password required")
            return self._json(200, self.runtime.control.board())
        if path == "/favicon.ico":
            icon = Path(wb_proxy.DASHBOARD_HTML).with_name("app_icon.ico").read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "image/x-icon")
            self.send_header("Content-Length", str(len(icon)))
            self.end_headers()
            return self.wfile.write(icon)
        if path == "/workbuddy":
            return self._get_dashboard()
        if path == "/gateway/status":
            if not self._panel_ok():
                return self._error(401, "panel password required")
            return self._json(200, self.runtime.status())
        if path.startswith(("/trae/", "/qoder/")):
            return self._provider_panel(path)
        return super().do_GET()

    def do_POST(self):
        path = urlsplit(self.path).path
        if path.startswith("/updates/") and any(part in path for part in ("repack", "upgrade", "restart")):
            return self._error(409, "此版本需要通过聚合网关安装包更新")
        if path.startswith(("/trae/", "/qoder/")):
            return self._provider_panel(path)
        if path.startswith("/gateway/"):
            if not self._panel_ok():
                return self._error(401, "panel password required")
            payload = self._payload_or_error()
            if payload is None:
                return
            try:
                match = re.fullmatch(r"/gateway/providers/(" + PROVIDER_ROUTE + r")/(gateway|region|login|poll|enable|delete|credits|credentials|import|checkin|batch)", path)
                if match:
                    result = self.runtime.control.action(*match.groups(), payload, self._panel_token(),
                                                        wb_proxy.agents_client_allowed(self.client_address))
                    self.runtime.catalog["expires"] = 0
                elif path == "/gateway/configure":
                    result = self.runtime.control.action("workbuddy", "gateway", payload, self._panel_token())
                elif path == "/gateway/minimax/login":
                    result = self.runtime.minimax.start_login(payload.get("region", "cn"))
                elif path == "/gateway/minimax/poll":
                    result = self.runtime.minimax.poll_login(payload.get("login_id", ""))
                    if result.get("status") == "authenticated":
                        self.runtime.catalog["expires"] = 0
                elif path == "/gateway/minimax/region":
                    self.runtime.minimax.set_region(payload.get("region"))
                    self.runtime.catalog["expires"] = 0
                    result = self.runtime.minimax.status()
                elif path == "/gateway/trae/login":
                    if not wb_proxy.agents_client_allowed(self.client_address):
                        return self._error(403, "Trae browser login must start on the gateway computer")
                    result = self.runtime.trae.start_login("http://127.0.0.1:%s/trae" % self.server.server_port)
                elif path == "/gateway/api-keys/generate":
                    name = str(payload.get("name") or "").strip() or "API Key"
                    raw_key = "sk-mg-" + secrets.token_urlsafe(24)
                    entry_id = secrets.token_hex(6)
                    created_at = time.strftime("%Y/%m/%d %H:%M")
                    new_entry = {
                        "id": entry_id,
                        "name": name,
                        "key": raw_key,
                        "realm": "",
                        "models": [],
                        "enabled": True,
                        "created_at": created_at,
                    }
                    wb_settings.set_api_keys(wb_proxy.ACCOUNTS_DIR, [new_entry], delete_ids=[])
                    if not wb_proxy.API_KEY:
                        wb_proxy.API_KEY = raw_key
                        wb_proxy.API_KEY_FILE_SET = True
                    masked = raw_key[:6] + "*" * 8 + raw_key[-4:]
                    result = {
                        "ok": True,
                        "key": {
                            "id": entry_id,
                            "name": name,
                            "key": raw_key,
                            "masked": masked,
                            "created_at": created_at,
                            "enabled": True,
                        },
                    }
                elif path == "/gateway/api-keys/delete":
                    key_id = str(payload.get("id") or "").strip()
                    if not key_id:
                        return self._error(400, "missing key id")
                    wb_settings.set_api_keys(wb_proxy.ACCOUNTS_DIR, [], delete_ids=[key_id])
                    result = {"ok": True}
                else:
                    return self._error(404, "not found")
                return self._json(200, result)
            except ProviderError as exc:
                return self._error(exc.status, str(exc), exc.code)
        return super().do_POST()

    def _provider_panel(self, path):
        if not self._panel_ok():
            return self._error(401, "panel password required")
        name, route = path.lstrip("/").split("/", 1)
        if name == "trae" and not route:
            route = "web/login"
        query = urlsplit(self.path).query
        route = "/" + route + ("?" + query if query else "")
        body = None
        if self.command == "POST":
            payload = self._payload_or_error()
            if payload is None:
                return
            body = json.dumps(payload).encode("utf-8")
            self.runtime.catalog["expires"] = 0
            getattr(self.runtime, name).catalog = None
        headers = {"Content-Type": self.headers.get("Content-Type", "application/json")}
        try:
            with getattr(self.runtime, name).open(route, body, headers, self.command, timeout=60) as response:
                return self._relay(response, name, panel=True)
        except ProviderError as exc:
            return self._error(exc.status, str(exc), exc.code)

    def _provider(self, payload):
        model = payload.get("model") or ""
        prefix, separator, actual = str(model).partition("/")
        selected = self._gateway_group
        if selected and prefix in PROVIDERS and separator and selected != prefix:
            raise ProviderError("该分组只服务 " + selected, 400, "invalid_request_error")
        if prefix in PROVIDERS and separator:
            if not actual:
                raise ProviderError("model name is required after provider prefix", 400)
            self._gateway_requested_model = model
            blocked = self._key_model_error(model)
            if blocked:
                raise ProviderError(blocked, 400, "invalid_request_error")
            return prefix, dict(payload, model=actual)
        if selected:
            self._gateway_requested_model = (str(model) if selected == "workbuddy"
                and not super()._key_model_error(model) else selected + "/" + str(model))
            blocked = self._key_model_error(model)
            if blocked:
                raise ProviderError(blocked, 400, "invalid_request_error")
        return selected or "workbuddy", payload

    def _tracked(self, provider, body, kind, callback):
        activity = getattr(self.runtime, "activity", None)
        if activity is None:
            return callback()
        session = wb_proxy.extract_session_key(self.headers, body)
        row = activity.begin(provider, body.get("model", ""), kind, self.server.server_port, session)
        writer = ObservedWriter(self.wfile, row)
        original_writer, self.wfile = self.wfile, writer
        self._gateway_observer = writer
        try:
            return callback()
        except ProviderError as exc:
            row["error"] = str(exc)[:300]
            return self._anthropic_error(exc.status, str(exc), exc.code) if kind == "messages" else self._error(exc.status, str(exc), exc.code)
        except Exception:
            row["error"] = "请求处理异常，详情见 gateway.log"
            self._gateway_http_status = self._gateway_http_status or 500
            raise
        finally:
            writer.complete()
            self.wfile = original_writer
            self._gateway_observer = None
            row["account"] = row.get("account") or self._gateway_account or ""
            activity.finish(row, self._gateway_http_status, writer.disconnected)

    def _dispatch_chat_post(self, path, payload):
        kind = "responses" if path.endswith("responses") else "chat"
        try:
            provider, body = self._provider(payload)
            if provider == "workbuddy":
                return self._tracked(provider, body, kind, lambda: super(GatewayHandler, self)._dispatch_chat_post(path, body))
            if path.endswith("completions") and not path.endswith("chat/completions"):
                raise ProviderError("use /v1/chat/completions for this provider", 400)
            return self._tracked(provider, body, kind, lambda: self._external(provider, body, kind, payload))
        except ProviderError as exc:
            return self._error(exc.status, str(exc), exc.code)

    def _handle_messages(self, payload):
        try:
            provider, body = self._provider(payload)
            if provider == "workbuddy":
                return self._tracked(provider, body, "messages", lambda: super(GatewayHandler, self)._handle_messages(body))
            return self._tracked(provider, body, "messages", lambda: self._external(provider, body, "messages", payload))
        except ProviderError as exc:
            return self._anthropic_error(exc.status, str(exc), exc.code)

    def _external(self, provider, body, kind, original):
        session = wb_proxy.extract_session_key(self.headers, original)
        native = getattr(self.runtime, "native", {})
        if provider in native:
            return self._native(provider, native[provider], body, kind, original, session)
        if provider == "minimax" and kind != "messages":
            return self._minimax_chat(body, kind, original, session)
        if provider == "minimax":
            response = self.runtime.minimax.open_messages(body, session)
        else:
            headers = {"Content-Type": "application/json"}
            if session:
                headers["X-Session-Id"] = session
            response = getattr(self.runtime, provider).open(PATHS[kind], json.dumps(body).encode("utf-8"), headers)
        with response:
            return self._relay(response, provider)

    def _native(self, provider, impl, body, kind, original, session):
        """Serve one request from an in-process OAuth provider."""
        if kind == "messages":
            frames = self._attributed(impl, impl.messages(body, session))
            if not body.get("stream"):
                return self._json(200, anthropic_aggregate(frames, body.get("model")))
            self._begin_stream()
            return self._write_frames(frames, anthropic=True)
        if kind == "responses" and body.get("previous_response_id"):
            raise ProviderError("previous_response_id is not supported for %s; send the full input"
                                % impl.label, 400)
        try:
            chat = wb_proxy.responses_to_chat(body) if kind == "responses" else body
        except (ValueError, TypeError) as exc:
            raise ProviderError("invalid Responses input", 400) from exc
        namespace_map = chat.pop("_namespace_map", None)
        public_model = original["model"]
        meta = {key: original[key] for key in ("tools", "tool_choice", "parallel_tool_calls") if key in original}
        custom_names = wb_proxy.custom_tool_names(original.get("tools"))
        events = self._attributed(impl, impl.chat(chat, session))
        if not original.get("stream"):
            result = wb_proxy.aggregate_stream(events, public_model, None)
            if kind == "responses":
                result = wb_proxy.chat_to_response(result, public_model, custom_names, meta, namespace_map)
            return self._json(200, result)
        holder = {"custom_names": custom_names, "request_meta": meta, "namespace_map": namespace_map}
        frames = (wb_proxy.stream_responses_events(events, public_model, holder)
                  if kind == "responses" else events)
        self._begin_stream()
        return self._write_frames(frames)

    def _attributed(self, impl, frames):
        """Tie streamed frames to the account the provider selected on this thread."""
        for frame in frames:
            account = impl.current_account()
            if account:
                self._gateway_account = account
            yield frame

    def _begin_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.close_connection = True

    def _write_frames(self, frames, anthropic=False):
        try:
            for frame in frames:
                self.wfile.write(frame)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            return
        except ProviderError as exc:
            prefix = "event: error\n" if anthropic else ""
            error = ({"type": "error", "error": {"type": exc.code, "message": str(exc)}}
                     if anthropic else {"error": {"type": exc.code, "message": str(exc)}})
            try:
                self.wfile.write((prefix + "data: " + json.dumps(error, ensure_ascii=False) + "\n\n").encode("utf-8"))
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                pass

    def _relay(self, response, provider, panel=False):
        if getattr(response, "gateway_account", ""):
            self._gateway_account = response.gateway_account
        content_type = response.headers.get("Content-Type", "application/json")
        if panel and "text/html" in content_type:
            text = response.read().decode("utf-8")
            paths = r"([\"'`])/(?=(?:api|web|v1|accounts|usage|panel|settings|realm|scheduler|tasks|logs|models|favicon|authorize|healthz?|proxy|updates?|agents|pricing|diag)(?:[/?.\"'`]|$))"
            text = re.sub(paths, lambda match: match[1] + "/" + provider + "/", text)
            text = text.replace('href="/"', 'href="/' + provider + '/"')
            body = text.encode("utf-8")
            self.send_response(response.status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(response.status)
        self.send_header("Content-Type", content_type)
        self.send_header("X-Gateway-Provider", provider)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Connection", "close")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.close_connection = True
        try:
            while True:
                chunk = response.read1(65536)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass

    def _minimax_chat(self, body, kind, original, session):
        if kind == "responses" and body.get("previous_response_id"):
            raise ProviderError("previous_response_id is not supported for MiniMax; send the full input", 400)
        try:
            chat = wb_proxy.responses_to_chat(body) if kind == "responses" else body
        except (ValueError, TypeError) as exc:
            raise ProviderError("invalid Responses input", 400) from exc
        namespace_map = chat.pop("_namespace_map", None)
        upstream_payload = chat_to_anthropic(chat)
        public_model = original["model"]
        meta = {key: original[key] for key in ("tools", "tool_choice", "parallel_tool_calls") if key in original}
        custom_names = wb_proxy.custom_tool_names(original.get("tools"))
        with self.runtime.minimax.open_messages(upstream_payload, session) as response:
            events = minimax_chat_stream(response, public_model)
            if not original.get("stream"):
                result = wb_proxy.aggregate_stream(events, public_model, None)
                if kind == "responses":
                    result = wb_proxy.chat_to_response(result, public_model, custom_names, meta, namespace_map)
                return self._json(200, result)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream; charset=utf-8")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.close_connection = True
            holder = {"custom_names": custom_names, "request_meta": meta, "namespace_map": namespace_map}
            frames = wb_proxy.stream_responses_events(events, public_model, holder) if kind == "responses" else events
            try:
                for frame in frames:
                    self.wfile.write(frame)
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                return
            except ProviderError as exc:
                error = {"error": {"type": exc.code, "message": str(exc)}}
                prefix = "event: error\n" if kind == "responses" else ""
                self.wfile.write((prefix + "data: " + json.dumps(error) + "\n\n").encode("utf-8"))
                self.wfile.flush()
