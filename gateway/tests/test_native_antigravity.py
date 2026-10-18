"""Unit tests for the Google Antigravity provider.

A local ``ThreadingHTTPServer`` stands in for Google's token/userinfo endpoints
and for Cloud Code Assist, so nothing here touches the real network. The
module-level base URLs are patched per test; the callback listener runs on a
free port chosen by the test instead of the registered 51121.
"""
import json
import socket
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

from gateway.native import antigravity
from gateway.transport import ProviderError


class MockCloudCode(BaseHTTPRequestHandler):
    """Minimal Google + Cloud Code Assist stand-in."""

    protocol_version = "HTTP/1.1"

    requests = []
    token_response = {}
    userinfo_response = {}
    load_response = {}
    onboard_response = {}
    models_response = {}
    models_status = 200
    stream_frames = []

    def log_message(self, *args):
        pass

    def reply(self, value, status=200, content_type="application/json"):
        body = value if isinstance(value, bytes) else json.dumps(value).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def record(self, body):
        type(self).requests.append((self.path, body, dict(self.headers)))

    def do_GET(self):
        self.record(None)
        if self.path.startswith("/userinfo"):
            return self.reply(type(self).userinfo_response)
        return self.reply({"error": {"message": "not found"}}, 404)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        path = urllib.parse.urlsplit(self.path).path
        if path == "/token":
            form = {key: values[0] for key, values in
                    urllib.parse.parse_qs(raw.decode("utf-8")).items()}
            self.record(form)
            return self.reply(type(self).token_response)
        body = json.loads(raw or b"{}")
        self.record(body)
        if path.endswith(":loadCodeAssist"):
            return self.reply(type(self).load_response)
        if path.endswith(":onboardUser"):
            return self.reply(type(self).onboard_response)
        if path.endswith(":fetchAvailableModels"):
            status = type(self).models_status
            if status != 200:
                return self.reply({"error": {"message": "catalog unavailable"}}, status)
            return self.reply(type(self).models_response)
        if path.endswith(":streamGenerateContent"):
            body = b"".join(b"data: " + json.dumps(frame).encode("utf-8") + b"\n\n"
                            for frame in type(self).stream_frames)
            return self.reply(body, content_type="text/event-stream")
        return self.reply({"error": {"message": "not found"}}, 404)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wrapped(response):
    return {"response": response, "traceId": "trace"}


def chunks_of(frames):
    """Decode provider frames, keeping the terminal sentinel as ``[DONE]``."""
    chunks = []
    for frame in frames:
        text = frame.decode("utf-8")
        assert text.startswith("data: "), text
        data = text[6:].strip()
        chunks.append("[DONE]" if data == "[DONE]" else json.loads(data))
    return chunks


def header_of(headers, name):
    """Case-insensitive header lookup (urllib normalizes header key casing)."""
    for key, value in headers.items():
        if key.lower() == name.lower():
            return value
    raise AssertionError("missing header %r in %r" % (name, sorted(headers)))


def text_of(chunks):
    return "".join(chunk["choices"][0]["delta"].get("content") or ""
                   for chunk in chunks if chunk != "[DONE]" and chunk["choices"])


def field_of(chunks, key):
    for chunk in chunks:
        if chunk == "[DONE]" or not chunk["choices"]:
            continue
        value = chunk["choices"][0]["delta"].get(key)
        if value:
            return value
    return None


class AntigravityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), MockCloudCode)
        cls.server.daemon_threads = True
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.origin = "http://127.0.0.1:%d" % cls.server.server_port

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        self.work = tempfile.TemporaryDirectory(prefix="antigravity-tests-")
        self.provider = antigravity.Antigravity(Path(self.work.name))
        MockCloudCode.requests = []
        MockCloudCode.token_response = {"access_token": "access-1",
                                        "refresh_token": "refresh-1",
                                        "expires_in": 3599, "token_type": "Bearer"}
        MockCloudCode.userinfo_response = {"sub": "google-sub", "email": "user@example.com",
                                           "name": "Test User"}
        MockCloudCode.load_response = {"cloudaicompanionProject": "server-project",
                                       "currentTier": {"id": "free-tier"}}
        MockCloudCode.onboard_response = {"done": True, "response": {
            "cloudaicompanionProject": {"id": "onboarded-project"}}}
        MockCloudCode.models_response = {"models": {
            "gemini-3.8-flash-high": {"displayName": "Gemini 3.8 Flash (High)",
                                      "maxTokens": 1048576, "maxOutputTokens": 65536}}}
        MockCloudCode.models_status = 200
        MockCloudCode.stream_frames = []
        self.patches = [
            mock.patch.object(antigravity, "HOST_PROD", self.origin),
            mock.patch.object(antigravity, "HOST_DAILY", self.origin),
            mock.patch.object(antigravity, "HOST_SANDBOX", self.origin),
            mock.patch.object(antigravity, "TOKEN_URL", self.origin + "/token"),
            mock.patch.object(antigravity, "USERINFO_URL", self.origin + "/userinfo"),
            mock.patch.object(antigravity, "ONBOARD_INTERVAL", 0),
        ]
        for patcher in self.patches:
            patcher.start()

    def tearDown(self):
        self.provider.stop()
        for patcher in reversed(self.patches):
            patcher.stop()
        self.work.cleanup()

    # ------------------------------------------------------------------ helpers
    def add_account(self, **overrides):
        record = {"id": "acct-1", "access_token": "access-1", "refresh_token": "refresh-1",
                  "expires_at": time.time() + 3600, "email": "user@example.com",
                  "label": "Test User", "extra": {}}
        record.update(overrides)
        self.provider.pool.save(record)
        return record

    def calls(self, *fragments):
        return [entry for entry in MockCloudCode.requests
                if any(fragment in entry[0] for fragment in fragments)]

    def envelope_calls(self):
        return [entry for entry in self.calls(":streamGenerateContent")]

    def hit_callback(self, port, query, path="/oauth-callback"):
        url = "http://127.0.0.1:%d%s?%s" % (port, path, urllib.parse.urlencode(query))
        try:
            with urllib.request.urlopen(url, timeout=5) as response:
                return response.status
        except urllib.error.HTTPError as exc:
            with exc:
                return exc.code

    def start_login(self):
        port = free_port()
        patcher = mock.patch.object(antigravity, "CALLBACK_PORT", port)
        patcher.start()
        self.addCleanup(patcher.stop)
        return port, self.provider.start_login()

    # -------------------------------------------------------------------- login
    def test_login_start_and_poll_stores_account(self):
        port, start = self.start_login()
        self.assertEqual(set(start), {"login_id", "url", "interval", "expires_at"})
        self.assertTrue(start["url"].startswith("https://"))
        query = urllib.parse.parse_qs(urllib.parse.urlsplit(start["url"]).query)
        self.assertEqual(query["client_id"], [antigravity.CLIENT_ID])
        self.assertEqual(query["response_type"], ["code"])
        self.assertEqual(query["access_type"], ["offline"])
        self.assertEqual(query["prompt"], ["consent"])
        self.assertEqual(query["redirect_uri"],
                         ["http://localhost:%d/oauth-callback" % port])
        self.assertEqual(len(query["scope"][0].split(" ")), len(antigravity.SCOPES))
        self.assertNotIn("code_challenge", query)

        self.assertEqual(self.hit_callback(port, {"code": "auth-code", "state": "x"}), 200)
        result = self.provider.poll_login(start["login_id"])
        self.assertEqual(result["status"], "authenticated")
        self.assertEqual(result["account_id"], "google-sub")

        _, row = self.provider.pool.records()[0]
        self.assertEqual(row["email"], "user@example.com")
        self.assertEqual(row["label"], "Test User")
        self.assertEqual(row["access_token"], "access-1")
        self.assertEqual(row["refresh_token"], "refresh-1")
        self.assertGreater(row["expires_at"], time.time() + 3000)

        exchange = self.calls("/token")[-1][1]
        self.assertEqual(exchange["grant_type"], "authorization_code")
        self.assertEqual(exchange["code"], "auth-code")
        self.assertEqual(exchange["client_secret"], antigravity.CLIENT_SECRET)
        self.assertEqual(exchange["redirect_uri"], "http://localhost:%d/oauth-callback" % port)
        profile = self.calls("/userinfo")[-1]
        self.assertEqual(header_of(profile[2], "Authorization"), "Bearer access-1")

    def test_login_denied_is_terminal(self):
        port, start = self.start_login()
        self.assertEqual(self.hit_callback(port, {"error": "access_denied",
                                                  "error_description": "user denied"}), 400)
        with self.assertRaises(ProviderError) as caught:
            self.provider.poll_login(start["login_id"])
        self.assertEqual(caught.exception.code, "access_denied")
        self.assertIn("授权", str(caught.exception))
        self.assertEqual(self.provider.pending, {})

    def test_login_expiry_switches_callback_path(self):
        with mock.patch.object(antigravity, "LOGIN_TIMEOUT", 0.05):
            _, start = self.start_login()
            time.sleep(0.1)
            with self.assertRaises(ProviderError) as caught:
                self.provider.poll_login(start["login_id"])
        self.assertEqual(caught.exception.code, "expired_token")
        self.assertEqual(self.provider.load_config()["redirect_path_index"], 1)

        port, retry = self.start_login()
        query = urllib.parse.parse_qs(urllib.parse.urlsplit(retry["url"]).query)
        self.assertEqual(query["redirect_uri"],
                         ["http://localhost:%d/oauthcallback" % port])

    # ------------------------------------------------------------------ refresh
    def test_refresh_preserves_refresh_token_and_extra(self):
        self.add_account(expires_at=time.time() - 10, extra={"project_id": "cached-project"})
        MockCloudCode.token_response = {"access_token": "access-2", "expires_in": 3599}
        row = self.provider._access()
        self.assertEqual(row["access_token"], "access-2")
        self.assertEqual(row["refresh_token"], "refresh-1")
        self.assertGreater(row["expires_at"], time.time() + 3000)
        self.assertEqual(row["extra"]["project_id"], "cached-project")

        form = self.calls("/token")[-1][1]
        self.assertEqual(form["grant_type"], "refresh_token")
        self.assertEqual(form["refresh_token"], "refresh-1")
        self.assertEqual(form["client_id"], antigravity.CLIENT_ID)
        self.assertEqual(form["client_secret"], antigravity.CLIENT_SECRET)

    def test_refresh_requires_refresh_token(self):
        with self.assertRaises(ProviderError) as caught:
            self.provider.refresh_grant({"id": "acct-1", "refresh_token": ""})
        self.assertIn("刷新令牌", str(caught.exception))

    # ------------------------------------------------------------------ project
    def test_project_is_resolved_once_and_cached(self):
        self.add_account()
        MockCloudCode.stream_frames = [wrapped({"candidates": [{
            "content": {"role": "model", "parts": [{"text": "ok"}]},
            "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 1,
                                                        "candidatesTokenCount": 1}})]
        payload = {"model": "gemini-3-pro-high",
                   "messages": [{"role": "user", "content": "hi"}]}
        list(self.provider.chat(payload))
        list(self.provider.chat(payload))
        self.assertEqual(len(self.calls(":loadCodeAssist")), 1)
        envelopes = [entry[1] for entry in self.envelope_calls()]
        self.assertEqual(envelopes[0]["project"], "server-project")
        self.assertEqual(envelopes[1]["project"], "server-project")
        _, row = self.provider.pool.records()[0]
        self.assertEqual(row["extra"]["project_id"], "server-project")

    def test_project_onboarded_when_missing_and_cached(self):
        self.add_account()
        MockCloudCode.load_response = {"currentTier": {"id": "free-tier"},
                                       "allowedTiers": [{"id": "free-tier", "isDefault": True}]}
        MockCloudCode.stream_frames = [wrapped({"candidates": [{
            "content": {"role": "model", "parts": [{"text": "ok"}]},
            "finishReason": "STOP"}]})]
        payload = {"model": "gemini-3-pro-high",
                   "messages": [{"role": "user", "content": "hi"}]}
        list(self.provider.chat(payload))
        self.assertEqual(len(self.calls(":onboardUser")), 1)
        self.assertEqual(self.calls(":onboardUser")[0][1]["tierId"], "free-tier")
        self.assertEqual(self.envelope_calls()[0][1]["project"], "onboarded-project")
        list(self.provider.chat(payload))
        self.assertEqual(len(self.calls(":onboardUser")), 1)
        self.assertEqual(len(self.calls(":loadCodeAssist")), 1)

    # --------------------------------------------------------------------- chat
    def test_chat_text_frames_and_envelope(self):
        self.add_account()
        MockCloudCode.stream_frames = [
            wrapped({"candidates": [{"content": {"role": "model",
                                                 "parts": [{"text": "Hello"}]}}],
                     "usageMetadata": {"promptTokenCount": 16, "candidatesTokenCount": 1}}),
            wrapped({"candidates": [{"content": {"role": "model",
                                                 "parts": [{"text": " world"}]},
                                     "finishReason": "STOP"}],
                     "usageMetadata": {"promptTokenCount": 16, "candidatesTokenCount": 4,
                                       "totalTokenCount": 20}}),
        ]
        payload = {"model": "gemini-3.8-flash-high",
                   "messages": [{"role": "system", "content": "be brief"},
                                {"role": "user", "content": "hi"}]}
        frames = self.provider.chat(payload, session="gateway-session")
        # Lazy: nothing was sent until the first frame is pulled.
        self.assertEqual(MockCloudCode.requests, [])
        chunks = chunks_of(list(frames))

        self.assertEqual(chunks[-1], "[DONE]")
        self.assertEqual(text_of(chunks), "Hello world")
        first = chunks[0]
        self.assertTrue(first["id"].startswith("chatcmpl-"))
        self.assertEqual(first["object"], "chat.completion.chunk")
        self.assertEqual(first["model"], "gemini-3.8-flash-high")
        self.assertIsInstance(first["created"], int)

        finish = next(chunk for chunk in chunks if chunk != "[DONE]" and chunk["choices"]
                      and chunk["choices"][0]["finish_reason"])
        self.assertEqual(finish["choices"][0]["finish_reason"], "stop")
        usage = next(chunk for chunk in chunks if chunk != "[DONE]" and not chunk["choices"])
        self.assertEqual(usage["usage"], {"prompt_tokens": 16, "completion_tokens": 4,
                                          "total_tokens": 20})

        path, body, headers = self.envelope_calls()[-1]
        self.assertIn("?alt=sse", path)
        self.assertIn("antigravity/", header_of(headers, "User-Agent"))
        self.assertEqual(header_of(headers, "Authorization"), "Bearer access-1")
        self.assertEqual(header_of(headers, "Accept"), "text/event-stream")
        self.assertEqual(list(body), ["project", "requestId", "request", "model",
                                      "userAgent", "requestType"])
        self.assertTrue(body["requestId"].startswith("agent/"))
        self.assertEqual(body["model"], "gemini-3.8-flash-high")
        self.assertEqual(body["userAgent"], "antigravity")
        self.assertEqual(body["requestType"], "agent")
        request = body["request"]
        self.assertEqual(request["systemInstruction"], {"parts": [{"text": "be brief"}]})
        self.assertEqual(request["contents"], [{"role": "user", "parts": [{"text": "hi"}]}])
        self.assertTrue(request["sessionId"].startswith("-"))
        self.assertEqual(request["generationConfig"]["maxOutputTokens"], 65536)

    def test_chat_reasoning_and_tool_calls(self):
        self.add_account()
        MockCloudCode.stream_frames = [wrapped({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"text": "I should check the weather", "thought": True},
                    {"text": "Checking."},
                    {"functionCall": {"name": "get_weather", "args": {"city": "Shanghai"},
                                      "id": "call-1"}},
                ]},
                "finishReason": "STOP",
            }],
            "usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 6,
                              "totalTokenCount": 14}})]
        chunks = chunks_of(list(self.provider.chat({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "weather?"}],
            "tools": [{"type": "function", "function": {
                "name": "get_weather",
                "parameters": {"type": "object", "properties": {
                    "city": {"type": "string"}}}}}]})))

        self.assertEqual(field_of(chunks, "reasoning_content"), "I should check the weather")
        self.assertEqual(text_of(chunks), "Checking.")
        tool_call = field_of(chunks, "tool_calls")[0]
        self.assertEqual(tool_call["index"], 0)
        self.assertEqual(tool_call["id"], "call-1")
        self.assertEqual(tool_call["function"]["name"], "get_weather")
        self.assertEqual(json.loads(tool_call["function"]["arguments"]), {"city": "Shanghai"})
        finish = next(chunk for chunk in chunks if chunk != "[DONE]" and chunk["choices"]
                      and chunk["choices"][0]["finish_reason"])
        self.assertEqual(finish["choices"][0]["finish_reason"], "tool_calls")

        path, body, headers = self.envelope_calls()[-1]
        self.assertEqual(header_of(headers, "anthropic-beta"), antigravity.ANTHROPIC_BETA)
        self.assertEqual(body["request"]["toolConfig"],
                         {"functionCallingConfig": {"mode": "VALIDATED"}})
        declaration = body["request"]["tools"][0]["functionDeclarations"][0]
        self.assertEqual(declaration["name"], "get_weather")

    def test_chat_usage_reports_cached_and_reasoning_tokens(self):
        self.add_account()
        MockCloudCode.stream_frames = [
            wrapped({"candidates": [{"content": {"role": "model", "parts": [{"text": "done"}]},
                                     "finishReason": "STOP"}],
                     "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 20,
                                       "totalTokenCount": 150, "thoughtsTokenCount": 30,
                                       "cachedContentTokenCount": 40}}),
            # A trailing chunk with an empty usage block must not wipe the totals.
            wrapped({"usageMetadata": {}}),
        ]
        chunks = chunks_of(list(self.provider.chat({
            "model": "gemini-3-flash", "messages": [{"role": "user", "content": "hi"}]})))
        usage = next(chunk for chunk in chunks if chunk != "[DONE]" and not chunk["choices"])
        self.assertEqual(usage["usage"]["prompt_tokens"], 100)
        self.assertEqual(usage["usage"]["total_tokens"], 150)
        self.assertEqual(usage["usage"]["prompt_tokens_details"], {"cached_tokens": 40})
        self.assertEqual(usage["usage"]["completion_tokens_details"], {"reasoning_tokens": 30})

    def test_chat_translates_tools_and_thinking_constraints(self):
        self.add_account()
        MockCloudCode.stream_frames = [wrapped({"candidates": [{
            "content": {"role": "model", "parts": [{"text": "ok"}]}, "finishReason": "STOP"}]})]
        payload = {
            "model": "gemini-3.1-pro-high",
            "max_tokens": 1024,
            "reasoning_effort": "high",
            "temperature": 0.2,
            "stop": "END",
            "messages": [
                {"role": "system", "content": "be nice"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "tool_calls": [{"id": "call-9", "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\": \"x\"}"}}]},
                {"role": "tool", "tool_call_id": "call-9", "content": "{\"value\": 7}"},
            ],
            "tools": [{"type": "function", "function": {
                "name": "lookup",
                "parameters": {"$schema": "https://json-schema.org/draft/2020-12/schema",
                               "type": "object", "default": {},
                               "properties": {"q": {"const": "x", "type": "string"},
                                              "n": {"$ref": "#/$defs/count"}}}}}],
        }
        list(self.provider.chat(payload))
        request = self.envelope_calls()[-1][1]["request"]

        self.assertEqual(request["systemInstruction"], {"parts": [{"text": "be nice"}]})
        self.assertEqual(request["contents"][1]["role"], "model")
        self.assertEqual(request["contents"][1]["parts"][0]["functionCall"],
                         {"name": "lookup", "args": {"q": "x"}, "id": "call-9"})
        self.assertEqual(request["contents"][2]["role"], "model")
        self.assertEqual(request["contents"][2]["parts"][0]["functionResponse"],
                         {"name": "lookup", "response": {"value": 7}})

        parameters = request["tools"][0]["functionDeclarations"][0]["parameters"]
        self.assertNotIn("$schema", parameters)
        self.assertNotIn("default", parameters)
        self.assertEqual(parameters["properties"]["q"], {"enum": ["x"], "type": "string"})
        self.assertNotIn("$ref", parameters["properties"]["n"])

        config = request["generationConfig"]
        self.assertEqual(config["stopSequences"], ["END"])
        self.assertEqual(config["temperature"], 0.2)
        self.assertEqual(config["thinkingConfig"], {"thinkingBudget": 16384,
                                                    "includeThoughts": True})
        self.assertGreater(config["maxOutputTokens"],
                           config["thinkingConfig"]["thinkingBudget"])

    def test_chat_without_account_reports_chinese_error(self):
        with self.assertRaises(ProviderError) as caught:
            list(self.provider.chat({"model": "gemini-3-flash",
                                     "messages": [{"role": "user", "content": "hi"}]}))
        self.assertIn("账号", str(caught.exception))

    def test_chat_stream_error_becomes_provider_error(self):
        self.add_account()
        MockCloudCode.stream_frames = [wrapped({"error": {"code": 429,
                                                          "message": "quota exhausted"}})]
        with self.assertRaises(ProviderError) as caught:
            list(self.provider.chat({"model": "gemini-3-flash",
                                     "messages": [{"role": "user", "content": "hi"}]}))
        self.assertIn("quota exhausted", str(caught.exception))
        self.assertIn("Antigravity", str(caught.exception))

    # ------------------------------------------------------------------- models
    def test_fetch_models_uses_upstream_catalog(self):
        self.add_account()
        rows = self.provider.fetch_models()
        self.assertEqual(rows, [{"id": "gemini-3.8-flash-high", "object": "model",
                                 "display_name": "Gemini 3.8 Flash (High)",
                                 "context_window": 1048576, "max_output_tokens": 65536}])
        self.assertEqual(self.calls(":fetchAvailableModels")[-1][1],
                         {"project": "server-project"})

    def test_fetch_models_falls_back_to_static_list(self):
        self.add_account()
        MockCloudCode.models_status = 500
        rows = self.provider.fetch_models()
        identifiers = [row["id"] for row in rows]
        self.assertIn("gemini-3.1-pro-high", identifiers)
        self.assertIn("claude-sonnet-4-6", identifiers)

        empty = antigravity.Antigravity(Path(self.work.name) / "empty")
        self.assertEqual([row["id"] for row in empty.fetch_models()],
                         [row["id"] for row in antigravity.DEFAULT_MODELS])

    # ------------------------------------------------------------------- messages
    def test_messages_translates_anthropic_frames(self):
        self.add_account()
        MockCloudCode.stream_frames = [wrapped({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "你好"}]},
                            "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2,
                              "totalTokenCount": 5}})]
        frames = list(self.provider.messages({
            "model": "gemini-3-flash", "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]}))
        text = b"".join(frames).decode("utf-8")
        self.assertIn("event: message_start", text)
        self.assertIn("event: content_block_delta", text)
        self.assertIn("你好", text)
        self.assertIn("event: message_stop", text)


if __name__ == "__main__":
    unittest.main()
