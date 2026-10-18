"""HTTP integration tests against local providers; no real accounts or charges."""
import io
import json
import os
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
WORK = tempfile.TemporaryDirectory(prefix="multigateway-tests-")
os.environ["WB_ACCOUNTS_DIR"] = str(Path(WORK.name) / "accounts")
os.environ["WB_PROXY_USAGE_DIR"] = str(Path(WORK.name) / "usage")
Path(os.environ["WB_ACCOUNTS_DIR"]).mkdir()
Path(os.environ["WB_PROXY_USAGE_DIR"]).mkdir()

import wb_proxy as wb
import wb_settings
from gateway.handler import GatewayHandler
from gateway.activity import Activity
from gateway.control import Control, account_view
from gateway.ports import Ports
from gateway.minimax import MiniMax
from gateway.protocols import chat_to_anthropic, minimax_chat_stream
from gateway.transport import ProviderError, open_request


def anthropic_events(tools=False):
    events = [
        {"type": "message_start", "message": {"id": "msg-test", "usage": {
            "input_tokens": 3, "cache_read_input_tokens": 5, "cache_creation_input_tokens": 2}}},
        {"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}},
        {"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "plan"}},
        {"type": "content_block_stop", "index": 0},
    ]
    if tools:
        events += [
            {"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "call-1", "name": "weather", "input": {}}},
            {"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": '{"city":'}},
            {"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": '"上海"}'}},
        ]
    else:
        events += [{"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "你好"}}]
    events += [
        {"type": "message_delta", "delta": {"stop_reason": "tool_use" if tools else "end_turn"}, "usage": {"output_tokens": 4}},
        {"type": "message_stop"},
    ]
    return b"".join(("event: " + event["type"] + "\r\ndata: " + json.dumps(event, ensure_ascii=False) + "\r\n\r\n").encode("utf-8") for event in events)


class MockProvider(BaseHTTPRequestHandler):
    requests = []
    oauth_pending = False
    refresh_failure = False
    use_tools = False

    def log_message(self, *args):
        pass

    def reply(self, value, status=200, content_type="application/json"):
        body = value if isinstance(value, bytes) else json.dumps(value).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/api/accounts":
            return self.reply({"accounts": [{"id": "trae-only", "label": "Trae 测试账号",
                "is_valid": True, "model_enabled": True, "model_eligible": True,
                "account_credits": {"remaining": 12}, "token": "never-expose-credentials"}]})
        if self.path == "/api/usage/records":
            return self.reply([{"model": "trae-test", "account_id": "trae-only", "prompt_tokens": 2}])
        if self.path.startswith("/mavis/api/v1/models"):
            return self.reply({"providers": [{"providerId": "minimax", "config": {
                "model_order": ["MiniMax-Test"], "models": {"MiniMax-Test": {}}}}]})
        return self.reply(b'<html><script>fetch("/api/accounts"); fetch("/update/check"); fetch("/diag/vm"); const slash = "/";</script></html>', content_type="text/html")

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if self.path.startswith("/oauth2"):
            body = {key: value[0] for key, value in urllib.parse.parse_qs(raw.decode()).items()}
        else:
            body = json.loads(raw)
        type(self).requests.append((self.path, body, dict(self.headers)))
        if self.path == "/oauth2/device/code":
            return self.reply({"device_code": "device-secret", "user_code": "ABCD", "verification_uri": "https://example.invalid/authorize", "expires_in": 600, "interval": 1})
        if self.path == "/oauth2/token":
            if body.get("grant_type") == "refresh_token" and type(self).refresh_failure:
                return self.reply({"error": "invalid_grant"}, 400)
            if type(self).oauth_pending:
                return self.reply({"error": "authorization_pending"}, 400)
            return self.reply({"access_token": "mock-account-session", "refresh_token": "mock-refresh", "expires_in": 3600})
        if self.path.endswith("/llm/v1/messages"):
            if body.get("stream"):
                return self.reply(anthropic_events(type(self).use_tools), content_type="text/event-stream")
            return self.reply({"id": "msg-native", "type": "message", "role": "assistant", "model": body["model"],
                               "content": [{"type": "text", "text": "native"}], "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 4}})
        if body.get("stream"):
            return self.reply(b'data: {"choices":[{"delta":{"content":"hello"}}]}\n\ndata: [DONE]\n\n', content_type="text/event-stream")
        return self.reply({"model": body.get("model"), "path": self.path, "content": "provider"})


class LocalProvider:
    def __init__(self, origin):
        self.origin = origin

    def open(self, path, body=None, headers=None, method=None, timeout=60):
        return open_request(self.origin + path, body, headers, method=method, timeout=timeout)

    def status(self):
        return {"running": True, "accounts": 1}


class GatewayTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.upstream = ThreadingHTTPServer(("127.0.0.1", 0), MockProvider)
        cls.upstream.daemon_threads = True
        threading.Thread(target=cls.upstream.serve_forever, daemon=True).start()
        cls.origin = "http://127.0.0.1:%s" % cls.upstream.server_port
        cls.minimax = MiniMax(Path(WORK.name) / "minimax", {"cn": (cls.origin, cls.origin), "global": (cls.origin, cls.origin)})
        cls.minimax._save_grant("cn", {"access_token": "mock-account-session", "refresh_token": "mock-refresh", "expires_in": 3600})
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), GatewayHandler)
        cls.server.daemon_threads = True
        cls.server.gateway = type("Runtime", (), {
            "minimax": cls.minimax, "trae": LocalProvider(cls.origin), "qoder": LocalProvider(cls.origin),
            "catalog": {}, "status": lambda self: {"mock": True},
            "models": lambda self, realm: ([{"id": "trae/test", "owned_by": "trae"}, {"id": "qoder/test", "owned_by": "qoder"}], {}),
            "provider_models": lambda self, name: [{"id": name + "-test", "object": "model"}],
        })()
        runtime = cls.server.gateway
        runtime.directory = Path(WORK.name)
        runtime.origin = "http://127.0.0.1:%s" % cls.server.server_port
        runtime.activity = Activity(runtime.directory)
        runtime.control = Control(runtime)
        runtime.qoder.native = type("Native", (), {"CURRENT_REALM": "cn",
            "account_views": staticmethod(lambda: [{"uid": "qoder-only", "nickname": "Qoder 测试账号", "realm": "cn", "enabled": False}]),
            "recent_usage": staticmethod(lambda count: {"rows": [{"model": "qoder-test", "account": "qoder-only",
                "prompt_tokens": 8, "cached_tokens": 5, "at": 9, "elapsed_ms": 2000, "ttft_ms": 300,
                "status": "completed"}]}),
        })()
        runtime.ports = Ports(runtime, "127.0.0.1", cls.server.server_port)
        import socket
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            runtime.ports.config["port"] = reserved.getsockname()[1]
        runtime.ports.start()
        wb.API_KEY = "gateway-test-key"
        wb.PANEL = wb_settings.PanelSessions()
        cls.url = "http://127.0.0.1:%s" % cls.server.server_port
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.gateway.ports.stop()
        cls.server.shutdown()
        cls.server.server_close()
        cls.upstream.shutdown()
        cls.upstream.server_close()
        WORK.cleanup()

    def setUp(self):
        MockProvider.requests.clear()
        MockProvider.oauth_pending = False
        MockProvider.refresh_failure = False
        MockProvider.use_tools = False
        wb_settings.set_api_keys(wb.ACCOUNTS_DIR, [])

    def request(self, path, body=None, key="gateway-test-key", cookie=None):
        headers = {"Content-Type": "application/json"}
        if key:
            headers["Authorization"] = "Bearer " + key
        if cookie:
            headers["Cookie"] = cookie
        request = urllib.request.Request(self.url + path, None if body is None else json.dumps(body).encode(), headers)
        try:
            response = urllib.request.urlopen(request, timeout=10)
        except urllib.error.HTTPError as exc:
            response = exc
        with response:
            return response.status, response.read(), response.headers

    def test_auth_blocks_upstream_and_panels(self):
        for path in ("/v1/chat/completions", "/v1/responses", "/v1/messages"):
            status, _, _ = self.request(path, {"model": "trae/test", "messages": []}, key="wrong")
            self.assertEqual(status, 401)
        for path in ("/gateway/status", "/trae/api/accounts", "/qoder/accounts"):
            status, _, _ = self.request(path)
            self.assertEqual(status, 401)
        self.assertEqual(MockProvider.requests, [])

    def test_shared_panel_cookie_and_logout(self):
        status, body, headers = self.request("/panel/login", {"password": "admin"}, key=None)
        self.assertEqual(status, 200, body)
        cookie = headers["Set-Cookie"].split(";", 1)[0]
        self.assertIn("HttpOnly", headers["Set-Cookie"])
        self.assertEqual(self.request("/gateway/status", cookie=cookie, key=None)[0], 200)
        self.assertEqual(self.request("/trae/", cookie=cookie, key=None)[0], 200)
        self.assertEqual(self.request("/qoder/", cookie=cookie, key=None)[0], 200)
        self.request("/panel/logout", {}, cookie=cookie, key=None)
        self.assertEqual(self.request("/gateway/status", cookie=cookie, key=None)[0], 401)

    def test_desktop_header_bootstraps_cookie_without_query_token(self):
        token = wb.PANEL.create()
        request = urllib.request.Request(self.url + "/panel/status", headers={"X-Panel-Token": token})
        with urllib.request.urlopen(request) as response:
            self.assertTrue(json.load(response)["authenticated"])
            self.assertIn("gateway_session=", response.headers["Set-Cookie"])
        self.assertEqual(self.request("/gateway/status?token=" + token, key=None)[0], 401)

    def test_panel_rewrites_routes_not_slash_delimiters(self):
        token = wb.PANEL.create()
        _, body, _ = self.request("/trae/", cookie="gateway_session=" + token)
        self.assertIn(b'fetch("/trae/api/accounts")', body)
        self.assertIn(b'fetch("/trae/update/check")', body)
        self.assertIn(b'fetch("/trae/diag/vm")', body)
        self.assertIn(b'const slash = "/"', body)

    def test_native_panel_token_does_not_shadow_gateway_cookie(self):
        token = wb.PANEL.create()
        request = urllib.request.Request(self.url + "/gateway/status", headers={
            "X-Panel-Token": "native-panel-session", "Cookie": "gateway_session=" + token})
        with urllib.request.urlopen(request) as response:
            self.assertEqual(response.status, 200)

    def test_native_providers_route_all_protocols_and_streams(self):
        for provider in ("trae", "qoder"):
            for path in ("/v1/chat/completions", "/v1/responses", "/v1/messages"):
                for stream in (False, True):
                    status, body, headers = self.request(path, {"model": provider + "/same-name", "messages": [{"role": "user", "content": "hi"}], "stream": stream})
                    self.assertEqual(status, 200, body)
                    self.assertEqual(MockProvider.requests[-1][1]["model"], "same-name")
                    self.assertEqual(headers["X-Gateway-Provider"], provider)
                    self.assertIn(b"[DONE]" if stream else b"provider", body)

    def test_workbuddy_prefix_uses_original_engine(self):
        def dispatch(handler, path, body):
            return handler._json(200, {"model": body["model"]})
        with mock.patch.object(wb.Handler, "_dispatch_chat_post", dispatch):
            for model in ("workbuddy/test", "test"):
                status, body, _ = self.request("/v1/chat/completions", {"model": model})
                self.assertEqual(status, 200)
                self.assertEqual(json.loads(body)["model"], "test")

    def test_namespaced_key_model_restriction(self):
        wb_settings.set_api_keys(wb.ACCOUNTS_DIR, [{"key": "restricted", "name": "Trae only", "enabled": True, "models": ["trae/test"]}])
        allowed = self.request("/v1/chat/completions", {"model": "trae/test", "messages": []}, key="restricted")
        denied = self.request("/v1/chat/completions", {"model": "qoder/test", "messages": []}, key="restricted")
        self.assertEqual(allowed[0], 200, allowed[1])
        self.assertEqual(denied[0], 400, denied[1])
        self.assertEqual(len(MockProvider.requests), 1)

    def test_minimax_chat_and_responses(self):
        for path in ("/v1/chat/completions", "/v1/responses"):
            for stream in (False, True):
                payload = {"model": "minimax/MiniMax-Test", "stream": stream}
                payload["input" if path.endswith("responses") else "messages"] = "hi" if path.endswith("responses") else [{"role": "user", "content": "hi"}]
                status, body, _ = self.request(path, payload)
                self.assertEqual(status, 200, body)
                if stream:
                    self.assertIn(b"response.completed" if path.endswith("responses") else b"[DONE]", body)
                else:
                    result = json.loads(body)
                    self.assertEqual(result["model"], "minimax/MiniMax-Test")
                    self.assertIn("你好", str(result))
                forwarded = MockProvider.requests[-1]
                self.assertEqual(forwarded[1]["model"], "MiniMax-Test")
                self.assertEqual(forwarded[2]["Authorization"], "Bearer mock-account-session")

    def test_minimax_native_messages_preserves_fields(self):
        payload = {"model": "minimax/MiniMax-Test", "max_tokens": 33, "messages": [{"role": "user", "content": "hi"}], "thinking": {"type": "enabled", "budget_tokens": 16}}
        status, body, _ = self.request("/v1/messages", payload)
        self.assertEqual(status, 200, body)
        self.assertEqual(MockProvider.requests[-1][1]["thinking"], payload["thinking"])
        self.assertEqual(MockProvider.requests[-1][1]["max_tokens"], 33)

    def test_fragmented_tool_calls_and_real_usage(self):
        result = wb.aggregate_stream(minimax_chat_stream(io.BytesIO(anthropic_events(True)), "minimax/test"), "minimax/test", None)
        tool = result["choices"][0]["message"]["tool_calls"][0]
        self.assertEqual(json.loads(tool["function"]["arguments"]), {"city": "上海"})
        self.assertEqual(result["choices"][0]["finish_reason"], "tool_calls")
        self.assertEqual(result["usage"]["prompt_tokens"], 10)
        self.assertEqual(result["usage"]["total_tokens"], 14)
        self.assertEqual(result["usage"]["prompt_tokens_details"]["cached_tokens"], 5)

    def test_truncated_stream_is_an_error(self):
        raw = anthropic_events().split(b"event: message_stop")[0]
        with self.assertRaises(ProviderError):
            list(minimax_chat_stream(io.BytesIO(raw), "minimax/test"))

    def test_zero_argument_tool_produces_valid_json(self):
        events = [
            {"type": "message_start", "message": {"id": "zero-tool"}},
            {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "call-zero", "name": "clock", "input": {}}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
            {"type": "message_stop"},
        ]
        raw = b"".join(("data: " + json.dumps(event) + "\n\n").encode() for event in events)
        result = wb.aggregate_stream(minimax_chat_stream(io.BytesIO(raw), "minimax/test"), "minimax/test", None)
        arguments = result["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
        self.assertEqual(json.loads(arguments), {})

    def test_tool_results_stay_correlated(self):
        result = chat_to_anthropic({"model": "test", "messages": [
            {"role": "system", "content": "guide"}, {"role": "user", "content": "hi"},
            {"role": "assistant", "tool_calls": [{"id": "call-x", "function": {"name": "weather", "arguments": '{"city":"上海"}'}}]},
            {"role": "tool", "tool_call_id": "call-x", "content": "sun"},
        ]})
        self.assertEqual(result["messages"][1]["content"][0]["id"], "call-x")
        self.assertEqual(result["messages"][2]["content"][0]["tool_use_id"], "call-x")

    def test_oauth_pkce_pending_and_success(self):
        result = self.minimax.start_login("global")
        request = MockProvider.requests[-1][1]
        self.assertEqual(request["code_challenge_method"], "S256")
        self.assertNotIn("code_verifier", request)
        self.assertNotIn("device_code", result)
        pending = self.minimax.pending[result["login_id"]]
        pending["next_poll"] = 0
        MockProvider.oauth_pending = True
        self.assertEqual(self.minimax.poll_login(result["login_id"])["status"], "pending")
        MockProvider.oauth_pending = False
        pending["next_poll"] = 0
        self.assertEqual(self.minimax.poll_login(result["login_id"])["status"], "authenticated")
        self.assertEqual(self.minimax.region, "global")
        self.assertNotIn("mock-account-session", json.dumps(self.minimax.status()))
        self.minimax.set_region("cn")

    def test_failed_refresh_keeps_last_credentials(self):
        path = self.minimax.directory / "cn.json"
        credentials = json.loads(path.read_text())
        credentials["expires_at"] = time.time() - 1
        path.write_text(json.dumps(credentials))
        before = path.read_bytes()
        MockProvider.refresh_failure = True
        with self.assertRaises(ProviderError):
            self.minimax.access_token()
        self.assertEqual(path.read_bytes(), before)
        MockProvider.refresh_failure = False
        self.minimax.access_token()

    def test_first_launch_copies_existing_accounts_once(self):
        import argparse
        import gateway_app
        with tempfile.TemporaryDirectory(prefix="gateway-migration-") as folder:
            root = Path(folder)
            source = root / "accounts"
            source.mkdir()
            record = source / "account.json"
            record.write_text('{"uid":"existing-account"}', encoding="utf-8")
            original = record.read_bytes()
            with mock.patch.object(gateway_app, "__file__", str(root / "gateway_app.py")), \
                    mock.patch.object(gateway_app.logging, "basicConfig"), mock.patch.dict(os.environ):
                _, destination, _ = gateway_app.prepare_paths(argparse.Namespace(data_dir=None))
                copied = destination / "account.json"
                self.assertEqual(copied.read_bytes(), original)
                copied.write_text('{"uid":"gateway-account"}', encoding="utf-8")
                gateway_app.prepare_paths(argparse.Namespace(data_dir=None))
                self.assertEqual(copied.read_text(), '{"uid":"gateway-account"}')
                self.assertEqual(record.read_bytes(), original)

    def test_model_catalog_retains_previous_on_failure(self):
        self.assertEqual(self.minimax.models(), ["MiniMax-Test"])
        self.minimax.catalog["cn"] = (0, ["MiniMax-Test"])
        with mock.patch("gateway.minimax.json_request", side_effect=ProviderError("offline")):
            self.assertEqual(self.minimax.models(), ["MiniMax-Test"])

    def test_previous_response_id_rejected_before_call(self):
        status, _, _ = self.request("/v1/responses", {"model": "minimax/test", "input": "hi", "previous_response_id": "old"})
        self.assertEqual(status, 400)
        self.assertEqual(MockProvider.requests, [])

    def test_invalid_minimax_messages_return_readable_400(self):
        status, body, _ = self.request("/v1/chat/completions", {"model": "minimax/test", "messages": "bad"})
        self.assertEqual(status, 400, body)
        self.assertEqual(MockProvider.requests, [])

    def test_unified_snapshots_are_scoped_and_panel_only(self):
        cookie = "gateway_session=" + wb.PANEL.create()
        for name in ("workbuddy", "trae", "minimax", "qoder"):
            path = "/gateway/providers/" + name + "/snapshot"
            self.assertEqual(self.request(path)[0], 401)
            status, body, _ = self.request(path, cookie=cookie)
            data = json.loads(body)
            self.assertEqual(status, 200, body)
            self.assertEqual(data["provider"], name)
            self.assertEqual(data["gateway"]["port"], self.server.gateway.ports.config["port"])
            self.assertNotIn("never-expose-credentials", body.decode())
            self.assertNotIn("mock-account-session", body.decode())
            if name in ("trae", "qoder"):
                self.assertEqual(data["accounts"][0]["id"], name + "-only")

    def port_request(self, name, path, body=None):
        origin = "http://127.0.0.1:%s" % self.server.gateway.ports.info(name)["port"]
        payload = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(origin + ("/" + name if name else "") + path, payload, {"Authorization": "Bearer gateway-test-key", "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read()

    def test_shared_port_groups_route_bare_models_and_reject_wrong_provider(self):
        for name in ("trae", "qoder", "minimax"):
            status, body = self.port_request(name, "/v1/chat/completions", {"model": "test", "messages": [{"role": "user", "content": "hello"}]})
            self.assertEqual(status, 200, body)
            self.assertEqual(MockProvider.requests[-1][1]["model"], "test")
            self.assertEqual(json.loads(self.port_request(name, "/health")[1])["provider"], name)
            self.assertEqual(self.port_request(name, "/gateway/providers/" + name + "/snapshot")[0], 404)
            other = "qoder" if name != "qoder" else "trae"
            self.assertEqual(self.port_request(name, "/v1/chat/completions", {"model": other + "/test", "messages": []})[0], 400)

    def test_workbuddy_group_uses_original_engine(self):
        def original(handler, path, payload):
            handler._json(200, {"model": payload["model"], "usage": {"prompt_tokens": 2, "completion_tokens": 3}})
        with mock.patch.object(wb.Handler, "_dispatch_chat_post", original):
            self.assertEqual(self.port_request("workbuddy", "/v1/chat/completions", {"model": "test", "messages": []})[0], 200)
        row = self.server.gateway.activity.view("workbuddy")["records"][0]
        self.assertEqual((row["input_tokens"], row["output_tokens"]), (2, 3))

    def test_one_listener_serves_all_groups_concurrently(self):
        import concurrent.futures
        names = ("workbuddy", "trae", "minimax", "qoder")
        def original(handler, path, payload):
            handler._json(200, {"model": payload["model"]})
        def call(name):
            return self.port_request(name, "/v1/chat/completions", {
                "model": "parallel-" + name, "messages": [{"role": "user", "content": "hello"}]})
        with mock.patch.object(wb.Handler, "_dispatch_chat_post", original), concurrent.futures.ThreadPoolExecutor() as executor:
            replies = list(executor.map(call, names))
        for name, (status, body) in zip(names, replies):
            self.assertEqual(status, 200, body)
            row = next(item for item in self.server.gateway.activity.view(name)["records"] if item["model"] == "parallel-" + name)
            self.assertEqual(row["provider"], name)
            self.assertEqual(row["port"], self.server.gateway.ports.config["port"])
        self.assertEqual({self.server.gateway.ports.info(name)["port"] for name in names}, {self.server.gateway.ports.server.server_port})
        self.assertEqual(self.port_request(None, "/v1/chat/completions", {"model": "trae/test", "messages": []})[0], 200)

    def test_shared_gateway_configuration_is_panel_only_and_persists(self):
        ports = self.server.gateway.ports
        self.assertEqual(self.request("/gateway/configure", {"port": ports.info()["port"], "enabled": True})[0], 401)
        status, body, _ = self.request("/gateway/configure", {"host": "127.0.0.1", "port": ports.info()["port"], "enabled": True},
                                      cookie="gateway_session=" + wb.PANEL.create())
        self.assertEqual(status, 200, body)
        restored = Ports(self.server.gateway, "0.0.0.0", self.server.server_port)
        self.assertEqual(restored.config, ports.config)
        self.assertEqual(len(restored.info()["groups"]), 4)

    def test_group_api_preflight_allows_authorization_and_blocks_management(self):
        origin = "http://127.0.0.1:%s" % self.server.gateway.ports.info()["port"]
        for name in ("workbuddy", "trae", "minimax", "qoder"):
            request = urllib.request.Request(origin + "/" + name + "/v1/models", method="OPTIONS",
                headers={"Origin": "https://client.example", "Access-Control-Request-Headers": "authorization"})
            with urllib.request.urlopen(request, timeout=5) as response:
                self.assertEqual(response.status, 204)
                self.assertEqual(response.headers["Access-Control-Allow-Origin"], "*")
                self.assertIn("Authorization", response.headers["Access-Control-Allow-Headers"])
        request = urllib.request.Request(origin + "/workbuddy/gateway/configure", method="OPTIONS")
        with self.assertRaises(urllib.error.HTTPError) as raised:
            urllib.request.urlopen(request, timeout=5)
        self.assertEqual(raised.exception.code, 404)

    def test_qoder_region_switch_drops_previous_region_catalog(self):
        self.server.gateway.qoder.catalog = (time.monotonic() + 60, [{"id": "old-region"}])
        with mock.patch.object(self.server.gateway.control, "call", return_value={"ok": True}):
            self.server.gateway.control.action("qoder", "region", {"region": "intl"}, "token")
        self.assertIsNone(self.server.gateway.qoder.catalog)

    def test_old_port_configuration_migrates_with_backup(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            legacy = {"workbuddy": {"port": 12345, "enabled": False}, "trae": {"port": 12346, "enabled": True}}
            (directory / "ports.json").write_text(json.dumps(legacy), encoding="utf-8")
            runtime = type("Runtime", (), {"directory": directory})()
            ports = Ports(runtime, "127.0.0.1", 12344)
            self.assertEqual(ports.config["port"], 12345)
            ports.configure("qoder", 12345, False)
            self.assertEqual(json.loads((directory / "ports.v0.3.backup.json").read_text()), legacy)
            self.assertEqual(Ports(runtime, "127.0.0.1", 12344).config, ports.config)

    def test_credentials_refresh_routes_and_minimax_failure_preserves_session(self):
        control = self.server.gateway.control
        with mock.patch.object(control, "call", return_value={"results": [{"ok": True}]} ) as native:
            for name in ("workbuddy", "qoder"):
                control.action(name, "credentials", {"id": "one"}, "panel-token")
                self.assertEqual(native.call_args.args[:2], (name, "/accounts/refresh"))
                self.assertEqual(native.call_args.args[2]["uid"], "one")
        with mock.patch.object(self.server.gateway.trae, "refresh_account", create=True, return_value={"ok": False}) as refresh:
            self.assertFalse(control.action("trae", "credentials", {"id": "one"}, "token")["ok"])
            refresh.assert_called_once_with("one")
        before = self.minimax._load("cn")
        MockProvider.refresh_failure = True
        with self.assertRaises(ProviderError):
            control.action("minimax", "credentials", {"id": before["id"]}, "token")
        self.assertEqual(self.minimax._load("cn"), before)
        MockProvider.refresh_failure = False
        result = control.action("minimax", "credentials", {"id": before["id"]}, "token")
        self.assertTrue(result["ok"])
        self.assertNotIn("access_token", result)

    def test_activity_keeps_provider_port_tokens_and_failure(self):
        self.minimax.set_region("cn")
        status, _ = self.port_request("minimax", "/v1/chat/completions", {"model": "activity-test", "messages": [{"role": "user", "content": "secret prompt must not be persisted"}], "stream": True})
        self.assertEqual(status, 200)
        row = self.server.gateway.activity.view("minimax")["records"][0]
        self.assertEqual(row["model"], "activity-test")
        self.assertEqual(row["status"], "success")
        self.assertGreater(row["input_tokens"], 0)
        self.assertEqual(row["output_tokens"], 4)
        self.assertEqual(row["port"], self.server.gateway.ports.info("minimax")["port"])
        self.assertNotIn("secret prompt", json.dumps(row))
        self.assertFalse(any(item["model"] == "activity-test" for item in self.server.gateway.activity.view("qoder")["records"]))
        self.port_request("minimax", "/v1/chat/completions", {"model": "invalid-input", "messages": "bad"})
        self.assertEqual(self.server.gateway.activity.view("minimax")["records"][0]["status"], "failed")

    def test_port_stop_start_change_and_conflict_preserve_old_listener(self):
        import socket
        ports = self.server.gateway.ports
        previous = ports.info("trae")["port"]
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", 0))
            with self.assertRaises(ProviderError):
                ports.configure("trae", occupied.getsockname()[1], True)
        self.assertEqual(ports.info("trae")["port"], previous)
        self.assertEqual(self.port_request("trae", "/health")[0], 200)
        ports.configure("trae", previous, False)
        self.assertFalse(ports.info("trae")["running"])
        with self.assertRaises(urllib.error.URLError):
            self.port_request("trae", "/health")
        ports.configure("trae", previous, True)
        self.assertEqual(self.port_request("trae", "/health")[0], 200)
        with socket.socket() as replacement:
            replacement.bind(("127.0.0.1", 0))
            new_port = replacement.getsockname()[1]
        ports.configure("trae", new_port, True)
        self.assertEqual(ports.info("trae")["port"], new_port)
        self.assertEqual(self.port_request("trae", "/health")[0], 200)
        ports.configure("trae", previous, True)

    def test_minimax_pool_rotates_disables_deletes_and_persists(self):
        with tempfile.TemporaryDirectory() as folder:
            client = MiniMax(folder)
            first = client._save_grant("cn", {"access_token": "first", "expires_in": 3600}, new_account=True)
            second = client._save_grant("cn", {"access_token": "second", "expires_in": 3600}, new_account=True)
            self.assertNotEqual(first, second)
            self.assertEqual([client.access_token()[1] for _ in range(4)], ["first", "second", "first", "second"])
            client.pool.mutate(first, False)
            self.assertEqual(client.access_token()[1], "second")
            self.assertFalse(MiniMax(folder).pool.public()[0]["enabled"])
            client.pool.mutate(second, delete=True)
            with self.assertRaises(ProviderError):
                client.access_token()
            self.assertNotIn('"access_token"', json.dumps(client.pool.public()))

    def test_usage_adapter_reads_native_rows_and_preserves_unknowns(self):
        rows = self.server.gateway.control.backend_rows("qoder", 100)
        self.assertEqual((rows[0]["timestamp"], rows[0]["finished_at"]), (7, 9))
        self.assertEqual((rows[0]["input_tokens"], rows[0]["cached_tokens"], rows[0]["new_input_tokens"]),
                         (8, 5, 3))
        self.assertEqual(rows[0]["ttft_ms"], 300)
        self.assertEqual(rows[0]["status"], "success")
        self.assertEqual(rows[0]["account"], "qoder-only")
        self.assertIsNone(rows[0]["output_tokens"])
        self.assertIsNone(rows[0]["tokens_per_sec"])
        self.assertEqual(self.server.gateway.control.backend_rows("minimax", 100), [])
        self.assertIsNone(account_view("trae", {"id": "unknown", "is_valid": True})["remaining"])

    def test_board_joins_backend_rows_by_completion_time(self):
        control = self.server.gateway.control
        activity = self.server.gateway.activity
        start = time.time() - 60
        with mock.patch("gateway.activity.time.time", side_effect=[start, start + 0.5]):
            request = activity.begin("qoder", "qoder/board-test", "chat", 1, "board-session")
            request.update(input_tokens=0, output_tokens=2)
            activity.finish(request, 200)
        record = {"model": "board-test", "at": start + 0.5, "elapsed_ms": 500, "prompt_tokens": 92000,
                  "completion_tokens": 2, "cached_tokens": 90000, "ttft_ms": 310, "credit": 2.4,
                  "account": "qoder-only", "status": "completed"}
        with mock.patch.object(self.server.gateway.qoder.native, "recent_usage", return_value={"rows": [record]}):
            board = control.board()
        rows = [row for row in board["rows"] if row["model"] == "board-test"]
        self.assertEqual(len(rows), 1)
        merged = rows[0]
        self.assertEqual((merged["input_tokens"], merged["cached_tokens"], merged["new_input_tokens"]),
                         (92000, 90000, 2000))
        self.assertEqual((merged["account"], merged["ttft_ms"], merged["credits"]), ("qoder-only", 310, 2.4))
        self.assertTrue(merged["session_id"].startswith("ses_"))
        self.assertEqual({row["id"]: row["name"] for row in board["facets"]["accounts"]}["qoder-only"],
                         "Qoder 测试账号")
        self.assertEqual({row["id"]: row["count"] for row in board["facets"]["providers"]}["qoder"], 1)
        cookie = "gateway_session=" + wb.PANEL.create()
        self.assertEqual(self.request("/gateway/usage/board")[0], 401)
        status, raw, _ = self.request("/gateway/usage/board", cookie=cookie)
        self.assertEqual(status, 200)
        self.assertNotIn("board-session", raw.decode())

    def test_board_estimates_throughput_when_backend_omits_it(self):
        control = self.server.gateway.control
        activity = self.server.gateway.activity
        start = time.time() - 30
        with mock.patch("gateway.activity.time.time", side_effect=[start, start + 5]):
            request = activity.begin("minimax", "slow", "messages", 1)
            request.update(output_tokens=120, ttft_ms=1000)
            activity.finish(request, 200)
        with mock.patch.object(control, "backend_rows", return_value=[]):
            row = next(item for item in control.board()["rows"] if item["model"] == "slow")
        self.assertEqual(row["tokens_per_sec"], 30)
        self.assertEqual(row["new_input_tokens"], None)

    def test_management_models_keep_credit_factors_and_unknowns(self):
        status, _, headers = self.request("/panel/login", {"password": "admin"})
        self.assertEqual(status, 200)
        cookie = headers.get("Set-Cookie").split(";")[0]
        catalogs = {
            "workbuddy": [{"id": "free", "credits": 0}, {"id": "paid", "credits": "x0.79"}],
            "qoder": [{"id": "peak-valley", "price_factor": 0.5, "price_factor_peak": 0.5,
                "price_factor_valley": 0.2, "off_peak_active_now": False,
                "off_peak": {"window_start": "22:00", "window_end": "08:00", "timezone": "Asia/Shanghai"}}],
            "trae": [{"id": "unknown", "token": "private-credential", "price": 123}],
            "minimax": [{"id": "unknown"}],
        }
        with mock.patch.object(self.server.gateway, "provider_models", side_effect=lambda name: catalogs[name]):
            for name, expected in catalogs.items():
                path = "/gateway/providers/" + name + "/models"
                self.assertEqual(self.request(path)[0], 401)
                status, raw, _ = self.request(path, cookie=cookie)
                self.assertEqual(status, 200)
                data = json.loads(raw)
                self.assertEqual(data["provider"], name)
                for source, row in zip(expected, data["data"]):
                    for key, value in source.items():
                        if key not in ("token", "price"):
                            self.assertEqual(row[key], value)
                    self.assertNotIn("token", row)
                    self.assertNotIn("price", row)
                if name in ("trae", "minimax"):
                    self.assertNotIn("credits", data["data"][0])
                    self.assertNotIn("price_factor", data["data"][0])

    def test_workbuddy_zero_credit_multiplier_survives_catalog_adapter(self):
        from gateway.runtime import Runtime
        with mock.patch.object(wb, "fetch_models", return_value=[("free", {"credits": 0})]), \
                mock.patch.object(wb, "model_entry", return_value={"id": "free", "object": "model"}):
            rows = Runtime.provider_models(None, "workbuddy")
        self.assertEqual(rows[0]["credits"], 0)

    def test_runtime_models_excludes_unprefixed_workbuddy_models(self):
        from gateway.runtime import Runtime
        rt = mock.MagicMock()
        rt.catalog_lock = threading.Lock()
        rt.catalog = {}
        rt.errors = {}
        rt.trae.models = lambda: [{"id": "trae-m", "object": "model"}]
        rt.qoder.models = lambda: [{"id": "qoder-m", "object": "model"}]
        rt.minimax.models = lambda: [{"id": "minimax-m", "object": "model"}]
        with mock.patch.object(wb, "fetch_models", return_value=[("wb-m", {})]), \
                mock.patch.object(wb, "model_entry", return_value={"id": "wb-m", "object": "model"}):
            models, errors = Runtime.models(rt, realm="cn")
        ids = [row["id"] for row in models]
        self.assertIn("workbuddy/wb-m", ids)
        self.assertNotIn("wb-m", ids)
        for model_id in ids:
            self.assertIn("/", model_id)

    def test_daily_totals_outlive_recent_request_window(self):
        with tempfile.TemporaryDirectory() as folder:
            activity = Activity(Path(folder))
            for _ in range(2001):
                activity.finish(activity.begin("qoder", "test", "chat", 1), 200)
            self.assertEqual(activity.view("qoder")["summary"]["requests"], 2001)
            restored = Activity(Path(folder))
            self.assertEqual(restored.view("qoder")["summary"]["requests"], 2001)
            self.assertEqual(len(restored.view("qoder")["records"]), 200)

    def test_group_model_limits_keep_workbuddy_legacy_names(self):
        wb_settings.set_api_keys(wb.ACCOUNTS_DIR, [{"id": "limited", "name": "legacy", "key": "gateway-test-key", "models": ["workbuddy-test"], "enabled": True}])
        self.assertEqual(json.loads(self.port_request("workbuddy", "/v1/models")[1])["data"][0]["id"], "workbuddy-test")
        self.assertEqual(json.loads(self.port_request("trae", "/v1/models")[1])["data"], [])
        def original(handler, path, payload):
            handler._json(200, {"model": payload["model"]})
        with mock.patch.object(wb.Handler, "_dispatch_chat_post", original):
            self.assertEqual(self.port_request("workbuddy", "/v1/chat/completions", {"model": "workbuddy-test", "messages": []})[0], 200)
        self.assertEqual(self.port_request("trae", "/v1/chat/completions", {"model": "workbuddy-test", "messages": []})[0], 400)

    def test_gateway_rows_sniff_cache_tokens_and_first_token_timing(self):
        self.minimax.set_region("cn")
        status, _ = self.port_request("minimax", "/v1/messages",
                                      {"model": "sniff-test", "messages": [{"role": "user", "content": "hi"}], "stream": True})
        self.assertEqual(status, 200)
        row = self.server.gateway.activity.view("minimax")["records"][0]
        self.assertEqual((row["input_tokens"], row["cached_tokens"], row["output_tokens"]), (10, 5, 4))
        self.assertEqual(row["reasoning_tokens"], None)
        self.assertIsInstance(row["ttft_ms"], int)
        self.assertEqual(row["account"], "cn-primary")

    def test_activity_records_span_providers_and_persist(self):
        with tempfile.TemporaryDirectory() as folder:
            activity = Activity(Path(folder))
            for name in ("trae", "qoder"):
                for _ in range(3):
                    activity.finish(activity.begin(name, "model", "chat", 1), 200)
            self.assertEqual({row["provider"] for row in activity.records()}, {"trae", "qoder"})
            self.assertEqual(len(activity.records(2)), 2)
            restored = Activity(Path(folder))
            self.assertEqual(len(restored.records()), 6)
            in_flight = restored.begin("trae", "model", "chat", 1)
            self.assertEqual(restored.records()[0]["id"], in_flight["id"])
            self.assertEqual(restored.records()[0]["status"], "processing")

    def test_board_keeps_sessions_of_real_http_calls_and_hides_raw_identifier(self):
        session_id = "private-session-" + str(time.time_ns())
        self.minimax.set_region("cn")
        body = {"model": "minimax/session-test", "messages": [{"role": "user", "content": "hello"}],
                "metadata": {"conversation_id": session_id}}
        for _ in range(2):
            self.assertEqual(self.request("/v1/chat/completions", body)[0], 200)
        rows = [row for row in self.server.gateway.activity.records() if row["model"] == "session-test"]
        self.assertEqual(len(rows), 2)
        self.assertEqual({row["session_id"] for row in rows}, {rows[0]["session_id"]})
        self.assertEqual(rows[0]["session_source"], "client")
        self.assertNotIn(session_id, json.dumps([self.server.gateway.control.board()["rows"]]))

    def test_bulk_actions_scope_failures_and_boolean_state(self):
        control = self.server.gateway.control
        fixture = {"region": "cn", "accounts": [
            {"id":"first", "name":"First", "region":"cn", "enabled":True, "checkin":{"supported":True}},
            {"id":"second", "name":"Second", "region":"cn", "enabled":True, "checkin":{"supported":True}},
            {"id":"global", "name":"International", "region":"intl", "enabled":True, "checkin":{"supported":False}},
        ], "capabilities":{"checkin":True,"credits":True}}
        with mock.patch.object(control, "snapshot", return_value=fixture), mock.patch.object(control, "call") as upstream, mock.patch("gateway.operations.time.sleep"):
            upstream.side_effect = [{"results":[{"ok":True,"msg":"signed"}]}, ProviderError("account expired", 401)]
            result = control.action("workbuddy", "batch", {"operation":"checkin"}, "panel")
            self.assertEqual((result["succeeded"],result["failed"],result["skipped"]), (1,1,0))
            self.assertEqual([call.args[1] for call in upstream.call_args_list], ["/accounts/checkin", "/accounts/checkin"])
            self.assertEqual([call.args[2]["uid"] for call in upstream.call_args_list], ["first","second"])
            self.assertIn("account expired", result["results"][1]["message"])
            upstream.reset_mock(); upstream.side_effect = None; upstream.return_value = {"account":{"enabled":False}}
            result = control.action("workbuddy", "batch", {"operation":"disable","ids":["first"]}, "panel")
            self.assertEqual(result["succeeded"],1)
            self.assertFalse(upstream.call_args.args[2]["enabled"])
            with self.assertRaises(ProviderError):
                control.action("workbuddy", "batch", {"operation":"enable","ids":["global"]}, "panel")
            self.assertEqual(upstream.call_count,1)

    def test_bulk_unavailable_capability_skips_disabled_qoder_and_truthful_checkin(self):
        control = self.server.gateway.control
        with self.assertRaises(ProviderError):
            control.action("minimax", "batch", {"operation":"checkin"}, "panel")
        result = control.action("qoder", "batch", {"operation":"checkin"}, "panel")
        self.assertEqual((result["succeeded"], result["skipped"]), (0,1))
        view = account_view("workbuddy", {"uid":"attempt-only","realm":"cn","lastCheckin":"2099-01-01"})
        self.assertIsNone(view["checkin"]["done"])

    def test_trae_single_account_checkin_maps_to_claim_endpoint(self):
        control = self.server.gateway.control
        with mock.patch.object(control, "call", return_value={"success":True}) as upstream:
            control.action("trae", "checkin", {"id":"id/with slash"}, "panel")
            self.assertEqual(upstream.call_args.args[1], "/api/checkin/account/id%2Fwith%20slash")

    def test_batch_management_requires_panel_session(self):
        path = "/gateway/providers/qoder/batch"
        self.assertEqual(self.request(path,{"operation":"enable"})[0],401)
        cookie = "gateway_session=" + wb.PANEL.create()
        control = self.server.gateway.control
        with mock.patch.object(control,"call",return_value={"account":{"enabled":True}}) as upstream:
            self.assertEqual(self.request(path,{"operation":"enable"},cookie=cookie)[0],200)
            self.assertEqual(upstream.call_args.args[1],"/accounts/set")

    def test_board_orders_by_start_time_and_keeps_unmatched_requests(self):
        control = self.server.gateway.control
        activity = self.server.gateway.activity
        now = time.time()
        with mock.patch("gateway.activity.time.time", side_effect=[now - 20, now - 10]):
            finished = activity.begin("trae", "old-model", "chat", 1)
            activity.finish(finished, 200)
        pending = activity.begin("trae", "pending-model", "chat", 1)
        with mock.patch.object(control, "backend_rows", return_value=[]):
            models = [row["model"] for row in control.board()["rows"] if row["provider"] == "trae"]
        self.assertEqual(models[0], "pending-model")
        self.assertLess(models.index("old-model"), len(models))
        self.assertEqual([row["status"] for row in control.board()["rows"]
                          if row["model"] == "pending-model"], ["processing"])
        activity.finish(pending, 200)

    def test_dashboard_ui_assets_support_back_button_and_no_sidebar(self):
        ui_dir = Path(__file__).resolve().parent.parent / "ui"
        dashboard_html = (Path(__file__).resolve().parent.parent / "dashboard.html").read_text(encoding="utf-8")
        styles_css = (ui_dir / "styles.css").read_text(encoding="utf-8")
        icons_js = (ui_dir / "icons.js").read_text(encoding="utf-8")
        app_js = (ui_dir / "app.js").read_text(encoding="utf-8")

        self.assertIn('id="back-button"', dashboard_html)
        self.assertIn('topbar-nav', dashboard_html)
        self.assertIn('body.no-sidebar .sidebar', styles_css)
        self.assertIn('body.no-sidebar .workspace', styles_css)
        self.assertIn('.back-button', styles_css)
        self.assertIn('back:', icons_js)
        self.assertIn('no-sidebar', app_js)
        self.assertIn('back-button', app_js)
        self.assertIn('sidebar.hidden = false', app_js)
        self.assertIn('.rail-tool.selected', styles_css)
        self.assertIn('isAccounts', app_js)
        self.assertIn('data-view', app_js)
        self.assertIn('id="sidebar-toggle"', dashboard_html)
        self.assertIn('nav-label', dashboard_html)
        self.assertIn('sidebar-expanded', styles_css)
        self.assertIn('.sidebar-toggle-btn', styles_css)
        self.assertIn('sidebar-toggle', app_js)
        self.assertIn('setSidebarExpanded', app_js)
        self.assertTrue((ui_dir / "icons" / "workbuddy.svg").exists())
        self.assertTrue((ui_dir / "icons" / "workbuddy.ico").exists())
        status, body, headers = self.request("/gateway/ui/icons/workbuddy.svg")
        self.assertEqual(status, 200)
        self.assertIn("image/svg+xml", headers.get("Content-Type", ""))

    def test_frameless_window_controls_and_gateway_bar_hidden(self):
        ui_dir = Path(__file__).resolve().parent.parent / "ui"
        dashboard_html = (Path(__file__).resolve().parent.parent / "dashboard.html").read_text(encoding="utf-8")
        styles_css = (ui_dir / "styles.css").read_text(encoding="utf-8")
        app_js = (ui_dir / "app.js").read_text(encoding="utf-8")

        self.assertIn('id="window-controls"', dashboard_html)
        self.assertIn('id="win-min"', dashboard_html)
        self.assertIn('id="win-max"', dashboard_html)
        self.assertIn('id="win-close"', dashboard_html)
        self.assertIn('pywebview-drag-region', dashboard_html)
        self.assertIn('.gateway-bar', styles_css)
        self.assertIn('display: none !important', styles_css)
        self.assertIn('initWindowControls', app_js)
        self.assertIn('-webkit-app-region: drag', styles_css)

    def test_api_key_generate_reveal_and_delete(self):
        ui_dir = Path(__file__).resolve().parent.parent / "ui"
        dashboard_html = (Path(__file__).resolve().parent.parent / "dashboard.html").read_text(encoding="utf-8")
        styles_css = (ui_dir / "styles.css").read_text(encoding="utf-8")
        app_js = (ui_dir / "app.js").read_text(encoding="utf-8")
        access_js = (ui_dir / "access.js").read_text(encoding="utf-8")

        self.assertIn('api-keys-list', dashboard_html)
        self.assertIn('key-name', dashboard_html)
        self.assertIn('.api-key-row', styles_css)
        self.assertIn('.key-action-btn', styles_css)
        self.assertIn('renderApiKeys', app_js)
        self.assertIn('renderApiKeys', access_js)

        status, _, headers = self.request("/panel/login", {"password": "admin"})
        self.assertEqual(status, 200)
        cookie = headers.get("Set-Cookie").split(";")[0]

        # Generate a new API Key
        status, body, _ = self.request("/gateway/api-keys/generate", {"name": "Test Key"}, cookie=cookie)
        self.assertEqual(status, 200)
        data = json.loads(body.decode("utf-8"))
        self.assertTrue(data.get("ok"))
        key_info = data.get("key")
        self.assertEqual(key_info.get("name"), "Test Key")
        plain_key = key_info.get("key")
        self.assertTrue(plain_key.startswith("sk-mg-"))
        self.assertIn("*", key_info.get("masked"))
        key_id = key_info.get("id")

        # Reveal the key
        status, reveal_body, _ = self.request("/settings/reveal?id=" + key_id, cookie=cookie)
        self.assertEqual(status, 200)
        reveal_data = json.loads(reveal_body.decode("utf-8"))
        self.assertEqual(reveal_data.get("key"), plain_key)

        # Delete the key
        status, del_body, _ = self.request("/gateway/api-keys/delete", {"id": key_id}, cookie=cookie)
        self.assertEqual(status, 200)
        del_data = json.loads(del_body.decode("utf-8"))
        self.assertTrue(del_data.get("ok"))

    def test_kpi_cards_and_cost_estimation(self):
        from gateway.control import estimate_row_cost_usd
        # 1. Test estimate_row_cost_usd
        row = {
            "model": "deepseek-v4.1-flash",
            "input_tokens": 2000000,
            "cached_tokens": 1000000,
            "output_tokens": 1000000,
        }
        cost = estimate_row_cost_usd(row)
        self.assertAlmostEqual(cost, 0.54, places=2)

        # 2. Check dashboard.html structure
        dashboard_html = (Path(__file__).resolve().parent.parent / "dashboard.html").read_text(encoding="utf-8")
        self.assertIn("<h1>用量统计</h1>", dashboard_html)
        self.assertNotIn("log-count", dashboard_html)
        self.assertIn('id="kpi-grid"', dashboard_html)
        self.assertIn('id="kpi-sub-grid"', dashboard_html)
        self.assertIn('id="kpi-expand-btn"', dashboard_html)
        self.assertIn('更多指标', dashboard_html)

        # 3. Check ui assets
        ui_dir = Path(__file__).resolve().parent.parent / "ui"
        board_js = (ui_dir / "board.js").read_text(encoding="utf-8")
        styles_css = (ui_dir / "styles.css").read_text(encoding="utf-8")
        icons_js = (ui_dir / "icons.js").read_text(encoding="utf-8")

        self.assertIn("formatTokens", board_js)
        self.assertIn("formatCostUSD", board_js)
        self.assertIn("积分消耗", board_js)
        self.assertIn("kpi-sub-card", board_js)
        self.assertIn("export function bucketize", board_js)
        self.assertIn('board.range === "today"', board_js)
        self.assertIn('board.range === "7"', board_js)
        self.assertIn('board.range === "30"', board_js)
        self.assertIn(".kpi-section", styles_css)
        self.assertIn(".kpi-sub-card", styles_css)
        self.assertIn("info:", icons_js)


if __name__ == "__main__":
    unittest.main(verbosity=2)
