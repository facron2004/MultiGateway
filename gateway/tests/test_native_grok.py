"""Unit tests for the xAI Grok OAuth provider.

Everything runs against a local ``ThreadingHTTPServer`` that speaks the xAI OAuth
token endpoint and the Responses SSE stream; no real network and no real
credentials are involved. Module-level endpoint constants are patched so the
provider cannot reach the internet even by accident.
"""
import base64
import hashlib
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
import uuid
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from gateway.native import grok
from gateway.native.grok_responses import chat_to_responses, responses_frames
from gateway.transport import ProviderError

CREATED = {"type": "response.created",
           "response": {"id": "resp_1", "model": "grok-4.7", "created_at": 1700000000}}
USAGE = {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}


def b64(value):
    return base64.urlsafe_b64encode(json.dumps(value).encode("utf-8")).decode("ascii").rstrip("=")


def jwt(claims):
    return b64({"alg": "ES256", "typ": "JWT"}) + "." + b64(claims) + ".signature"


def token_grant(email="grok@example.com", tier=2, refresh="refresh-1", expires_in=3600,
                exp_delta=3600, include_refresh=True):
    claims = {"sub": "user-1", "email": email, "tier": tier, "exp": int(time.time()) + exp_delta}
    grant = {"access_token": jwt(claims), "id_token": jwt({"sub": "user-1", "email": email}),
             "token_type": "Bearer", "expires_in": expires_in, "scope": grok.SCOPES}
    if include_refresh:
        grant["refresh_token"] = refresh
    return grant


def text_events():
    return [CREATED,
            {"type": "response.output_text.delta", "output_index": 0, "delta": "你好"},
            {"type": "response.completed",
             "response": {"id": "resp_1", "model": "grok-4.7", "status": "completed",
                          "usage": dict(USAGE)}}]


def sse(events):
    return b"".join(b"data: " + json.dumps(event, ensure_ascii=False).encode("utf-8") + b"\n\n"
                    for event in events) + b"data: [DONE]\n\n"


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def header_map(message):
    return {key.lower(): value for key, value in message.items()}


class MockXai(BaseHTTPRequestHandler):
    """Local xAI stand-in: token endpoint plus streaming Responses endpoint."""

    protocol_version = "HTTP/1.1"
    requests = []
    token_grants = {}
    errors = {}
    events = []
    models = ["grok-4.7", "grok-4.6"]

    def log_message(self, *args):
        pass

    def reply(self, payload, status=200, content_type="application/json"):
        body = payload if isinstance(payload, bytes) else json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def record(self, body):
        path = urllib.parse.urlsplit(self.path).path
        type(self).requests.append((path, body, header_map(self.headers)))
        return path

    def failure_for(self, path):
        """Return the next scripted failure for a path, or None."""
        script = type(self).errors.get(path)
        if isinstance(script, list):
            return script.pop(0) if script else None
        return script

    def do_GET(self):
        path = self.record(None)
        failure = self.failure_for(path)
        if failure:
            return self.reply(failure[1], failure[0])
        if path.endswith("/models"):
            return self.reply({"data": [{"id": model} for model in type(self).models]})
        return self.reply({"error": "not found"}, 404)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        token_path = urllib.parse.urlsplit(grok.TOKEN_URL).path
        path = urllib.parse.urlsplit(self.path).path
        if path == token_path:
            body = {key: values[0] for key, values in
                    urllib.parse.parse_qs(raw.decode("utf-8")).items()}
        else:
            body = json.loads(raw.decode("utf-8") or "{}")
        self.record(body)
        failure = self.failure_for(path)
        if failure:
            return self.reply(failure[1], failure[0])
        if path == token_path:
            grant = type(self).token_grants.get(body.get("grant_type"))
            if grant is None:
                return self.reply({"error": "invalid_grant",
                                   "error_description": "unexpected grant"}, 400)
            status, payload = grant if isinstance(grant, tuple) else (200, grant)
            return self.reply(payload, status)
        return self.reply(sse(type(self).events), content_type="text/event-stream")


class GrokTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.saved = {name: getattr(grok, name) for name in
                     ("AUTHORIZE_URL", "TOKEN_URL", "API_BASE", "CLI_BASE", "REDIRECT_PORT")}
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), MockXai)
        cls.server.daemon_threads = True
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        origin = "http://127.0.0.1:%s" % cls.server.server_port
        grok.AUTHORIZE_URL = origin + "/oauth2/authorize"
        grok.TOKEN_URL = origin + "/oauth2/token"
        grok.API_BASE = origin + "/v1"
        grok.CLI_BASE = origin + "/cli-v1"

    @classmethod
    def tearDownClass(cls):
        for name, value in cls.saved.items():
            setattr(grok, name, value)
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        MockXai.requests = []
        MockXai.token_grants = {}
        MockXai.errors = {}
        MockXai.events = []
        MockXai.models = ["grok-4.7", "grok-4.6"]
        grok.REDIRECT_PORT = free_port()
        self.workspace = tempfile.TemporaryDirectory(prefix="grok-tests-")
        self.addCleanup(self.workspace.cleanup)
        self.provider = self.new_provider()

    # ------------------------------------------------------------------ fixtures
    def new_provider(self, **_):
        provider = grok.Grok(Path(self.workspace.name) / uuid.uuid4().hex)
        self.addCleanup(provider.stop)
        return provider

    def seed(self, provider=None, **overrides):
        provider = provider or self.provider
        email = overrides.pop("email", "grok@example.com")
        tier = overrides.pop("tier", 2)
        claims = {"sub": "user-1", "email": email, "tier": tier, "exp": int(time.time()) + 3600}
        row = {"access_token": jwt(claims), "refresh_token": "refresh-1",
               "expires_at": time.time() + 3600, "id": email, "email": email,
               "tier": tier, "kind": "oauth", "label": email}
        row.update(overrides)
        return provider.pool.save(row)

    def chat_payload(self, **overrides):
        payload = {"model": "grok-4.7", "messages": [{"role": "user", "content": "你好"}],
                   "stream": True}
        payload.update(overrides)
        return payload

    def collect(self, frames):
        chunks, done = [], False
        for frame in frames:
            self.assertIsInstance(frame, bytes)
            text = frame.decode("utf-8")
            self.assertTrue(text.startswith("data: "), text)
            data = text[6:].strip()
            if data == "[DONE]":
                done = True
                continue
            chunks.append(json.loads(data))
        return chunks, done

    def paths(self):
        return [item[0] for item in MockXai.requests]

    def responses_request(self):
        for path, body, headers in MockXai.requests:
            if path.endswith("/responses"):
                return path, body, headers
        self.fail("no /responses request was sent")

    def token_request(self, grant_type):
        for path, body, headers in MockXai.requests:
            if path == urllib.parse.urlsplit(grok.TOKEN_URL).path \
                    and body.get("grant_type") == grant_type:
                return body, headers
        self.fail("no %s token request was sent" % grant_type)

    def finish_callback(self, login_id, query):
        state = self.provider.pending[login_id]["state"]
        params = {"state": state, **query}
        url = grok.redirect_uri() + "?" + urllib.parse.urlencode(params)
        try:
            with urllib.request.urlopen(url, timeout=5) as response:
                return response.read()
        except urllib.error.HTTPError as exc:
            return exc.read()

    # --------------------------------------------------------------------- login
    def test_login_start_builds_s256_pkce_authorize_url(self):
        login = self.provider.start_login("intl")
        self.assertTrue(login["login_id"])
        self.assertGreater(login["expires_at"], time.time())
        self.assertEqual(login["interval"], 2)
        parsed = urllib.parse.urlsplit(login["url"])
        self.assertEqual(parsed.scheme + "://" + parsed.netloc + parsed.path, grok.AUTHORIZE_URL)
        query = {key: values[0] for key, values in urllib.parse.parse_qs(parsed.query).items()}
        self.assertEqual(query["response_type"], "code")
        self.assertEqual(query["client_id"], grok.CLIENT_ID)
        self.assertEqual(query["redirect_uri"], grok.redirect_uri())
        self.assertEqual(query["scope"], grok.SCOPES)
        self.assertEqual(query["code_challenge_method"], "S256")
        self.assertTrue(query["state"])
        self.assertTrue(query["nonce"])
        self.assertEqual(query["plan"], "generic")
        self.assertEqual(query["referrer"], grok.REFERRER)
        verifier = self.provider.pending[login["login_id"]]["verifier"]
        expected = base64.urlsafe_b64encode(
            hashlib.sha256(verifier.encode("ascii")).digest()).decode("ascii").rstrip("=")
        self.assertEqual(query["code_challenge"], expected)
        self.assertNotIn("verifier", json.dumps(login))

    def test_login_poll_exchanges_code_with_verifier(self):
        MockXai.token_grants = {"authorization_code": token_grant()}
        login = self.provider.start_login()
        verifier = self.provider.pending[login["login_id"]]["verifier"]
        self.finish_callback(login["login_id"], {"code": "auth-code"})
        result = self.provider.poll_login(login["login_id"])
        self.assertEqual(result["status"], "authenticated")
        self.assertEqual(result["account_id"], "grok@example.com")
        self.assertEqual(self.provider.pending, {})
        body, headers = self.token_request("authorization_code")
        self.assertEqual(body["code"], "auth-code")
        self.assertEqual(body["client_id"], grok.CLIENT_ID)
        self.assertEqual(body["redirect_uri"], grok.redirect_uri())
        self.assertEqual(body["code_verifier"], verifier)
        self.assertNotIn("client_secret", body)
        self.assertEqual(headers["x-grok-client-version"], grok.CLIENT_VERSION)
        rows = self.provider.account_views()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["name"], "grok@example.com")
        self.assertTrue(rows[0]["ready"])
        serialized = json.dumps(rows)
        self.assertNotIn("access_token", serialized)
        self.assertNotIn("refresh_token", serialized)
        self.assertNotIn("refresh-1", serialized)
        _, saved = self.provider.pool.records()[0]
        self.assertEqual(saved["refresh_token"], "refresh-1")
        self.assertEqual(saved["tier"], 2)
        # expires_in (3600) is trusted less than the JWT exp claim.
        self.assertLess(abs(saved["expires_at"] - (time.time() + 3600)), 30)

    def test_login_denied_is_terminal(self):
        login = self.provider.start_login()
        self.finish_callback(login["login_id"],
                             {"error": "access_denied", "error_description": "用户取消授权"})
        with self.assertRaises(ProviderError) as raised:
            self.provider.poll_login(login["login_id"])
        self.assertEqual(raised.exception.code, "access_denied")
        self.assertIn("授权", str(raised.exception))
        self.assertEqual(self.provider.pending, {})

    def test_login_expiry_is_reported(self):
        login = self.provider.start_login()
        self.provider.pending[login["login_id"]]["expires_at"] = time.time() - 1
        with self.assertRaises(ProviderError) as raised:
            self.provider.poll_login(login["login_id"])
        self.assertEqual(raised.exception.code, "expired_token")
        self.assertEqual(self.provider.pending, {})

    def test_login_state_mismatch_is_rejected_before_token_exchange(self):
        MockXai.token_grants = {"authorization_code": token_grant()}
        login = self.provider.start_login()
        self.finish_callback(login["login_id"], {"code": "stolen", "state": "forged"})
        with self.assertRaises(ProviderError) as raised:
            self.provider.poll_login(login["login_id"])
        self.assertEqual(raised.exception.code, "invalid_state")
        self.assertEqual(self.paths(), [])
        self.assertEqual(self.provider.pool.records(), [])

    # ------------------------------------------------------------------- refresh
    def expire(self, account):
        path, row = self.provider.pool.find(account)
        row["expires_at"] = time.time() - 10
        self.provider.pool.save(row, account_id=account)

    def test_refresh_rotates_token_identity_and_tier(self):
        account = self.seed(email="old@example.com", tier=2)
        self.expire(account)
        MockXai.token_grants = {"refresh_token": token_grant(email="new@example.com", tier=0,
                                                             refresh="refresh-2")}
        row = self.provider._access()
        self.assertEqual(row["id"], account)
        self.assertEqual(row["refresh_token"], "refresh-2")
        self.assertEqual(row["email"], "new@example.com")
        self.assertEqual(row["tier"], 0)
        self.assertGreater(row["expires_at"], time.time() + 3000)
        body, _ = self.token_request("refresh_token")
        self.assertEqual(body["refresh_token"], "refresh-1")
        self.assertEqual(body["client_id"], grok.CLIENT_ID)
        self.assertNotIn("client_secret", body)

    def test_refresh_without_rotation_keeps_previous_token(self):
        account = self.seed()
        self.expire(account)
        MockXai.token_grants = {"refresh_token": token_grant(include_refresh=False)}
        row = self.provider._access()
        self.assertEqual(row["refresh_token"], "refresh-1")

    def test_refresh_invalid_grant_marks_account_for_relogin(self):
        account = self.seed()
        self.expire(account)
        MockXai.token_grants = {"refresh_token": (400, {
            "error": "invalid_grant", "error_description": "Refresh token expired"})}
        with self.assertRaises(ProviderError) as raised:
            self.provider._access()
        self.assertEqual(raised.exception.code, "invalid_grant")
        self.assertIn("重新授权", str(raised.exception))
        _, row = self.provider.pool.find(account)
        self.assertIn("重新授权", row["error"])

    def test_proactive_refresh_happens_before_expiry(self):
        self.seed()
        path, row = self.provider.pool.find("grok@example.com")
        row["expires_at"] = time.time() + grok.REFRESH_SKEW - 30
        self.provider.pool.save(row, account_id="grok@example.com")
        MockXai.token_grants = {"refresh_token": token_grant(refresh="refresh-early")}
        row = self.provider._access()
        self.assertEqual(row["refresh_token"], "refresh-early")

    def test_expiry_prefers_jwt_exp_over_expires_in(self):
        exp = int(time.time()) + 120
        self.assertLess(abs(grok.Grok._expiry(jwt({"exp": exp}), 21600) - exp), 2)
        self.assertLess(abs(grok.Grok._expiry("opaque-token", 7200) - (time.time() + 7200)), 3)
        self.assertLess(abs(grok.Grok._expiry("opaque-token", None)
                            - (time.time() + grok.DEFAULT_EXPIRES_IN)), 3)

    # ---------------------------------------------------------------------- chat
    def test_chat_text_frames_match_openai_chunk_shape(self):
        self.seed(tier=2)
        _, account = self.provider.pool.records()[0]
        MockXai.events = [
            CREATED,
            {"type": "response.output_item.added", "output_index": 0,
             "item": {"type": "message", "role": "assistant"}},
            {"type": "response.output_text.delta", "output_index": 0, "delta": "你好"},
            {"type": "response.output_text.delta", "output_index": 0, "delta": "！"},
            {"type": "response.output_item.done", "output_index": 0,
             "item": {"type": "message",
                      "content": [{"type": "output_text", "text": "你好！"}]}},
            {"type": "response.completed",
             "response": {"id": "resp_1", "model": "grok-4.7", "status": "completed",
                          "output": [{"type": "message",
                                      "content": [{"type": "output_text", "text": "你好！"}]}],
                          "usage": {"input_tokens": 11, "output_tokens": 3, "total_tokens": 14,
                                    "input_tokens_details": {"cached_tokens": 5},
                                    "output_tokens_details": {"reasoning_tokens": 2}}}},
        ]
        chunks, done = self.collect(self.provider.chat(self.chat_payload()))
        self.assertTrue(done)
        self.assertEqual(chunks[0], {
            "id": "resp_1", "object": "chat.completion.chunk", "created": 1700000000,
            "model": "grok-4.7",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""},
                         "finish_reason": None}]})
        self.assertEqual(chunks[1], {
            "id": "resp_1", "object": "chat.completion.chunk", "created": 1700000000,
            "model": "grok-4.7",
            "choices": [{"index": 0, "delta": {"content": "你好"}, "finish_reason": None}]})
        self.assertEqual(chunks[2]["choices"][0]["delta"], {"content": "！"})
        self.assertEqual(chunks[3]["choices"][0], {"index": 0, "delta": {},
                                                   "finish_reason": "stop"})
        self.assertEqual(chunks[4], {
            "id": "resp_1", "object": "chat.completion.chunk", "created": 1700000000,
            "model": "grok-4.7", "choices": [],
            "usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14,
                      "prompt_tokens_details": {"cached_tokens": 5},
                      "completion_tokens_details": {"reasoning_tokens": 2}}})
        self.assertEqual(len(chunks), 5)
        path, body, headers = self.responses_request()
        self.assertEqual(path, "/v1/responses")
        self.assertEqual(body["model"], "grok-4.7")
        self.assertTrue(body["stream"])
        self.assertFalse(body["store"])
        self.assertEqual(body["include"], ["reasoning.encrypted_content"])
        self.assertEqual(body["input"], [{"role": "user", "content": "你好"}])
        self.assertEqual(headers["authorization"], "Bearer " + account["access_token"])
        self.assertNotIn("x-grok-client-version", headers)

    def test_chat_reasoning_delta_and_cli_headers(self):
        self.seed(tier=0)
        MockXai.events = [
            CREATED,
            {"type": "response.reasoning_summary_text.delta", "output_index": 0,
             "delta": "先想一想"},
            {"type": "response.output_text.delta", "output_index": 0, "delta": "答案"},
            {"type": "response.completed",
             "response": {"id": "resp_1", "model": "grok-4.7", "status": "completed",
                          "usage": dict(USAGE)}},
        ]
        chunks, done = self.collect(self.provider.chat(self.chat_payload()))
        self.assertTrue(done)
        deltas = [chunk["choices"][0]["delta"] for chunk in chunks if chunk["choices"]]
        self.assertIn({"reasoning_content": "先想一想"}, deltas)
        self.assertIn({"content": "答案"}, deltas)
        path, _, headers = self.responses_request()
        self.assertEqual(path, "/cli-v1/responses")
        self.assertEqual(headers["x-grok-client-version"], grok.CLIENT_VERSION)
        self.assertEqual(headers["x-grok-client-identifier"], "grok-shell")
        self.assertEqual(headers["x-grok-client-mode"], "headless")
        self.assertEqual(headers["x-xai-token-auth"], "xai-grok-cli")
        self.assertEqual(headers["x-authenticateresponse"], "authenticate-response")
        self.assertTrue(headers["user-agent"].startswith(
            "grok-shell/%s (" % grok.CLIENT_VERSION))

    def test_chat_function_call_streams_stable_tool_indexes(self):
        self.seed()
        MockXai.events = [
            CREATED,
            {"type": "response.output_item.added", "output_index": 0,
             "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                      "name": "get_weather", "arguments": ""}},
            {"type": "response.function_call_arguments.delta", "output_index": 0,
             "delta": '{"city":'},
            {"type": "response.function_call_arguments.delta", "output_index": 0,
             "delta": '"上海"}'},
            {"type": "response.output_item.done", "output_index": 0,
             "item": {"type": "function_call", "call_id": "call_1", "name": "get_weather",
                      "arguments": '{"city":"上海"}'}},
            {"type": "response.completed",
             "response": {"id": "resp_1", "model": "grok-4.7", "status": "completed",
                          "usage": dict(USAGE)}},
        ]
        payload = self.chat_payload(tools=[{"type": "function", "function": {
            "name": "get_weather", "parameters": {"type": "object", "properties": {}}}}])
        chunks, done = self.collect(self.provider.chat(payload))
        self.assertTrue(done)
        calls = [chunk["choices"][0]["delta"]["tool_calls"][0] for chunk in chunks
                 if chunk["choices"] and "tool_calls" in chunk["choices"][0]["delta"]]
        self.assertEqual(calls[0], {"index": 0, "id": "call_1", "type": "function",
                                    "function": {"name": "get_weather", "arguments": ""}})
        self.assertEqual([call["function"]["arguments"] for call in calls[1:]],
                         ['{"city":', '"上海"}'])
        self.assertTrue(all(call["index"] == 0 for call in calls))
        self.assertEqual(chunks[-2]["choices"][0]["finish_reason"], "tool_calls")
        body = self.responses_request()[1]
        self.assertEqual(body["tools"], [{"type": "function", "name": "get_weather",
                                          "parameters": {"type": "object", "properties": {}}}])

    def test_chat_completed_with_empty_output_rebuilds_from_item_done(self):
        self.seed()
        MockXai.events = [
            CREATED,
            {"type": "response.output_item.done", "output_index": 0,
             "item": {"type": "message",
                      "content": [{"type": "output_text", "text": "重建文本"}]}},
            {"type": "response.output_item.done", "output_index": 1,
             "item": {"type": "reasoning",
                      "summary": [{"type": "summary_text", "text": "先想"}]}},
            {"type": "response.output_item.done", "output_index": 2,
             "item": {"type": "function_call", "call_id": "call_9", "name": "lookup",
                      "arguments": '{"q":"x"}'}},
            {"type": "response.completed",
             "response": {"id": "resp_1", "model": "grok-4.7", "status": "completed",
                          "output": [], "usage": dict(USAGE)}},
        ]
        chunks, done = self.collect(self.provider.chat(self.chat_payload()))
        self.assertTrue(done)
        deltas = [chunk["choices"][0]["delta"] for chunk in chunks if chunk["choices"]]
        self.assertIn({"content": "重建文本"}, deltas)
        self.assertIn({"reasoning_content": "先想"}, deltas)
        named = [delta["tool_calls"][0] for delta in deltas
                 if "tool_calls" in delta and delta["tool_calls"][0].get("function", {})
                 .get("name")]
        self.assertEqual(named[0]["id"], "call_9")
        self.assertEqual(named[0]["function"]["name"], "lookup")
        self.assertEqual(chunks[-2]["choices"][0]["finish_reason"], "tool_calls")

    def test_chat_completed_output_replayed_when_no_item_events(self):
        self.seed()
        MockXai.events = [
            CREATED,
            {"type": "response.completed",
             "response": {"id": "resp_1", "model": "grok-4.7", "status": "completed",
                          "output": [
                              {"type": "message",
                               "content": [{"type": "output_text", "text": "最终"}]},
                              {"type": "function_call", "call_id": "call_x", "name": "run",
                               "arguments": "{}"}],
                          "usage": dict(USAGE)}},
        ]
        chunks, done = self.collect(self.provider.chat(self.chat_payload()))
        self.assertTrue(done)
        deltas = [chunk["choices"][0]["delta"] for chunk in chunks if chunk["choices"]]
        self.assertIn({"content": "最终"}, deltas)
        self.assertIn({"tool_calls": [{"index": 0, "function": {"arguments": "{}"}}]}, deltas)
        self.assertEqual(chunks[-2]["choices"][0]["finish_reason"], "tool_calls")

    def test_chat_incomplete_response_reports_length(self):
        self.seed()
        MockXai.events = [
            CREATED,
            {"type": "response.output_text.delta", "output_index": 0, "delta": "截断"},
            {"type": "response.incomplete",
             "response": {"id": "resp_1", "model": "grok-4.7", "status": "incomplete",
                          "incomplete_details": {"reason": "max_output_tokens"},
                          "usage": dict(USAGE)}},
        ]
        chunks, done = self.collect(self.provider.chat(
            self.chat_payload(max_tokens=16)))
        self.assertTrue(done)
        self.assertEqual(chunks[-2]["choices"][0]["finish_reason"], "length")
        self.assertEqual(self.responses_request()[1]["max_output_tokens"], 16)

    def test_chat_rejects_invalid_payload_before_any_upstream_call(self):
        with self.assertRaises(ProviderError) as raised:
            self.provider.chat({"model": "grok-4.7", "messages": []})
        self.assertEqual(raised.exception.status, 400)
        self.assertEqual(MockXai.requests, [])

    def test_chat_upstream_error_event_is_surfaced(self):
        self.seed()
        MockXai.events = [CREATED, {"type": "error",
                                    "error": {"code": "server_error",
                                              "message": "overloaded"}}]
        with self.assertRaises(ProviderError) as raised:
            self.collect(self.provider.chat(self.chat_payload()))
        self.assertIn("Grok 上游返回错误", str(raised.exception))
        self.assertIn("overloaded", str(raised.exception))

    def test_chat_truncated_event_stream_is_an_error(self):
        self.seed()
        MockXai.events = [CREATED,
                          {"type": "response.output_text.delta", "output_index": 0,
                           "delta": "半截"}]
        with self.assertRaises(ProviderError) as raised:
            self.collect(self.provider.chat(self.chat_payload()))
        self.assertEqual(raised.exception.code, "upstream_truncated")

    def test_chat_retries_same_account_after_reactive_refresh(self):
        self.seed()
        MockXai.errors = {"/v1/responses": [(401, {"error": "access token expired"})]}
        MockXai.token_grants = {"refresh_token": token_grant(refresh="refresh-2")}
        MockXai.events = text_events()
        chunks, done = self.collect(self.provider.chat(self.chat_payload()))
        self.assertTrue(done)
        self.assertEqual(self.paths(), ["/v1/responses",
                                        urllib.parse.urlsplit(grok.TOKEN_URL).path,
                                        "/v1/responses"])
        _, saved = self.provider.pool.records()[0]
        self.assertEqual(saved["refresh_token"], "refresh-2")
        self.assertEqual(MockXai.requests[-1][2]["authorization"],
                         "Bearer " + saved["access_token"])

    def test_chat_is_lazy_until_the_stream_is_consumed(self):
        self.seed()
        frames = self.provider.chat(self.chat_payload())
        self.assertEqual(MockXai.requests, [])
        MockXai.events = text_events()
        self.assertEqual(len(self.collect(frames)[0]), 4)

    def test_messages_endpoint_is_derived_from_chat_frames(self):
        self.seed()
        MockXai.events = text_events()
        events = [frame.decode("utf-8") for frame in self.provider.messages(
            {"model": "grok-4.7", "max_tokens": 32,
             "messages": [{"role": "user", "content": "hi"}]})]
        joined = "".join(events)
        self.assertIn("event: message_start", joined)
        self.assertIn('"type": "text_delta"', joined)
        self.assertIn('"text": "你好"', joined)
        self.assertIn("event: message_stop", joined)
        body = self.responses_request()[1]
        self.assertEqual(body["input"], [{"role": "user", "content": "hi"}])
        self.assertEqual(body["max_output_tokens"], 32)

    # ---------------------------------------------------------------- transports
    def test_transport_routing_by_tier_and_composer_models(self):
        low = self.new_provider()
        self.seed(low, tier=0)
        MockXai.events = text_events()
        self.collect(low.chat(self.chat_payload(model="grok-4.7")))
        self.assertEqual(self.paths(), ["/cli-v1/responses"])
        self.assertIn("x-grok-client-version", MockXai.requests[0][2])

        MockXai.requests = []
        high = self.new_provider()
        self.seed(high, tier=3)
        self.collect(high.chat(self.chat_payload(model="grok-4.7")))
        self.assertEqual(self.paths(), ["/v1/responses"])
        self.assertNotIn("x-grok-client-version", MockXai.requests[0][2])

        MockXai.requests = []
        composer = self.new_provider()
        self.seed(composer, tier=3)
        self.collect(composer.chat(self.chat_payload(model="grok-composer-2.5-fast")))
        self.assertEqual(self.paths(), ["/cli-v1/responses"])

        MockXai.requests = []
        overridden = self.new_provider()
        self.seed(overridden, tier=3, base_url=grok.CLI_BASE)
        self.collect(overridden.chat(self.chat_payload(model="grok-4.7")))
        self.assertEqual(self.paths(), ["/cli-v1/responses"])

    def test_config_base_url_override_wins_over_tier_routing(self):
        self.provider.save_config({"base_url": grok.CLI_BASE})
        self.seed(tier=3)
        MockXai.events = text_events()
        self.collect(self.provider.chat(self.chat_payload(model="grok-4.7")))
        self.assertEqual(self.paths(), ["/cli-v1/responses"])

    def test_entitlement_wall_retries_free_model_on_cli_proxy(self):
        self.seed(tier=2)
        MockXai.errors = {"/v1/responses": (403, {
            "code": "permission_denied",
            "error": "You have either run out of available resources or do not have an "
                     "active Grok subscription. Manage your subscription at grok.com."})}
        MockXai.events = text_events()
        chunks, done = self.collect(self.provider.chat(self.chat_payload(model="grok-4.5")))
        self.assertTrue(done)
        self.assertEqual(chunks[0]["choices"][0]["delta"]["role"], "assistant")
        self.assertEqual(self.paths(), ["/v1/responses", "/cli-v1/responses"])
        self.assertEqual(MockXai.requests[1][2]["x-grok-client-version"], grok.CLIENT_VERSION)

    def test_entitlement_wall_on_other_models_is_a_typed_error(self):
        self.seed(tier=2)
        MockXai.errors = {"/v1/responses": (403, {
            "code": "permission_denied",
            "error": "The caller does not have an active Grok subscription or api tier."})}
        with self.assertRaises(ProviderError) as raised:
            self.collect(self.provider.chat(self.chat_payload(model="grok-4.7")))
        self.assertEqual(raised.exception.code, "entitlement_required")
        self.assertEqual(raised.exception.status, 403)
        self.assertIn("API Key", str(raised.exception))
        self.assertEqual(self.paths(), ["/v1/responses"])

    def test_payment_required_402_retries_free_model_on_cli_proxy(self):
        self.seed(tier=2)
        MockXai.errors = {"/v1/responses": (402, {"error": "payment required"})}
        MockXai.events = text_events()
        self.collect(self.provider.chat(self.chat_payload(model="grok-4.5")))
        self.assertEqual(self.paths(), ["/v1/responses", "/cli-v1/responses"])

    def test_tier_claim_routing_rules(self):
        self.assertFalse(grok.tier_allows_api(None))
        self.assertFalse(grok.tier_allows_api(0))
        self.assertFalse(grok.tier_allows_api("free"))
        self.assertFalse(grok.tier_allows_api("pro"))
        self.assertTrue(grok.tier_allows_api(2))
        self.assertTrue(grok.tier_allows_api("3"))

    def test_non_entitlement_403_is_not_retried_on_the_cli_proxy(self):
        self.seed(tier=2)
        MockXai.errors = {"/v1/responses": (403, {
            "code": "blocked", "error": "request blocked by policy"})}
        with self.assertRaises(ProviderError) as raised:
            self.collect(self.provider.chat(self.chat_payload(model="grok-4.5")))
        self.assertEqual(raised.exception.status, 403)
        self.assertEqual(self.paths(), ["/v1/responses"])

    def test_stale_client_version_426_is_translated(self):
        self.seed(tier=2)
        MockXai.errors = {"/v1/responses": (426, {"error": "client version is too old"})}
        with self.assertRaises(ProviderError) as raised:
            self.collect(self.provider.chat(self.chat_payload()))
        self.assertEqual(raised.exception.status, 426)
        self.assertEqual(raised.exception.code, "client_version_outdated")
        self.assertIn("426", str(raised.exception))
        self.assertIn("版本", str(raised.exception))

    def test_api_key_escape_hatch_bypasses_cli_headers(self):
        provider = self.new_provider()
        provider.import_credentials({"access_token": "xai-test-key",
                                     "email": "apikey@example.com"})
        MockXai.events = text_events()
        self.collect(provider.chat(self.chat_payload()))
        path, _, headers = self.responses_request()
        self.assertEqual(path, "/v1/responses")
        self.assertEqual(headers["authorization"], "Bearer xai-test-key")
        self.assertNotIn("x-grok-client-version", headers)
        self.assertNotIn("xai-test-key", json.dumps(provider.account_views()))

    # -------------------------------------------------------------------- models
    def test_models_use_live_catalog_with_static_fallback(self):
        self.seed(tier=2)
        self.assertEqual(self.provider.fetch_models(), ["grok-4.7", "grok-4.6"])
        self.assertEqual(self.paths(), ["/v1/models"])
        MockXai.errors = {"/v1/models": (500, {"error": "boom"})}
        self.assertEqual(self.provider.fetch_models(), list(grok.FALLBACK_MODELS))
        self.assertIn("grok-build-0.1", grok.FALLBACK_MODELS)
        self.assertNotIn("grok-code-fast-1", grok.FALLBACK_MODELS)

    def test_models_are_listed_from_the_cli_proxy_for_free_tier_accounts(self):
        provider = self.new_provider()
        self.seed(provider, tier=0)
        self.assertEqual(provider.fetch_models(), ["grok-4.7", "grok-4.6"])
        self.assertEqual(self.paths(), ["/cli-v1/models"])

    def test_models_fallback_without_any_account(self):
        provider = self.new_provider()
        self.assertEqual(provider.fetch_models(), list(grok.FALLBACK_MODELS))
        self.assertEqual(MockXai.requests, [])

    # -------------------------------------------------------------------- import
    def test_import_grok_auth_json_shape(self):
        token = jwt({"sub": "u-9", "email": "cli@example.com", "tier": 0,
                     "exp": int(time.time()) + 60})
        payload = {grok.ISSUER + "::" + grok.CLIENT_ID: {
            "key": token, "refresh_token": "imported-refresh",
            "expires_at": "2030-01-02T03:04:05Z", "user_id": "u-9",
            "email": "cli@example.com", "oidc_issuer": grok.ISSUER,
            "oidc_client_id": grok.CLIENT_ID, "auth_mode": "oidc"}}
        result = self.provider.import_credentials(payload)
        self.assertEqual(result["count"], 1)
        rows = self.provider.account_views()
        self.assertEqual(rows[0]["name"], "cli@example.com")
        self.assertTrue(rows[0]["ready"])
        _, saved = self.provider.pool.records()[0]
        self.assertEqual(saved["refresh_token"], "imported-refresh")
        self.assertEqual(saved["kind"], "oauth")
        self.assertEqual(saved["tier"], 0)
        expected = datetime(2030, 1, 2, 3, 4, 5, tzinfo=timezone.utc).timestamp()
        self.assertAlmostEqual(saved["expires_at"], expected, places=0)

    def test_import_ignores_foreign_oidc_entries(self):
        token = jwt({"email": "mine@example.com", "exp": int(time.time()) + 60})
        payload = {grok.ISSUER + "::other-client": {
            "key": token, "oidc_issuer": "https://other.example", "oidc_client_id": "x"}}
        with self.assertRaises(ProviderError):
            self.provider.import_credentials(payload)
        self.assertEqual(self.provider.pool.records(), [])

    def test_import_rejects_missing_token(self):
        with self.assertRaises(ProviderError) as raised:
            self.provider.import_credentials({"email": "a@example.com"})
        self.assertEqual(raised.exception.status, 400)

    # --------------------------------------------------------------- translation
    def test_chat_to_responses_translates_history_tools_and_strips_fields(self):
        payload = {
            "model": "grok-4.7",
            "messages": [
                {"role": "system", "content": "你是助手"},
                {"role": "user", "content": [
                    {"type": "text", "text": "看图"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}}]},
                {"role": "assistant", "content": None, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "get_weather", "arguments": '{"city":"上海"}'}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "晴"},
            ],
            "max_tokens": 256, "temperature": 0.2, "presence_penalty": 0.1,
            "frequency_penalty": 0.2, "logprobs": True, "top_logprobs": 2,
            "reasoning_effort": "high", "parallel_tool_calls": False,
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
            "tools": [{"type": "function", "function": {
                "name": "get_weather", "description": "查询天气",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}],
            "response_format": {"type": "json_schema",
                                "json_schema": {"name": "reply",
                                                "schema": {"type": "object"}, "strict": True}},
        }
        body = chat_to_responses(payload)
        self.assertEqual(body["model"], "grok-4.7")
        self.assertTrue(body["stream"])
        self.assertFalse(body["store"])
        self.assertEqual(body["include"], ["reasoning.encrypted_content"])
        self.assertEqual(body["max_output_tokens"], 256)
        self.assertEqual(body["temperature"], 0.2)
        self.assertEqual(body["reasoning"], {"effort": "high"})
        self.assertFalse(body["parallel_tool_calls"])
        self.assertEqual(body["input"], [
            {"role": "system", "content": "你是助手"},
            {"role": "user", "content": [
                {"type": "input_text", "text": "看图"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAA"}]},
            {"type": "function_call", "call_id": "call_1", "name": "get_weather",
             "arguments": '{"city":"上海"}'},
            {"type": "function_call_output", "call_id": "call_1", "output": "晴"},
        ])
        self.assertEqual(body["tools"], [{"type": "function", "name": "get_weather",
                                          "description": "查询天气",
                                          "parameters": {"type": "object", "properties": {
                                              "city": {"type": "string"}}}}])
        self.assertEqual(body["tool_choice"], {"type": "function", "name": "get_weather"})
        self.assertEqual(body["text"], {"format": {"type": "json_schema", "name": "reply",
                                                   "schema": {"type": "object"}, "strict": True}})
        for dropped in ("presence_penalty", "frequency_penalty", "logprobs", "top_logprobs",
                        "stop", "seed", "stream_options"):
            self.assertNotIn(dropped, body)

    def test_chat_to_responses_rejects_unsupported_input(self):
        with self.assertRaises(ProviderError):
            chat_to_responses({"model": "grok-4.7",
                               "messages": [{"role": "wizard", "content": "hi"}]})
        with self.assertRaises(ProviderError):
            chat_to_responses({"messages": []})

    def test_tool_catalog_is_capped_at_two_hundred(self):
        tools = [{"type": "function", "function": {
            "name": "tool_%s" % index,
            "parameters": {"type": "object", "properties": {}}}} for index in range(250)]
        body = chat_to_responses({"model": "grok-4.7",
                                  "messages": [{"role": "user", "content": "hi"}],
                                  "tools": tools})
        self.assertEqual(len(body["tools"]), 200)

    def test_responses_frames_ignore_non_response_events(self):
        events = [{"type": "ping"}, {"type": "response.output_text.annotation.added"},
                  CREATED, {"type": "response.output_text.delta", "delta": "x"},
                  {"type": "response.completed", "response": {"id": "resp_1",
                                                              "usage": dict(USAGE)}}]
        chunks = list(responses_frames(events, "grok-4.7"))
        self.assertEqual(chunks[-2]["choices"][0]["finish_reason"], "stop")
        self.assertEqual(chunks[-1]["usage"]["total_tokens"], 5)


if __name__ == "__main__":
    unittest.main(verbosity=2)
