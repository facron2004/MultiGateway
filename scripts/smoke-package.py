#!/usr/bin/env python3
"""Run the packaged gateway with empty, isolated data in both runtime modes."""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def smoke(binary, version, headless):
    with tempfile.TemporaryDirectory(prefix="multigateway-smoke-", ignore_cleanup_errors=True) as directory:
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        command = [str(binary), "--host", "127.0.0.1", "--port", str(port),
                   "--data-dir", directory, "--api-key", "package-smoke-key",
                   "--headless" if headless else "--silent"]
        checks = 0

        def request(path, expected=200, headers=None):
            nonlocal checks
            req = urllib.request.Request(base + path, headers=headers or {})
            try:
                response = urllib.request.urlopen(req, timeout=10)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                assert response.status == expected, (path, response.status, expected)
                body, response_headers = response.read(), response.headers
            checks += 1
            return body, response_headers

        log_path = pathlib.Path(directory) / "process.log"
        with log_path.open("wb") as log:
            process = subprocess.Popen(command, cwd=directory, stdout=log, stderr=log)
            try:
                for attempt in range(120):
                    assert process.poll() is None, log_path.read_text(errors="replace")
                    try:
                        body, _ = request("/health")
                        assert json.loads(body)["version"] == version
                        break
                    except (OSError, TimeoutError):
                        time.sleep(0.25)
                else:
                    raise AssertionError("Packaged gateway did not become ready")
                body, _ = request("/")
                assert f"v{version}".encode() in body
                assert b"./app.js" in body
                for path in ["/app.js", "/native-api.js", "/desktop.js", "/styles.css",
                             "/gateway/ui/app.js", "/gateway/status", "/gateway/usage/board",
                             "/settings", "/gateway/api-keys/list"]:
                    request(path)
                for provider in ["workbuddy", "trae", "minimax", "qoder", "antigravity", "grok", "copilot"]:
                    body, _ = request(f"/gateway/providers/{provider}/snapshot")
                    assert json.loads(body)["accounts"] == [], provider
                _, headers = request("/favicon.ico")
                request("/favicon.ico", 304, {"If-None-Match": headers["ETag"]})
                request("/v1/models", 401, {"Authorization": "Bearer wrong-package-key"})
                # Management routes reject arbitrary web origins, so this must
                # fail closed rather than answer with permissive CORS headers.
                _, headers = request(
                    "/gateway/status", 403, headers={"Origin": "https://example.com"}
                )
                assert headers.get("Access-Control-Allow-Origin") is None
                duplicate = subprocess.run(command, capture_output=True, timeout=20)
                # A second launch now defers to the running instance and exits
                # cleanly after asking it to reveal the window; the original
                # must still own the port.
                assert duplicate.returncode == 0, (
                    "duplicate instance did not defer to the running gateway",
                    duplicate.returncode,
                    duplicate.stderr.decode(errors="replace"),
                )
                request("/health")
                # Health starts before Tauri creates its window; allow setup to finish.
                time.sleep(3)
                assert process.poll() is None, log_path.read_text(errors="replace")
                print(json.dumps({"mode": "headless" if headless else "desktop",
                                  "checks": checks, "version": version, "duplicateRejected": True}))
            finally:
                if sys.platform == "win32" and process.poll() is None:
                    subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                                   capture_output=True, timeout=15)
                elif process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=8)


if __name__ == "__main__":
    executable = pathlib.Path(sys.argv[1]).resolve(strict=True)
    for headless in (True, False):
        smoke(executable, sys.argv[2], headless)
