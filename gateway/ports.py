"""One public API listener exposing every provider group."""
import json
import re
import threading
from http.server import ThreadingHTTPServer
from urllib.parse import urlsplit

from .minimax import atomic_json
from .native import NAMES as NATIVE_NAMES
from .transport import ProviderError

NAMES = ("workbuddy", "trae", "minimax", "qoder") + NATIVE_NAMES
GROUP_ROUTE = r"/(?:" + "|".join(NAMES) + r")(/.*)"


class Ports:
    def __init__(self, runtime, host, main_port):
        self.runtime, self.main_port = runtime, main_port
        self.path = runtime.directory / "ports.json"
        self.lock = threading.RLock()
        self.server, self.error, self.legacy = None, "", None
        self.config = {"host": host, "port": main_port + 1, "enabled": True}
        if self.path.exists():
            saved = json.loads(self.path.read_text(encoding="utf-8"))
            if "port" in saved:
                self.config.update({key: saved[key] for key in self.config if key in saved})
            else:
                self.legacy = saved
                self.config.update(saved.get("workbuddy", {}))

    def _listen(self, host, port):
        from .handler import GatewayHandler

        class GroupHandler(GatewayHandler):
            def _route_group(self, allowed):
                parsed = urlsplit(self.path)
                match = re.fullmatch(GROUP_ROUTE, parsed.path)
                path = match[1] if match else parsed.path
                if path not in allowed:
                    self._error(404, "use the management port for account operations")
                    return False
                if match:
                    self._gateway_group = parsed.path.lstrip("/").split("/", 1)[0]
                    self.path = path + ("?" + parsed.query if parsed.query else "")
                return True

            def do_GET(self):
                if self._route_group(("/health", "/v1/models", "/models")):
                    return super().do_GET()

            def do_POST(self):
                if self._route_group(("/v1/chat/completions", "/v1/responses", "/v1/messages", "/v1/messages/count_tokens")):
                    return super().do_POST()

            def do_OPTIONS(self):
                if not self._route_group(("/health", "/models", "/v1/models", "/v1/chat/completions", "/v1/responses", "/v1/messages", "/v1/messages/count_tokens")):
                    return
                self.send_response(204)
                self.send_header("Access-Control-Allow-Origin", "*")
                self.send_header("Access-Control-Allow-Headers", "Authorization, *")
                self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
                self.send_header("Content-Length", "0")
                self.end_headers()

        try:
            server = ThreadingHTTPServer((host, port), GroupHandler)
        except OSError as exc:
            raise ProviderError("监听地址或端口不可用，请检查地址与占用情况", 409) from exc
        server.daemon_threads = True
        server.gateway = self.runtime
        threading.Thread(target=server.serve_forever, daemon=True).start()
        return server

    def start(self):
        try:
            self.configure("workbuddy", **self.config, persist=False)
        except ProviderError as exc:
            self.error = str(exc)

    def configure(self, name, port, enabled, persist=True, host=None):
        if isinstance(port, bool) or not isinstance(port, int) or not 1 <= port <= 65535:
            raise ProviderError("端口必须为 1 到 65535 的整数", 400)
        if not isinstance(enabled, bool):
            raise ProviderError("enabled must be a boolean", 400)
        with self.lock:
            host = self.config["host"] if host is None else host
            if not isinstance(host, str) or not host.strip():
                raise ProviderError("监听地址不能为空", 400)
            host = host.strip()
            if port == self.main_port:
                raise ProviderError("该端口已用于管理界面", 409)
            old = self.server
            server = old if old and port == self.config["port"] and host == self.config["host"] and enabled else None
            if enabled and server is None:
                server = self._listen(host, port)
            if old and old is not server:
                old.shutdown()
                old.server_close()
            self.server = server
            self.config = {"host": host, "port": port, "enabled": enabled}
            self.error = ""
            if persist:
                backup = self.path.with_name("ports.v0.3.backup.json")
                if self.legacy is not None and not backup.exists():
                    atomic_json(backup, self.legacy)
                atomic_json(self.path, self.config)
                self.legacy = None
            return self.info()

    def info(self, name=None):
        with self.lock:
            return {**self.config, "running": self.server is not None, "error": self.error,
                    "groups": list(NAMES)}

    def stop(self):
        with self.lock:
            if self.server:
                self.server.shutdown()
                self.server.server_close()
                self.server = None
