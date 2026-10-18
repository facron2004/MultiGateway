"""Provider lifecycle and model catalog; the original WorkBuddy engine stays intact."""
import concurrent.futures
import logging
import threading
import time
from pathlib import Path

import wb_proxy

from .minimax import MiniMax
from .activity import Activity
from .control import Control
from .native import build as build_native
from .ports import Ports
from .qoder import Qoder
from .trae import Trae


class Runtime:
    def __init__(self, directory):
        self.directory = Path(directory)
        self.minimax = MiniMax(self.directory / "minimax")
        self.trae = Trae(self.directory / "trae")
        self.qoder = Qoder(self.directory / "qoder")
        self.native = build_native(self.directory / "native")
        for name, provider in self.native.items():
            setattr(self, name, provider)
        self.errors = {}
        self.catalog = {}
        self.catalog_lock = threading.Lock()
        self.activity = Activity(self.directory)
        self.ports = None
        self.control = Control(self)

    def start_ports(self, host, port):
        self.origin = "http://127.0.0.1:%s" % port
        self.ports = Ports(self, host, port)
        self.ports.start()

    def start(self):
        def start_one(name, provider):
            try:
                provider.start()
            except Exception as exc:
                provider.error = "%s backend failed to start (%s); inspect gateway.log" % (name, type(exc).__name__)
                wb_proxy.log(provider.error, level="ERROR", tag="gateway")
                logging.exception("%s backend failed to start", name)
        for name in ("trae", "qoder"):
            threading.Thread(target=start_one, args=(name, getattr(self, name)), daemon=True).start()
        for name, provider in self.native.items():
            try:
                provider.start()
            except Exception:
                logging.exception("%s provider failed to start", name)

    def stop(self):
        if self.ports:
            self.ports.stop()
        self.trae.stop()
        self.qoder.stop()
        for provider in self.native.values():
            try:
                provider.stop()
            except Exception:
                logging.exception("%s provider failed to stop", provider.name)

    def status(self):
        return {
            "workbuddy": {"running": True, "accounts": len(wb_proxy.POOL.accounts) if wb_proxy.POOL else 0},
            "trae": self.trae.status(), "qoder": self.qoder.status(),
            "minimax": self.minimax.status(),
            **{name: provider.status() for name, provider in self.native.items()},
        }

    def provider_models(self, name):
        if name == "workbuddy":
            rows = []
            for mid, metadata in wb_proxy.fetch_models(realm=wb_proxy.CURRENT_REALM):
                entry = wb_proxy.model_entry(mid, metadata)
                if metadata.get("credits") is not None:
                    entry["credits"] = metadata["credits"]
                rows.append(entry)
        else:
            rows = getattr(self, name).models()
        return [dict(row) if isinstance(row, dict) else {"id": row, "object": "model"} for row in rows]

    def models(self, realm):
        with self.catalog_lock:
            if self.catalog.get("expires", 0) > time.monotonic() and self.catalog.get("realm") == realm:
                return self.catalog["data"], self.errors.copy()
            def workbuddy_models():
                return [wb_proxy.model_entry(mid, metadata) for mid, metadata in wb_proxy.fetch_models(realm=realm)]
            sources = {"workbuddy": workbuddy_models, "trae": self.trae.models,
                       "qoder": self.qoder.models, "minimax": self.minimax.models}
            for name, provider in self.native.items():
                sources[name] = provider.models
            data = []
            with concurrent.futures.ThreadPoolExecutor(max_workers=len(sources)) as executor:
                pending = {name: executor.submit(function) for name, function in sources.items()}
                for name, future in pending.items():
                    try:
                        rows = future.result()
                        self.errors.pop(name, None)
                        self.catalog[name] = rows
                    except Exception:
                        self.errors[name] = name + " model catalog unavailable; the last successful catalog is retained"
                        rows = self.catalog.get(name, [])
                    for row in rows:
                        entry = dict(row) if isinstance(row, dict) else {"id": row, "object": "model"}
                        original = entry["id"]
                        entry.update(id=name + "/" + original, owned_by=name)
                        data.append(entry)
            self.catalog.update(expires=time.monotonic() + (5 if self.errors else 60), realm=realm, data=data)
            return data, self.errors.copy()
