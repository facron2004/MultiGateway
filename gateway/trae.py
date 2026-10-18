"""Run the pinned Trae backend in-process on a private loopback listener."""
import os
import secrets
import threading
import time
from pathlib import Path

from .transport import ProviderError, open_request


class Trae:
    def __init__(self, directory):
        self.directory = Path(directory)
        self.server = None
        self.port = None
        self.secret = secrets.token_urlsafe(32)
        self.error = ""
        self.catalog = None
        self.helper = None

    def start(self):
        import socket
        import uvicorn
        self.directory.mkdir(parents=True, exist_ok=True)
        os.environ.setdefault("TRAE_AUTH_SOURCE", "web-login")
        os.environ["TRAE_USAGE_RECORDS_PATH"] = str(self.directory / "usage_records.json")
        os.environ["TRAE_USAGE_STATS_PATH"] = str(self.directory / "usage_stats.json")
        from .vendor.trae.src import auth, main
        auth.APP_DIR = self.directory
        auth.ENV_PATH = self.directory / ".env"
        auth.ACCOUNTS_PATH = self.directory / "accounts.json"
        main.API_KEYS = [self.secret]
        main.PUBLIC_PATHS = {"/healthz", "/authorize"}
        main.PUBLIC_PATH_PREFIXES = ()
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.bind(("127.0.0.1", 0))
        self.port = listener.getsockname()[1]
        main.PORT = self.port
        listener.listen(128)
        config = uvicorn.Config(main.app, host="127.0.0.1", port=self.port,
                                log_level="warning", access_log=False, loop="asyncio", ws="none")
        self.server = uvicorn.Server(config)
        thread = threading.Thread(target=self.server.run, kwargs={"sockets": [listener]}, daemon=True)
        thread.start()
        deadline = time.monotonic() + 20
        while not self.server.started:
            if not thread.is_alive() or time.monotonic() > deadline:
                raise ProviderError("Trae backend failed to start; inspect gateway.log", 503)
            time.sleep(0.05)

    def stop(self):
        if self.server:
            self.server.should_exit = True
        if self.helper:
            self.helper.shutdown()
            self.helper.server_close()

    def start_login(self, gateway_url):
        from http.server import ThreadingHTTPServer
        from .vendor.trae import web_login
        if not self.server or not self.server.started:
            raise ProviderError("Trae backend is not ready", 503)
        if not self.helper:
            def forward_credentials(relay_url, credentials):
                import json
                try:
                    body = json.dumps(credentials).encode("utf-8")
                    with self.open("/api/web-auth", body, {"Content-Type": "application/json"}) as response:
                        result = json.load(response)
                    return bool(result.get("success")), "凭证已写入网关" if result.get("success") else "授权保存失败"
                except ProviderError:
                    return False, "授权保存失败，请重试"
            web_login.forward_to_relay = forward_credentials
            handler = type("GatewayTraeAuth", (web_login.RelayAuthHandler,), {
                "relay_url": gateway_url,
            })
            try:
                self.helper = ThreadingHTTPServer(("127.0.0.1", 8765), handler)
            except OSError as exc:
                raise ProviderError("Trae login callback port 8765 is occupied; close the previous authorization helper", 409) from exc
            threading.Thread(target=self.helper.serve_forever, daemon=True).start()
        return {"url": "http://127.0.0.1:8765/"}

    def open(self, path, body=None, headers=None, method=None, timeout=600):
        if not self.server or not self.server.started:
            raise ProviderError(self.error or "Trae backend is not running", 503)
        outgoing = {"Authorization": "Bearer " + self.secret}
        outgoing.update(headers or {})
        return open_request("http://127.0.0.1:%s%s" % (self.port, path), body,
                            outgoing, method=method, timeout=timeout)

    def refresh_account(self, account_id):
        if not self.server or not self.server.started:
            raise ProviderError(self.error or "Trae 后端正在启动", 503)
        import asyncio
        from .vendor.trae.src import auth
        ok = asyncio.run(auth.refresh_account(account_id))
        self.catalog = None
        return {"ok": ok, "message": "凭证已刷新" if ok else "凭证刷新失败，请重新授权或查看日志"}

    def models(self):
        if self.catalog and self.catalog[0] > time.monotonic():
            return self.catalog[1]
        with self.open("/v1/models", timeout=10) as response:
            import json
            models = json.load(response).get("data") or []
        self.catalog = (time.monotonic() + 60, models)
        return models

    def status(self):
        if not self.server or not self.server.started:
            return {"running": False, "error": self.error}
        try:
            with self.open("/api/accounts", timeout=10) as response:
                import json
                result = json.load(response)
            accounts = result.get("accounts") or []
            return {"running": True, "accounts": len(accounts)}
        except ProviderError:
            return {"running": True, "accounts": None}
