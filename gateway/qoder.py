"""Private Qoder listener with the native account dashboard and protocols."""
import hmac
import json
import secrets
import threading
import time
from http.server import ThreadingHTTPServer
from pathlib import Path

from .transport import ProviderError, open_request


class Qoder:
    def __init__(self, directory):
        self.directory = Path(directory)
        self.server = None
        self.secret = secrets.token_urlsafe(32)
        self.error = ""
        self.catalog = None

    def start(self):
        from .vendor.qoder import qoder_proxy as native
        accounts, usage = self.directory / "accounts", self.directory / "usage"
        accounts.mkdir(parents=True, exist_ok=True)
        usage.mkdir(parents=True, exist_ok=True)
        native.ACCOUNTS_DIR = str(accounts)
        native.USAGE_DIR = str(usage)
        native.USAGE_LOG = str(usage / "usage.jsonl")
        native.USAGE_SUMMARY = str(usage / "usage-summary.json")
        native.qoder_accounts.USAGE_LOG_OVERRIDE = native.USAGE_LOG
        native.API_KEY = self.secret
        native.POOL = native.qoder_accounts.AccountPool(str(accounts), log=native.log)
        native.POOL.load()
        native.load_persisted_realm()
        native.POOL.apply_settings(limits_data=native.qoder_settings.limits_data(str(accounts)),
                                   free_models=native.free_model_names(native.CURRENT_REALM))
        self.native = native
        secret = self.secret

        class PrivateHandler(native.Handler):
            def _panel_ok(self):
                return hmac.compare_digest(self.headers.get("Authorization", ""), "Bearer " + secret)

            def _key_ok(self):
                return self._panel_ok()

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), PrivateHandler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def stop(self):
        if self.server:
            self.server.shutdown()
            self.server.server_close()

    def open(self, path, body=None, headers=None, method=None, timeout=600):
        if not self.server:
            raise ProviderError(self.error or "Qoder backend is not running", 503)
        outgoing = {"Authorization": "Bearer " + self.secret}
        outgoing.update(headers or {})
        url = "http://127.0.0.1:%s%s" % (self.server.server_port, path)
        return open_request(url, body, outgoing, method=method, timeout=timeout)

    def models(self):
        if self.catalog and self.catalog[0] > time.monotonic():
            return self.catalog[1]
        with self.open("/v1/models", timeout=10) as response:
            models = json.load(response).get("data") or []
        self.catalog = (time.monotonic() + 60, models)
        return models

    def status(self):
        if not self.server:
            return {"running": False, "error": self.error}
        with self.open("/health", timeout=3) as response:
            result = json.load(response)
        return {"running": True, "accounts": result.get("accounts", 0), "realm": result.get("realm")}
