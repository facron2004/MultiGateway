"""Tests for the in-process OAuth provider layer: protocol translation, OAuth
loopback capture, account pool and the gateway routes that serve a provider."""
import json
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
WORK = tempfile.TemporaryDirectory(prefix="multigateway-native-tests-")
ACCOUNTS = Path(WORK.name) / "accounts"
USAGE = Path(WORK.name) / "usage"
ACCOUNTS.mkdir()
USAGE.mkdir()
import os

os.environ["WB_ACCOUNTS_DIR"] = str(ACCOUNTS)
os.environ["WB_PROXY_USAGE_DIR"] = str(USAGE)

from gateway.vendor.workbuddy import wb_proxy as wb, wb_settings
from gateway import handler as handler_module
from gateway import ports as ports_module
from gateway.activity import Activity
from gateway.control import Control
from gateway.handler import GatewayHandler
from gateway.native import NAMES as NATIVE_NAMES
from gateway.native.base import NativeProvider, OpenAIProvider
from gateway.native.oauth import LoopbackCallback, pkce_pair
from gateway.native.pool import AccountPool
from gateway.native.translate import (anthropic_aggregate, anthropic_to_chat,
                                      chat_frames_to_anthropic)
from gateway.ports import Ports
from gateway.transport import ProviderError

CHAT_FRAMES = [
    {"id": "chatcmpl-fake", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "你好"}}]},
    {"id": "chatcmpl-fake", "choices": [{"index": 0, "delta": {"content": "！"}}]},
    {"id": "chatcmpl-fake", "choices": [{"index": 0, "delta": {"reasoning_content": "先想一想"}}]},
    {"id": "chatcmpl-fake", "choices": [{"index": 0, "delta": {"tool_calls": [
        {"index": 0, "id": "call-1", "type": "function",
         "function": {"name": "weather", "arguments": "{\"city\":"}}]}}]},
    {"id": "chatcmpl-fake", "choices": [{"index": 0, "delta": {"tool_calls": [
        {"index": 0, "function": {"arguments": "\"上海\"}"}}]}}]},
    {"id": "chatcmpl-fake", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
    {"id": "chatcmpl-fake", "choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 4,
                                                      "total_tokens": 13,
                                                      "prompt_tokens_details": {"cached_tokens": 2}}},
]


def frames_bytes(frames):
    return b"".join(b"data: " + json.dumps(frame).encode() + b"\n\n" for frame in frames) + b"data: [DONE]\n\n"


class MockUpstream(BaseHTTPRequestHandler):
    requests = []
    reject_tokens = set()

    def log_message(self, *args):
        pass

    def reply(self, value, status=200, content_type="application/json"):
        body = value if isinstance(value, bytes) else json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _token_rejected(self):
        token = (self.headers.get("Authorization") or "").removeprefix("Bearer ")
        return token in type(self).reject_tokens

    def do_GET(self):
        if self._token_rejected():
            return self.reply({"error": {"message": "bad token"}}, 401)
        if self.path == "/v1/models":
            return self.reply({"object": "list", "data": [{"id": "fake-1"}, {"id": "fake-2"}]})
        return self.reply({"error": {"message": "not found"}}, 404)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        body = json.loads(raw or b"{}")
        type(self).requests.append((self.path, body, dict(self.headers)))
        if self._token_rejected():
            return self.reply({"error": {"message": "bad token"}}, 401)
        if self.path == "/v1/chat/completions":
            if body.get("stream"):
                return self.reply(frames_bytes(CHAT_FRAMES), content_type="text/event-stream")
            return self.reply({"id": "chatcmpl-fake", "object": "chat.completion",
                               "choices": [{"index": 0, "message": {"role": "assistant", "content": "你好"},
                                            "finish_reason": "stop"}],
                               "usage": {"prompt_tokens": 9, "completion_tokens": 4}})
        return self.reply({"error": {"message": "not found"}}, 404)


class FakeProvider(OpenAIProvider):
    """A native provider with a local upstream, used to exercise the routes."""

    name = "fake"
    label = "Fake Agent"
    region = "intl"
    chat_path = "/v1/chat/completions"
    models_path = "/v1/models"

    def __init__(self, directory, origin):
        super().__init__(directory)
        self.origin = origin
        self.login_calls = []

    def url(self, path):
        return self.origin + path

    def refresh_grant(self, row):
        return {"access_token": "refreshed-" + row["id"],
                "expires_at": time.time() + 3600}

    def start_login(self, region=None):
        self.login_calls.append(("start", region))
        return {"login_id": "login-1", "url": "https://example.invalid/authorize",
                "interval": 1, "expires_at": time.time() + 300}

    def poll_login(self, login_id):
        self.login_calls.append(("poll", login_id))
        if login_id != "login-1":
            raise ProviderError("授权会话已失效", 400, "expired_token")
        return {"status": "authenticated", "account_id": "imported"}


class LoopbackProvider(NativeProvider):
    """Minimal provider used to exercise the base loopback login path."""

    name = "loopback"
    label = "Loopback"

    def __init__(self, directory, origin):
        super().__init__(directory)
        self.origin = origin

    def url(self, path):
        return self.origin + path

    def fetch_models(self):
        return []

    def refresh_grant(self, row):
        return {"access_token": "x", "expires_at": time.time() + 3600}

    def poll_login(self, login_id):
        return self._loopback_poll(login_id, self._exchange)

    @staticmethod
    def _exchange(query, login):
        return {"id": "loopback-account", "label": "Loopback",
                "access_token": "loopback-token", "refresh_token": "loopback-refresh",
                "expires_at": time.time() + 3600}


class NativeGatewayTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.upstream = ThreadingHTTPServer(("127.0.0.1", 0), MockUpstream)
        cls.upstream.daemon_threads = True
        threading.Thread(target=cls.upstream.serve_forever, daemon=True).start()
        cls.origin = "http://127.0.0.1:%s" % cls.upstream.server_port
        cls.provider = FakeProvider(Path(WORK.name) / "fake", cls.origin)
        cls.provider.pool.save({"id": "acct-1", "label": "测试账号一", "access_token": "good-token",
                                "refresh_token": "refresh-1", "expires_at": time.time() + 3600})
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), GatewayHandler)
        cls.server.daemon_threads = True
        runtime = type("Runtime", (), {})()
        runtime.directory = Path(WORK.name)
        runtime.origin = "http://127.0.0.1:%s" % cls.server.server_port
        runtime.native = {"fake": cls.provider}
        runtime.minimax = None
        runtime.catalog = {}
        runtime.activity = Activity(runtime.directory)
        runtime.control = Control(runtime)
        runtime.status = lambda: {"fake": cls.provider.status()}
        runtime.models = lambda realm: ([{"id": "fake/fake-1", "owned_by": "fake"}], {})
        runtime.provider_models = lambda name: [
            {"id": row, "object": "model"} for row in cls.provider.models()]
        cls.server.gateway = runtime
        runtime.ports = Ports(runtime, "127.0.0.1", cls.server.server_port)
        import socket
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            runtime.ports.config["port"] = reserved.getsockname()[1]
        runtime.ports.start()
        wb.API_KEY = "gateway-test-key"
        wb.PANEL = wb_settings.PanelSessions()
        cls.url = "http://127.0.0.1:%s" % cls.server.server_port
        cls.patches = [
            mock.patch.object(handler_module, "PROVIDERS", handler_module.PROVIDERS | {"fake"}),
            mock.patch.object(handler_module, "PROVIDER_ROUTE",
                              "(?:fake|" + handler_module.PROVIDER_ROUTE + ")"),
            mock.patch.object(ports_module, "NAMES", ports_module.NAMES + ("fake",)),
            mock.patch.object(ports_module, "GROUP_ROUTE",
                              r"/(?:" + "|".join(ports_module.NAMES + ("fake",)) + r")(/.*)"),
        ]
        for patch in cls.patches:
            patch.start()
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        for patch in cls.patches:
            patch.stop()
        cls.server.gateway.ports.stop()
        cls.server.shutdown()
        cls.server.server_close()
        cls.upstream.shutdown()
        cls.upstream.server_close()
        cls.server.gateway.activity.close()
        WORK.cleanup()

    def setUp(self):
        MockUpstream.requests.clear()
        MockUpstream.reject_tokens = set()
        self.provider.catalog = None
        self.provider.login_calls = []
        for path in self.provider.pool.directory.glob("*.json"):
            path.unlink()
        self.provider.pool.save({"id": "acct-1", "label": "测试账号一",
                                 "access_token": "good-token", "refresh_token": "refresh-1",
                                 "expires_at": time.time() + 3600})

    def port_request(self, name, path, body=None):
        origin = "http://127.0.0.1:%s" % self.server.gateway.ports.info(name)["port"]
        payload = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(origin + "/" + name + path, payload,
                                     {"Authorization": "Bearer gateway-test-key",
                                      "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=10) as response:
                return response.status, response.read(), response.headers
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read(), exc.headers

    def request(self, path, body=None, key="gateway-test-key", cookie=None, raw=False):
        headers = {"Content-Type": "application/json"}
        if key:
            headers["Authorization"] = "Bearer " + key
        if cookie:
            headers["Cookie"] = cookie
        payload = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.url + path, payload, headers)
        try:
            response = urllib.request.urlopen(req, timeout=10)
        except urllib.error.HTTPError as exc:
            response = exc
        with response:
            return response.status, response.read(), response.headers

    def cookie(self):
        return "gateway_session=" + wb.PANEL.create()

    # ---------------------------------------------------------------- translation
    def test_anthropic_request_translation(self):
        payload = {
            "model": "fake-1", "max_tokens": 128, "temperature": 0.2,
            "system": "你是助手",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "看图"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                                  "data": "AAAA"}}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "我查一下"},
                    {"type": "tool_use", "id": "toolu-1", "name": "weather",
                     "input": {"city": "上海"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu-1", "content": "晴"},
                    {"type": "text", "text": "继续"}]},
            ],
            "tools": [{"name": "weather", "description": "天气",
                       "input_schema": {"type": "object", "properties": {}}}],
            "tool_choice": {"type": "tool", "name": "weather"},
        }
        result = anthropic_to_chat(payload)
        self.assertEqual(result["model"], "fake-1")
        self.assertEqual(result["max_tokens"], 128)
        self.assertEqual(result["messages"][0], {"role": "system", "content": "你是助手"})
        user = result["messages"][1]
        self.assertEqual(user["role"], "user")
        self.assertEqual(user["content"][0], {"type": "text", "text": "看图"})
        self.assertTrue(user["content"][1]["image_url"]["url"].startswith("data:image/png;base64,"))
        assistant = result["messages"][2]
        self.assertEqual(assistant["content"], "我查一下")
        self.assertEqual(assistant["tool_calls"][0]["function"]["name"], "weather")
        self.assertEqual(assistant["tool_calls"][0]["id"], "toolu-1")
        self.assertEqual(result["messages"][3], {"role": "tool", "tool_call_id": "toolu-1",
                                                 "content": "晴"})
        self.assertEqual(result["messages"][4], {"role": "user", "content": "继续"})
        self.assertEqual(result["tools"][0]["function"]["name"], "weather")
        self.assertEqual(result["tool_choice"], {"type": "function", "function": {"name": "weather"}})

    def test_anthropic_translation_rejects_invalid_input(self):
        with self.assertRaises(ProviderError):
            anthropic_to_chat({"model": "x", "messages": "bad"})
        with self.assertRaises(ProviderError):
            anthropic_to_chat({"model": "x", "messages": [
                {"role": "user", "content": [{"type": "video", "source": "x"}]}]})

    def test_chat_frames_translate_to_anthropic_events(self):
        frames = [frames_bytes(CHAT_FRAMES)]
        raw = b"".join(frames)
        lines = [line for line in raw.split(b"\n\n") if line.strip()]
        emitted = list(chat_frames_to_anthropic(lines, "fake-1"))
        text = b"".join(emitted).decode()
        self.assertIn('"type": "message_start"', text)
        self.assertIn('"type": "thinking_delta"', text)
        self.assertIn('"type": "text_delta"', text)
        self.assertIn("你好", text)
        self.assertIn('"type": "tool_use"', text)
        self.assertIn('"name": "weather"', text)
        self.assertIn('"type": "input_json_delta"', text)
        self.assertIn('"stop_reason": "tool_use"', text)
        self.assertIn('"output_tokens": 4', text)
        self.assertIn('"cache_read_input_tokens": 2', text)
        self.assertIn('"type": "message_stop"', text)

    def test_anthropic_aggregate_builds_message(self):
        lines = [line for line in frames_bytes(CHAT_FRAMES).split(b"\n\n") if line.strip()]
        message = anthropic_aggregate(chat_frames_to_anthropic(lines, "fake-1"), "fake-1")
        self.assertEqual(message["type"], "message")
        self.assertEqual(message["role"], "assistant")
        types = [block["type"] for block in message["content"]]
        self.assertIn("text", types)
        self.assertIn("tool_use", types)
        tool = next(block for block in message["content"] if block["type"] == "tool_use")
        self.assertEqual(tool["input"], {"city": "上海"})
        self.assertEqual(message["stop_reason"], "tool_use")

    # ----------------------------------------------------------------------- oauth
    def test_loopback_callback_captures_code_and_reports_denial(self):
        import socket
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        callback = LoopbackCallback(port, "http://127.0.0.1:%s/callback" % port).start()
        try:
            try:
                urllib.request.urlopen("http://127.0.0.1:%s/callback?code=abc&state=xyz" % port,
                                       timeout=5).read()
            except urllib.error.HTTPError as exc:
                self.fail("callback returned %s" % exc.code)
            self.assertEqual(callback.result["code"], "abc")
            self.assertEqual(callback.result["state"], "xyz")
        finally:
            callback.stop()
        with socket.socket() as probe2:
            probe2.bind(("127.0.0.1", 0))
            port2 = probe2.getsockname()[1]
        denied = LoopbackCallback(port2, "http://127.0.0.1:%s/callback" % port2).start()
        try:
            with self.assertRaises(urllib.error.HTTPError):
                urllib.request.urlopen("http://127.0.0.1:%s/callback?error=access_denied&error_description=nope"
                                       % port2, timeout=5).read()
            self.assertIn("nope", denied.error)
        finally:
            denied.stop()

    def test_loopback_login_completes_and_denial_is_terminal(self):
        import socket
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        provider = LoopbackProvider(Path(WORK.name) / "loop", self.origin)
        started = provider.loopback_start(
            port, "http://127.0.0.1:%s/callback" % port,
            lambda state, challenge: "https://example.invalid/?state=" + state,
            LoopbackProvider._exchange)
        self.assertTrue(started["url"].startswith("https://"))
        self.assertEqual(provider.poll_login(started["login_id"])["status"], "pending")
        urllib.request.urlopen("http://127.0.0.1:%s/callback?code=abc" % port, timeout=5).read()
        result = provider.poll_login(started["login_id"])
        self.assertEqual(result["status"], "authenticated")
        row = provider.pool.find("loopback-account")[1]
        self.assertEqual(row["access_token"], "loopback-token")

        with socket.socket() as probe2:
            probe2.bind(("127.0.0.1", 0))
            port2 = probe2.getsockname()[1]
        denied = provider.loopback_start(
            port2, "http://127.0.0.1:%s/callback" % port2,
            lambda state, challenge: "https://example.invalid/?state=" + state,
            LoopbackProvider._exchange)
        with self.assertRaises(urllib.error.HTTPError):
            urllib.request.urlopen(
                "http://127.0.0.1:%s/callback?error=access_denied&error_description=no" % port2,
                timeout=5).read()
        with self.assertRaises(ProviderError) as raised:
            provider.poll_login(denied["login_id"])
        self.assertEqual(raised.exception.code, "access_denied")

    def test_pkce_pair_is_s256(self):
        import base64
        import hashlib
        verifier, challenge = pkce_pair()
        expected = base64.urlsafe_b64encode(
            hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        self.assertEqual(challenge, expected)
        self.assertTrue(43 <= len(verifier) <= 128)

    # ------------------------------------------------------------------------ pool
    def test_account_pool_rotation_and_mutation(self):
        directory = Path(WORK.name) / "pool"
        provider = type("Owner", (), {"directory": directory, "region": "intl",
                                      "label": "Pool", "catalog": None})()
        pool = AccountPool(provider)
        first = pool.save({"id": "a", "access_token": "t-a", "refresh_token": "r-a"})
        second = pool.save({"id": "b", "access_token": "t-b", "refresh_token": "r-b"})
        self.assertEqual({first, second}, {"a", "b"})
        seen = {pool.choose()["id"] for _ in range(4)}
        self.assertEqual(seen, {"a", "b"})
        pool.mutate("a", False)
        self.assertEqual({pool.choose()["id"] for _ in range(3)}, {"b"})
        self.assertEqual(pool.choose("a")["id"], "a")  # explicit selection ignores the switch
        pool.mutate("a", True)
        pool.mutate("b", delete=True)
        self.assertEqual([row["id"] for _, row in pool.records()], ["a"])

    # ------------------------------------------------------------------- gateway
    def test_native_provider_is_registered_and_served(self):
        self.assertIn("fake", handler_module.PROVIDERS)
        self.assertIn("fake", ports_module.NAMES)
        # every real native integration is wired into the unified group list
        self.assertTrue({"antigravity", "grok"} <= set(NATIVE_NAMES))

    def test_chat_stream_and_non_stream(self):
        status, body, headers = self.request("/v1/chat/completions", {
            "model": "fake/fake-1", "stream": True,
            "messages": [{"role": "user", "content": "你好"}]})
        self.assertEqual(status, 200, body)
        self.assertIn("text/event-stream", headers.get("Content-Type", ""))
        text = body.decode()
        self.assertIn("你好", text)
        self.assertIn("[DONE]", text)
        path, sent, upstream_headers = MockUpstream.requests[-1]
        self.assertEqual(path, "/v1/chat/completions")
        self.assertTrue(sent["stream"])
        self.assertEqual(sent["stream_options"], {"include_usage": True})
        self.assertEqual(sent["model"], "fake-1")
        self.assertNotIn("fake/", json.dumps(sent))
        self.assertEqual(upstream_headers.get("Authorization"), "Bearer good-token")
        record = self.server.gateway.activity.view("fake")["records"][0]
        self.assertEqual(record["account"], "acct-1")

        status, body, _ = self.request("/v1/chat/completions", {
            "model": "fake/fake-1", "messages": [{"role": "user", "content": "你好"}]})
        self.assertEqual(status, 200, body)
        result = json.loads(body)
        self.assertEqual(result["choices"][0]["message"]["content"], "你好！")
        self.assertEqual(result["usage"]["prompt_tokens"], 9)

    def test_responses_endpoint_streams_and_aggregates(self):
        status, body, headers = self.request("/v1/responses", {
            "model": "fake/fake-1", "input": "你好", "stream": True})
        self.assertEqual(status, 200, body)
        text = body.decode()
        self.assertIn("response.output_text.delta", text)
        self.assertIn("response.completed", text)

        status, body, _ = self.request("/v1/responses", {
            "model": "fake/fake-1", "input": "你好"})
        self.assertEqual(status, 200, body)
        result = json.loads(body)
        self.assertEqual(result["object"], "response")
        self.assertIn("你好", result["output_text"])

    def test_messages_endpoint_streams_and_aggregates(self):
        anthropic = {"model": "fake/fake-1", "max_tokens": 64, "stream": True,
                     "messages": [{"role": "user", "content": "你好"}]}
        status, body, headers = self.request("/v1/messages", anthropic)
        self.assertEqual(status, 200, body)
        self.assertIn("text/event-stream", headers.get("Content-Type", ""))
        text = body.decode()
        self.assertIn("message_start", text)
        self.assertIn("text_delta", text)
        self.assertIn("message_stop", text)
        conversation = MockUpstream.requests[-1][1]
        self.assertNotIn("system", conversation)
        self.assertEqual(conversation["messages"][0]["content"], "你好")

        anthropic.pop("stream")
        status, body, _ = self.request("/v1/messages", anthropic)
        self.assertEqual(status, 200, body)
        result = json.loads(body)
        self.assertEqual(result["type"], "message")
        self.assertEqual(result["content"][0]["text"], "你好！")

    def test_model_catalog_and_snapshot_contract(self):
        status, body, _ = self.port_request("fake", "/v1/models")
        self.assertEqual(status, 200, body)
        ids = {row["id"] for row in json.loads(body)["data"]}
        self.assertEqual(ids, {"fake-1", "fake-2"})

        status, body, _ = self.request("/gateway/providers/fake/snapshot", cookie=self.cookie())
        self.assertEqual(status, 200, body)
        snapshot = json.loads(body)
        self.assertEqual(snapshot["provider"], "fake")
        self.assertEqual(snapshot["region"], "intl")
        self.assertEqual(snapshot["accounts"][0]["id"], "acct-1")
        self.assertEqual(snapshot["accounts"][0]["name"], "测试账号一")
        self.assertFalse(snapshot["capabilities"]["regions"])
        self.assertTrue(snapshot["capabilities"]["login"])
        self.assertEqual(snapshot["pool"]["total"], 1)
        self.assertEqual(snapshot["gateway"]["port"], self.server.gateway.ports.config["port"])
        self.assertNotIn("good-token", body.decode())
        self.assertNotIn("refresh-1", body.decode())

    def test_quota_refresh_route_batch_and_snapshot_keep_account_scope(self):
        quota = {"remaining": 42, "unit": "%", "label": "本周额度", "buckets": [
            {"label": "本周额度", "remaining": 42, "unit": "%"}]}
        with mock.patch.object(self.provider, "capabilities", dict(self.provider.capabilities, credits=True)), \
                mock.patch.object(self.provider, "fetch_quota", return_value=quota):
            cookie = self.cookie()
            status, body, _ = self.request("/gateway/providers/fake/credits", {"id": "acct-1"},
                                           cookie=cookie)
            self.assertEqual(status, 200, body)
            self.assertEqual(json.loads(body)["quota"]["remaining"], 42)
            status, body, _ = self.request("/gateway/providers/fake/batch",
                                           {"operation": "credits", "ids": ["acct-1"]}, cookie=cookie)
            self.assertEqual(status, 200, body)
            self.assertEqual(json.loads(body)["succeeded"], 1)
            status, body, _ = self.request("/gateway/providers/fake/snapshot", cookie=cookie)
            self.assertEqual(status, 200, body)
            account = json.loads(body)["accounts"][0]
            self.assertEqual(account["remaining"], 42)
            self.assertEqual(account["quota"]["unit"], "%")
            self.assertNotIn("good-token", body.decode())

    def test_account_snapshot_reports_tokens_from_all_public_protocols(self):
        runtime = self.server.gateway
        with tempfile.TemporaryDirectory() as directory, closing(Activity(Path(directory))) as activity:
            with mock.patch.object(runtime, "activity", activity):
                control = Control(runtime)
                with mock.patch.object(runtime, "control", control):
                    cookie = self.cookie()
                    status, body, _ = self.request("/gateway/providers/fake/snapshot", cookie=cookie)
                    self.assertEqual(status, 200, body)
                    self.assertEqual(json.loads(body)["accounts"][0]["daily_tokens"], 0)
                    for index, (path, payload) in enumerate([
                        ("/v1/chat/completions", {"messages": [{"role": "user", "content": "hi"}]}),
                        ("/v1/responses", {"input": "hi"}),
                        ("/v1/messages", {"messages": [{"role": "user", "content": "hi"}], "max_tokens": 16}),
                    ]):
                        for streamed in (False, True):
                            with self.subTest(path=path, stream=streamed):
                                status, body, _ = self.request(path, dict(payload, model="fake/fake-1", stream=streamed))
                                self.assertEqual(status, 200, body)
                                expected_requests = index * 2 + int(streamed) + 1
                                deadline = time.monotonic() + 2
                                while len(activity.records()) < expected_requests and time.monotonic() < deadline:
                                    time.sleep(0.01)
                                status, body, _ = self.request("/gateway/providers/fake/snapshot", cookie=cookie)
                                account = json.loads(body)["accounts"][0]
                                self.assertEqual(account["daily_tokens"], 13 * expected_requests)
                                self.assertTrue(account["daily_tokens_complete"])
                    with closing(Activity(Path(directory))) as restored, mock.patch.object(runtime, "activity", restored):
                        snapshot = Control(runtime).snapshot("fake")
                        self.assertEqual(snapshot["accounts"][0]["daily_tokens"], 78)

    def test_management_actions_reach_the_provider(self):
        status, body, _ = self.request("/gateway/providers/fake/login", {"region": "intl"},
                                       cookie=self.cookie())
        self.assertEqual(status, 200, body)
        self.assertEqual(json.loads(body)["url"], "https://example.invalid/authorize")
        self.assertEqual(self.provider.login_calls[-1][0], "start")

        status, body, _ = self.request("/gateway/providers/fake/poll", {"login_id": "login-1"},
                                       cookie=self.cookie())
        self.assertEqual(status, 200, body)
        self.assertEqual(json.loads(body)["status"], "authenticated")

        status, body, _ = self.request("/gateway/providers/fake/enable",
                                       {"id": "acct-1", "enabled": False}, cookie=self.cookie())
        self.assertEqual(status, 200, body)
        self.assertFalse(self.provider.pool.find("acct-1")[1]["enabled"])
        self.request("/gateway/providers/fake/enable", {"id": "acct-1", "enabled": True},
                     cookie=self.cookie())

        status, body, _ = self.request("/gateway/providers/fake/import",
                                       {"data": {"access_token": "imported-token",
                                                 "refresh_token": "imported-refresh",
                                                 "email": "user@example.com"}},
                                       cookie=self.cookie())
        self.assertEqual(status, 200, body)
        self.assertEqual(json.loads(body)["count"], 1)
        self.assertEqual(len(self.provider.pool.records()), 2)

        status, body, _ = self.request("/gateway/providers/fake/credentials", {"id": "acct-1"},
                                       cookie=self.cookie())
        self.assertEqual(status, 200, body)
        self.assertTrue(json.loads(body)["ok"])

        status, body, _ = self.request("/gateway/providers/fake/delete", {"id": "acct-1"},
                                       cookie=self.cookie())
        self.assertEqual(status, 200, body)
        self.assertEqual([row["id"] for _, row in self.provider.pool.records()], ["user@example.com"])

    def test_expired_credentials_refresh_before_use(self):
        self.provider.pool.save({"id": "acct-expired", "label": "过期",
                                 "access_token": "stale", "refresh_token": "refresh-2",
                                 "expires_at": time.time() - 10, "enabled": False})
        status, body, _ = self.request("/gateway/providers/fake/credentials", {"id": "acct-expired"},
                                       cookie=self.cookie())
        self.assertEqual(status, 200, body)
        row = self.provider.pool.find("acct-expired")[1]
        self.assertEqual(row["access_token"], "refreshed-acct-expired")
        self.assertEqual(row["refresh_token"], "refresh-2")

    def test_auth_failure_rotates_to_next_account(self):
        self.provider.pool.save({"id": "acct-bad", "label": "坏账号",
                                 "access_token": "bad-token", "refresh_token": "refresh-3",
                                 "expires_at": time.time() + 3600})
        MockUpstream.reject_tokens = {"bad-token"}
        for _ in range(4):
            status, body, _ = self.request("/v1/chat/completions", {
                "model": "fake/fake-1", "messages": [{"role": "user", "content": "你好"}]})
            self.assertEqual(status, 200, body)
        self.assertTrue(MockUpstream.requests)
        self.assertFalse((self.provider.pool.find("acct-1")[1].get("error")))

    def test_group_rejects_models_from_another_provider(self):
        status, body, _ = self.port_request("fake", "/v1/chat/completions",
                                           {"model": "grok/whatever", "messages": []})
        self.assertEqual(status, 400, body)
        status, body, _ = self.port_request("fake", "/v1/chat/completions",
                                           {"model": "fake/fake-1",
                                            "messages": [{"role": "user", "content": "你好"}]})
        self.assertEqual(status, 200, body)


class TranslationRegressionTests(unittest.TestCase):
    """The native layer must keep the gateway's Anthropic surface compatible."""

    def test_anthropic_cycle_preserves_tools_and_stop(self):
        from gateway.protocols import chat_to_anthropic
        chat = {"model": "m", "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 32,
                "tools": [{"type": "function", "function": {
                    "name": "read", "parameters": {"type": "object", "properties": {}}}}],
                "tool_choice": "required"}
        anthropic = chat_to_anthropic(chat)
        back = anthropic_to_chat(anthropic)
        self.assertEqual(back["tools"][0]["function"]["name"], "read")
        self.assertEqual(back["tool_choice"], "required")
        self.assertEqual(back["max_tokens"], 32)

    def test_error_frame_from_upstream_becomes_provider_error(self):
        frames = [b'data: {"error":{"message":"quota exceeded"}}\n\n']
        with self.assertRaises(ProviderError):
            list(chat_frames_to_anthropic(frames, "m"))


if __name__ == "__main__":
    unittest.main()
