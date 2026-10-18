"""MultiGateway desktop executable and headless HTTP launcher."""
import argparse
import logging
import os
import shutil
import sys
import threading
import traceback
from http.server import ThreadingHTTPServer
from pathlib import Path


class WindowedLog:
    """Windowed executables have no console; keep request logging writable."""
    encoding = "utf-8"

    def __init__(self, path):
        self.path = path

    def write(self, value):
        with open(self.path, "a", encoding="utf-8", errors="replace") as handle:
            return handle.write(value)

    def flush(self):
        pass

    def reconfigure(self, **kwargs):
        pass

    def isatty(self):
        return False


def arguments():
    parser = argparse.ArgumentParser(description="WorkBuddy / Trae / MiniMax Code / Qoder / Antigravity / Grok / GitHub Copilot / Qwen Code account gateway")
    parser.add_argument("--headless", action="store_true", help="serve the API without opening the desktop window")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8791)
    parser.add_argument("--data-dir", help="isolated credentials, settings and usage directory")
    parser.add_argument("--api-key", help="gateway API key; existing dashboard keys take precedence")
    parser.add_argument("--panel-password", help="set the shared dashboard password")
    return parser.parse_args()


def prepare_paths(args):
    base = Path(sys.executable).parent if getattr(sys, "frozen", False) else Path(__file__).parent
    resources = Path(getattr(sys, "_MEIPASS", base))
    directory = Path(args.data_dir).resolve() if args.data_dir else base / "gateway-data"
    directory.mkdir(parents=True, exist_ok=True)
    accounts = directory / "workbuddy" / "accounts"
    # First launch imports the user's current installation without changing it.
    if not args.data_dir and not accounts.exists() and (base / "accounts").is_dir():
        shutil.copytree(base / "accounts", accounts)
    accounts.mkdir(parents=True, exist_ok=True)
    usage = directory / "workbuddy" / "usage"
    usage.mkdir(parents=True, exist_ok=True)
    os.environ["WB_ACCOUNTS_DIR"] = str(accounts)
    os.environ["WB_PROXY_USAGE_DIR"] = str(usage)
    dashboard = base / "dashboard.html"
    os.environ["WB_DASHBOARD_HTML"] = str(dashboard if dashboard.exists() else resources / "dashboard.html")
    pricing = base / "pricing" / "pricing.json"
    os.environ["WB_PRICING_FILE"] = str(pricing if pricing.exists() else resources / "pricing" / "pricing.json")
    if getattr(sys, "frozen", False) or sys.stdout is None or sys.stderr is None:
        sys.stdout = WindowedLog(directory / "gateway.log")
        sys.stderr = WindowedLog(directory / "gateway.log")
    logging.basicConfig(filename=str(directory / "gateway.log"), encoding="utf-8", level=logging.WARNING, force=True)
    return directory, accounts, usage


class DesktopAPI:
    def __init__(self, directory):
        self._directory = directory
        self._window = None
        self._maximized = False

    def set_window(self, window):
        self._window = window
        try:
            window.events.maximized += self._on_maximized
            window.events.restored += self._on_restored
        except Exception:
            pass

    def _on_maximized(self):
        self._maximized = True

    def _on_restored(self):
        self._maximized = False

    def window_minimize(self):
        if self._window:
            try:
                self._window.minimize()
                return True
            except Exception:
                pass
        return False

    def window_maximize(self):
        if self._window:
            try:
                if self._maximized:
                    self._window.restore()
                    self._maximized = False
                else:
                    self._window.maximize()
                    self._maximized = True
                return True
            except Exception:
                pass
        return False

    def window_close(self):
        if self._window:
            try:
                self._window.destroy()
                return True
            except Exception:
                pass
        return False

    def open_folder(self):
        os.startfile(str(self._directory))

    def open_external(self, url):
        import webbrowser
        if str(url).startswith(("https://", "http://")):
            return webbrowser.open(url)
        return False


def main():
    args = arguments()
    directory, accounts, usage = prepare_paths(args)
    import wb_proxy
    from gateway.handler import GatewayHandler
    from gateway.runtime import Runtime

    class LoggedServer(ThreadingHTTPServer):
        def handle_error(self, request, client_address):
            with open(directory / "gateway.log", "a", encoding="utf-8") as handle:
                traceback.print_exc(file=handle)

    server = LoggedServer((args.host, args.port), GatewayHandler)
    server.daemon_threads = True
    options = argparse.Namespace(
        host=args.host, port=args.port, lan=args.host not in ("127.0.0.1", "localhost", "::1"),
        api_key=args.api_key, system_prompt=wb_proxy.DEFAULT_SYSTEM_PROMPT, user_agent=None,
        usage_dir=str(usage), accounts_dir=str(accounts), import_desktop=False,
        panel_password=args.panel_password, info=None,
    )
    wb_proxy._apply_cli_overrides(options)
    wb_proxy._bootstrap_runtime(options)
    server.gateway = Runtime(directory)
    server.gateway.start()
    server.gateway.start_ports(args.host, args.port)
    try:
        if args.headless:
            token = wb_proxy.PANEL.create()
            print("MultiGateway Web Dashboard: http://127.0.0.1:%s/#token=%s" % (args.port, token), flush=True)
            print("MultiGateway API: http://127.0.0.1:%s/v1" % args.port, flush=True)
            server.serve_forever()
        else:
            import webview
            from gateway import VERSION
            threading.Thread(target=server.serve_forever, daemon=True).start()
            token = wb_proxy.PANEL.create()
            url = "http://127.0.0.1:%s/#token=%s" % (args.port, token)
            webview.settings["OPEN_EXTERNAL_LINKS_IN_BROWSER"] = True
            api = DesktopAPI(directory)
            window = webview.create_window(
                "MultiGateway · " + VERSION,
                url,
                width=980,
                height=640,
                min_size=(700, 480),
                frameless=True,
                easy_drag=False,
                js_api=api
            )
            api.set_window(window)
            webview.start()
            server.shutdown()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        server.gateway.stop()
        if wb_proxy.SCHEDULER:
            wb_proxy.SCHEDULER.stop()


if __name__ == "__main__":
    main()
