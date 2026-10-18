"""Unit tests for the Qwen Code native provider.

Every vendor endpoint (device authorization, token exchange/refresh and the
OpenAI-compatible portal) is served by a local ``ThreadingHTTPServer``; the
provider's module-level base URLs are pointed at it, so nothing touches the
network and no real credentials are involved.
"""
import base64
import hashlib
import json
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from gateway.native import qwen as qwen_module
from gateway.native.qwen import (CLIENT_ID, DEVICE_CODE_PATH, DEVICE_GRANT,
                                 FALLBACK_MODELS, SCOPE, TOKEN_PATH, Qwen)
from gateway.transport import ProviderError


def jwt(claims):
    """A JWT-shaped id_token with an unsigned payload (display data only)."""
    def part(value):
        return base64.urlsafe_b64encode(json.dumps(value).encode("utf-8")).decode("ascii").rstrip("=")
    return part({"alg": "none", "typ": "JWT"}) + "." + part(claims) + ".signature"


class QwenMock(BaseHTTPRequestHandler):
    """Vendor protocol double; behaviour is scripted per test through class state."""

    device = {}
    token_script = []
    refresh = {}
    models = {"data": [{"id": "coder-model"}, {"id": "vision-model"}]}
    models_fail = False
    chat_frames = []
    requests = []

    @classmethod
    def reset(cls):
        cls.device = {"device_code": "device-secret", "user_code": "ABCD-EFGH",
                      "verification_uri": "https://chat.qwen.ai/authorize",
                      "verification_uri_complete": "https://chat.qwen.ai/authorize?code=ABCD-EFGH",
                      "expires_in": 600, "interval": 1}
        cls.token_script = []
        cls.refresh = {"access_token": "refreshed-token", "expires_in": 7200,
                       "refresh_token": "rotated-refresh"}
        cls.models = {"data": [{"id": "coder-model"}, {"id": "vision-model"}]}
        cls.models_fail = False
        cls.chat_frames = [
            {"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "你好"}}]},
            {"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"reasoning_content": "思考"}}]},
            {"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "type": "function",
                 "function": {"name": "weather", "arguments": '{"city":"上海"}'}}]}}]},
            {"id": "chatcmpl-1", "choices": [], "usage": {
                "prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}},
        ]
        cls.requests = []

    def log_message(self, *args):
        pass

    def reply(self, value, status=200, content_type="application/json"):
        body = value if isinstance(value, bytes) else json.dumps(value, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/v1/models":
            type(self).requests.append((self.path, None, dict(self.headers)))
            if type(self).models_fail:
                return self.reply({"error": {"type": "server_error", "message": "boom"}}, 500)
            return self.reply(type(self).models)
        return self.reply({"error": {"type": "not_found", "message": "not found"}}, 404)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        content_type = self.headers.get("Content-Type", "")
        if "application/x-www-form-urlencoded" in content_type:
            body = {key: value[0] for key, value in urllib.parse.parse_qs(raw.decode("ascii")).items()}
        else:
            body = json.loads(raw or b"{}")
        type(self).requests.append((self.path, body, dict(self.headers)))
        if self.path == DEVICE_CODE_PATH:
            return self.reply(type(self).device)
        if self.path == TOKEN_PATH:
            if body.get("grant_type") == "refresh_token":
                return self._refresh_reply()
            return self._device_reply()
        if self.path == "/v1/chat/completions":
            payload = b"".join(("data: " + json.dumps(frame, ensure_ascii=False) + "\n\n").encode("utf-8")
                               for frame in type(self).chat_frames)
            return self.reply(payload + b"data: [DONE]\n\n", content_type="text/event-stream")
        return self.reply({"error": {"type": "not_found", "message": "not found"}}, 404)

    def _refresh_reply(self):
        refresh = type(self).refresh
        if isinstance(refresh, str):
            return self.reply({"error": "invalid_grant", "error_description": "refresh token expired"}, 400)
        return self.reply(refresh)

    def _device_reply(self):
        script = type(self).token_script
        step = script.pop(0) if script else {"access_token": "access-token", "refresh_token": "refresh-token",
                                             "expires_in": 3600, "resource_url": "https://portal.qwen.ai/v1"}
        if isinstance(step, dict):
            return self.reply(step)
        if step == "pending":
            return self.reply({"error": "authorization_pending", "error_description": "pending"}, 400)
        if step == "slow_down":
            return self.reply({"error": "slow_down", "error_description": "too fast"}, 429)
        if step == "expired":
            return self.reply({"error": "expired_token", "error_description": "expired"}, 400)
        if step == "denied":
            return self.reply({"error": "access_denied", "error_description": "denied"}, 400)
        raise AssertionError("unknown scripted step: %r" % (step,))


class QwenTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), QwenMock)
        cls.server.daemon_threads = True
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.origin = "http://127.0.0.1:%s" % cls.server.server_port
        cls.patches = [mock.patch.object(qwen_module, "QWEN_AUTH_BASE", cls.origin),
                       mock.patch.object(qwen_module, "QWEN_API_ORIGIN", cls.origin)]
        for patch in cls.patches:
            patch.start()

    @classmethod
    def tearDownClass(cls):
        for patch in cls.patches:
            patch.stop()
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        QwenMock.reset()
        self.directory = tempfile.TemporaryDirectory(prefix="qwen-tests-")
        self.addCleanup(self.directory.cleanup)
        self.provider = Qwen(Path(self.directory.name))

    # ------------------------------------------------------------------ helpers
    def login(self):
        return self.provider.start_login()

    def poll_now(self, login_id):
        """Poll immediately, bypassing the RFC 8628 interval the provider enforces."""
        with self.provider.lock:
            self.provider.pending[login_id]["next_poll"] = 0
        return self.provider.poll_login(login_id)

    def seed_account(self, **credentials):
        row = {"id": "coder@example.com", "access_token": "stored-token",
               "refresh_token": "stored-refresh", "expires_at": time.time() + 3600,
               "email": "coder@example.com"}
        row.update(credentials)
        self.provider.pool.save(row)
        return row

    def requests_to(self, path):
        return [item for item in QwenMock.requests if item[0] == path]

    # ------------------------------------------------------------- device start
    def test_device_start_returns_code_and_pkce(self):
        payload = self.login()
        self.assertEqual(payload["user_code"], "ABCD-EFGH")
        self.assertEqual(payload["url"], "https://chat.qwen.ai/authorize?code=ABCD-EFGH")
        self.assertEqual(payload["interval"], 1.0)
        self.assertGreater(payload["expires_at"], time.time())

        sent = self.requests_to(DEVICE_CODE_PATH)
        self.assertEqual(len(sent), 1)
        body = sent[0][1]
        headers = {key.lower(): value for key, value in sent[0][2].items()}
        self.assertEqual(body["client_id"], CLIENT_ID)
        self.assertEqual(body["scope"], SCOPE)
        self.assertEqual(body["code_challenge_method"], "S256")
        self.assertIn("QwenCode", headers["user-agent"])

        verifier = self.provider.pending[payload["login_id"]]["verifier"]
        expected = base64.urlsafe_b64encode(
            hashlib.sha256(verifier.encode("ascii")).digest()).decode("ascii").rstrip("=")
        self.assertEqual(body["code_challenge"], expected)

    def test_start_login_prunes_expired_sessions(self):
        with self.provider.lock:
            self.provider.pending["stale"] = {"expires_at": time.time() - 5, "interval": 1}
        payload = self.login()
        self.assertNotIn("stale", self.provider.pending)
        self.assertIn(payload["login_id"], self.provider.pending)

    def test_incomplete_device_response_is_a_provider_error(self):
        QwenMock.device = {"device_code": "only"}
        with self.assertRaises(ProviderError) as caught:
            self.login()
        self.assertEqual(caught.exception.status, 502)

    # ---------------------------------------------------------------------- poll
    def test_poll_pending_then_authenticated(self):
        claims = {"email": "coder@example.com", "name": "Qwen Coder", "sub": "user-1"}
        grant = {"access_token": "access-token", "refresh_token": "refresh-token",
                 "expires_in": 3600, "resource_url": "https://portal.qwen.ai/v1",
                 "id_token": jwt(claims)}
        QwenMock.token_script = ["pending", grant]
        payload = self.login()
        with self.provider.lock:
            verifier = self.provider.pending[payload["login_id"]]["verifier"]

        self.assertEqual(self.poll_now(payload["login_id"]), {"status": "pending", "interval": 1.0})
        result = self.poll_now(payload["login_id"])
        self.assertEqual(result["status"], "authenticated")

        sent = self.requests_to(TOKEN_PATH)
        self.assertEqual(len(sent), 2)
        polled = sent[-1][1]
        self.assertEqual(polled["grant_type"], DEVICE_GRANT)
        self.assertEqual(polled["client_id"], CLIENT_ID)
        self.assertEqual(polled["device_code"], "device-secret")
        self.assertEqual(polled["code_verifier"], verifier)

        rows = self.provider.pool.records()
        self.assertEqual(len(rows), 1)
        stored = rows[0][1]
        self.assertEqual(stored["id"], "coder@example.com")
        self.assertEqual(stored["email"], "coder@example.com")
        self.assertEqual(stored["label"], "Qwen Coder")
        self.assertEqual(stored["access_token"], "access-token")
        self.assertEqual(stored["refresh_token"], "refresh-token")
        self.assertEqual(stored["resource_url"], "https://portal.qwen.ai/v1")
        self.assertGreater(stored["expires_at"], time.time())
        self.assertNotIn(payload["login_id"], self.provider.pending)

    # ------------------------------------------------------------------ accounts
    def test_login_without_id_token_uses_a_hashed_identity(self):
        QwenMock.token_script = [{"access_token": "token-a", "refresh_token": "refresh-a",
                                  "expires_in": 3600}]
        payload = self.login()
        self.poll_now(payload["login_id"])
        _, row = self.provider.pool.records()[0]
        self.assertTrue(row["id"].startswith("qwen-"))
        self.assertEqual(row["email"], "")
        self.assertEqual(self.provider.account_views()[0]["name"], row["id"])

    # --------------------------------------------------------------- slow_down
    def test_slow_down_bumps_interval_with_a_cap(self):
        QwenMock.token_script = ["slow_down", "slow_down"]
        payload = self.login()
        login_id = payload["login_id"]

        self.assertEqual(self.poll_now(login_id), {"status": "pending", "interval": 1.5})
        self.assertEqual(self.provider.pending[login_id]["interval"], 1.5)
        self.assertEqual(self.poll_now(login_id), {"status": "pending", "interval": 2.25})

        with self.provider.lock:
            self.provider.pending[login_id]["interval"] = 8.0
            self.provider.pending[login_id]["next_poll"] = 0
        QwenMock.token_script = ["slow_down"]
        self.assertEqual(self.provider.poll_login(login_id), {"status": "pending", "interval": 10.0})
        self.assertEqual(self.provider.pending[login_id]["interval"], 10.0)

    def test_authorization_pending_keeps_the_stored_interval(self):
        QwenMock.token_script = ["pending"]
        payload = self.login()
        self.assertEqual(self.poll_now(payload["login_id"]), {"status": "pending", "interval": 1.0})
        self.assertEqual(self.provider.pending[payload["login_id"]]["interval"], 1.0)

    # --------------------------------------------------------------- terminals
    def test_expired_token_is_terminal(self):
        QwenMock.token_script = ["expired"]
        payload = self.login()
        with self.assertRaises(ProviderError) as caught:
            self.poll_now(payload["login_id"])
        self.assertEqual(caught.exception.code, "expired_token")
        self.assertEqual(caught.exception.status, 400)
        self.assertNotIn(payload["login_id"], self.provider.pending)

    def test_access_denied_is_terminal(self):
        QwenMock.token_script = ["denied"]
        payload = self.login()
        with self.assertRaises(ProviderError) as caught:
            self.poll_now(payload["login_id"])
        self.assertEqual(caught.exception.code, "access_denied")
        self.assertNotIn(payload["login_id"], self.provider.pending)

    def test_unknown_and_expired_sessions_fail_without_network(self):
        with self.assertRaises(ProviderError) as caught:
            self.provider.poll_login("missing-login")
        self.assertEqual(caught.exception.code, "expired_token")

        payload = self.login()
        with self.provider.lock:
            self.provider.pending[payload["login_id"]]["expires_at"] = time.time() - 1
        with self.assertRaises(ProviderError) as caught:
            self.provider.poll_login(payload["login_id"])
        self.assertEqual(caught.exception.code, "expired_token")
        self.assertEqual(self.requests_to(TOKEN_PATH), [])

    def test_interval_gates_repeated_polls(self):
        QwenMock.token_script = ["pending", "pending"]
        payload = self.login()
        login_id = payload["login_id"]
        # RFC 8628: wait one interval before the first poll.
        self.assertEqual(self.provider.poll_login(login_id),
                         {"status": "pending", "interval": 1.0})
        self.assertEqual(self.requests_to(TOKEN_PATH), [])
        self.assertEqual(self.poll_now(login_id), {"status": "pending", "interval": 1.0})
        self.assertEqual(self.provider.poll_login(login_id),
                         {"status": "pending", "interval": 1.0})
        self.assertEqual(len(self.requests_to(TOKEN_PATH)), 1)

    # ------------------------------------------------------------------ refresh
    def test_refresh_grant_exchanges_and_persists_tokens(self):
        self.seed_account(access_token="stale-token", expires_at=time.time() - 60)
        self.provider.refresh_account("coder@example.com")

        sent = self.requests_to(TOKEN_PATH)
        self.assertEqual(len(sent), 1)
        body = sent[0][1]
        self.assertEqual(body["grant_type"], "refresh_token")
        self.assertEqual(body["refresh_token"], "stored-refresh")
        self.assertEqual(body["client_id"], CLIENT_ID)

        _, row = self.provider.pool.find("coder@example.com")
        self.assertEqual(row["access_token"], "refreshed-token")
        # A rotated refresh token wins over the stored one.
        self.assertEqual(row["refresh_token"], "rotated-refresh")
        self.assertGreater(row["expires_at"], time.time())
        self.assertEqual(row["email"], "coder@example.com")

    def test_refresh_failure_marks_the_account(self):
        self.seed_account(expires_at=time.time() - 60)
        QwenMock.refresh = "invalid"
        with self.assertRaises(ProviderError) as caught:
            self.provider.refresh_account("coder@example.com")
        self.assertEqual(caught.exception.code, "invalid_grant")
        self.assertEqual(caught.exception.status, 401)
        _, row = self.provider.pool.find("coder@example.com")
        self.assertTrue(row["error"])

    def test_refresh_without_refresh_token_is_a_provider_error(self):
        row = self.seed_account(refresh_token="")
        with self.assertRaises(ProviderError) as caught:
            self.provider.refresh_grant(row)
        self.assertEqual(caught.exception.code, "invalid_grant")

    # --------------------------------------------------------------------- chat
    def test_chat_relays_frames_with_usage_and_headers(self):
        self.seed_account(access_token="live-token")
        frames = list(self.provider.chat({"model": "coder-model",
                                          "messages": [{"role": "user", "content": "hi"}]}))
        self.assertTrue(frames[0].startswith(b"data: "))
        self.assertEqual(frames[-1], b"data: [DONE]\n\n")

        events = [json.loads(frame[6:].decode("utf-8")) for frame in frames[:-1]]
        self.assertEqual(events[0]["choices"][0]["delta"]["content"], "你好")
        self.assertEqual(events[1]["choices"][0]["delta"]["reasoning_content"], "思考")
        tool = events[2]["choices"][0]["delta"]["tool_calls"][0]
        self.assertEqual(tool["index"], 0)
        self.assertEqual(tool["function"]["name"], "weather")
        self.assertEqual(events[3]["usage"]["prompt_tokens"], 11)

        sent = self.requests_to("/v1/chat/completions")
        self.assertEqual(len(sent), 1)
        body, headers = sent[0][1], {key.lower(): value for key, value in sent[0][2].items()}
        self.assertTrue(body["stream"])
        self.assertEqual(body["stream_options"], {"include_usage": True})
        self.assertEqual(headers["authorization"], "Bearer live-token")
        self.assertIn("QwenCode", headers["user-agent"])
        self.assertEqual(headers["x-dashscope-authtype"], "qwen-oauth")
        self.assertEqual(headers["x-dashscope-cachecontrol"], "enable")

    def test_messages_translation_uses_the_chat_stream(self):
        self.seed_account()
        frames = list(self.provider.messages({"model": "coder-model", "max_tokens": 64,
                                              "messages": [{"role": "user", "content": "hi"}]}))
        text = b"".join(frames).decode("utf-8")
        self.assertIn("event: message_start", text)
        self.assertIn("你好", text)
        self.assertIn("event: message_stop", text)
        sent = self.requests_to("/v1/chat/completions")
        self.assertEqual(sent[0][1]["max_tokens"], 64)

    # ------------------------------------------------------------------- models
    def test_models_fallback_without_accounts_makes_no_request(self):
        self.assertEqual(self.provider.models(), list(FALLBACK_MODELS))
        self.assertEqual(QwenMock.requests, [])
        self.assertEqual(QwenMock.models["data"][0]["id"], "coder-model")

    def test_models_reads_the_portal_catalog(self):
        self.seed_account()
        self.assertEqual(self.provider.models(), ["coder-model", "vision-model"])
        self.assertEqual(len(self.requests_to("/v1/models")), 1)

    def test_models_fall_back_when_the_portal_fails(self):
        self.seed_account()
        QwenMock.models_fail = True
        self.assertEqual(self.provider.fetch_models(), list(FALLBACK_MODELS))


if __name__ == "__main__":
    unittest.main()
