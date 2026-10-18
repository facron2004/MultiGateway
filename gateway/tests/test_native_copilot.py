"""Unit tests for the GitHub Copilot native provider.

Every HTTP call goes to a local ``ThreadingHTTPServer`` that speaks the GitHub
device/token protocol and the Copilot OpenAI surface, so the suite never touches
the network and needs no credentials.
"""
import base64
import json
import socket
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from gateway.native import copilot
from gateway.transport import ProviderError

GITHUB_TOKEN = "gho-test-token"


def make_jwt(payload):
    """Build an unsigned JWT: only the payload is ever read by the provider."""
    header = base64.urlsafe_b64encode(b'{"alg":"none","typ":"JWT"}').decode().rstrip("=")
    body = base64.urlsafe_b64encode(json.dumps(payload).encode("utf-8")).decode().rstrip("=")
    return header + "." + body + ".signature"


def jwt_payload(token):
    parts = token.split(".")
    if len(parts) < 2:
        return {}
    try:
        return json.loads(base64.urlsafe_b64decode(parts[1] + "=" * (-len(parts[1]) % 4)))
    except ValueError:
        return {}


def parsed_frames(frames):
    """Decode relayed ``data:`` frames into their JSON chunks."""
    chunks = []
    for frame in frames:
        text = frame.decode("utf-8").strip()
        if not text.startswith("data:"):
            continue
        data = text[5:].strip()
        if data == "[DONE]":
            continue
        chunks.append(json.loads(data))
    return chunks


CHAT_FRAMES = [
    {"id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-4.1",
     "choices": [{"index": 0, "delta": {"role": "assistant", "content": "你好"}}]},
    {"id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-4.1",
     "choices": [{"index": 0, "delta": {"reasoning_content": "思考"}}]},
    {"id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-4.1",
     "choices": [{"index": 0, "delta": {"tool_calls": [{
         "index": 0, "id": "call_1", "type": "function",
         "function": {"name": "weather", "arguments": "{\"city\":"}}]}}]},
    {"id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-4.1",
     "choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {"tool_calls": [{
         "index": 0, "function": {"arguments": "\"上海\"}"}}]}}]},
    {"id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-4.1",
     "choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 4,
                              "total_tokens": 13,
                              "prompt_tokens_details": {"cached_tokens": 2}}},
]

MODEL_ROWS = [
    {"id": "gpt-4.1", "object": "model", "model_picker_enabled": True,
     "capabilities": {"type": "chat"}},
    {"id": "claude-sonnet-4", "object": "model"},
    {"id": "text-embedding-3-small", "object": "model",
     "capabilities": {"type": "embeddings"}},
    {"id": "retired-model", "object": "model", "model_picker_enabled": False},
]


class MockGitHub(BaseHTTPRequestHandler):
    """Mock GitHub device flow, Copilot token exchange and Copilot inference."""

    protocol_version = "HTTP/1.1"
    calls = []
    error_state = ""
    pending_polls = 0
    slow_down_polls = 0
    min_chat_nonce = 0
    token_serial = 0
    token_delay = 0.0
    exchange_denied = set()
    models_status = 200
    chat_status = 200
    frames = list(CHAT_FRAMES)

    def log_message(self, *args):
        pass

    @classmethod
    def reset(cls):
        cls.calls = []
        cls.error_state = ""
        cls.pending_polls = 0
        cls.slow_down_polls = 0
        cls.min_chat_nonce = 0
        cls.token_serial = 0
        cls.token_delay = 0.0
        cls.exchange_denied = set()
        cls.models_status = 200
        cls.chat_status = 200
        cls.frames = list(CHAT_FRAMES)

    @classmethod
    def count(cls, path):
        return sum(1 for call in cls.calls if call["path"] == path)

    @classmethod
    def last(cls, path):
        for call in reversed(cls.calls):
            if call["path"] == path:
                return call
        raise AssertionError("no request recorded for " + path)

    # ---------------------------------------------------------------- plumbing
    def _record(self, method, path, form=None, body=None):
        headers = {key.lower(): value for key, value in self.headers.items()}
        type(self).calls.append({"method": method, "path": path, "form": form or {},
                                 "body": body or {}, "headers": headers})

    def _reply(self, value, status=200, content_type="application/json; charset=utf-8"):
        body = value if isinstance(value, bytes) else json.dumps(value).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        content_type = (self.headers.get("Content-Type") or "").lower()
        if not raw:
            return {}, {}
        text = raw.decode("utf-8")
        if "application/json" in content_type:
            try:
                body = json.loads(text)
            except ValueError:
                body = {}
            return {}, body if isinstance(body, dict) else {}
        form = {key: values[0] for key, values in urllib.parse.parse_qs(text).items()}
        return form, {}

    def _bearer(self):
        value = self.headers.get("Authorization") or ""
        return value.split(" ", 1)[1] if " " in value else ""

    def _nonce(self):
        return int(jwt_payload(self._bearer()).get("nonce") or 0)

    # -------------------------------------------------------------------- POST
    def do_POST(self):
        path = urllib.parse.urlsplit(self.path).path
        form, body = self._read()
        self._record("POST", path, form, body)
        if path == "/login/device/code":
            return self._reply({"device_code": "device-secret", "user_code": "ABCD-1234",
                                "verification_uri": "https://github.com/login/device",
                                "expires_in": 900, "interval": 1})
        if path == "/login/oauth/access_token":
            if type(self).error_state:
                return self._reply({"error": type(self).error_state})
            if type(self).pending_polls > 0:
                type(self).pending_polls -= 1
                return self._reply({"error": "authorization_pending"})
            if type(self).slow_down_polls > 0:
                type(self).slow_down_polls -= 1
                return self._reply({"error": "slow_down"})
            return self._reply({"access_token": GITHUB_TOKEN, "token_type": "bearer",
                                "scope": "read:user"})
        if path == "/chat/completions":
            if type(self).chat_status != 200:
                return self._reply({"error": {"message": "upstream failed",
                                              "type": "provider_error"}},
                                   type(self).chat_status)
            if self._nonce() < type(self).min_chat_nonce:
                return self._reply({"error": {"message": "unauthorized",
                                              "type": "unauthorized"}}, 401)
            return self._reply(self._sse(), content_type="text/event-stream")
        return self._reply({"error": {"message": "not found", "type": "not_found"}}, 404)

    # --------------------------------------------------------------------- GET
    def do_GET(self):
        path = urllib.parse.urlsplit(self.path).path
        self._record("GET", path)
        if path == "/user":
            return self._reply({"login": "octocat", "name": "The Octocat",
                                "email": "octocat@example.invalid"})
        if path == "/copilot_internal/v2/token":
            if type(self).token_delay:
                time.sleep(type(self).token_delay)
            raw = self._bearer()
            token = raw[4:] if raw.startswith("token ") else raw
            if token in type(self).exchange_denied:
                return self._reply({"message": "Bad credentials"}, 401)
            type(self).token_serial += 1
            now = int(time.time())
            return self._reply({"token": make_jwt({"nonce": type(self).token_serial,
                                                   "exp": now + 1200}),
                                "expires_at": now + 1200, "refresh_in": 1200})
        if path == "/models":
            if type(self).models_status != 200:
                return self._reply({"error": {"message": "catalog unavailable",
                                              "type": "provider_error"}},
                                   type(self).models_status)
            return self._reply({"data": MODEL_ROWS})
        return self._reply({"error": {"message": "not found", "type": "not_found"}}, 404)

    def _sse(self):
        body = b"".join(b"data: " + json.dumps(frame).encode("utf-8") + b"\n\n"
                        for frame in type(self).frames)
        return body + b"data: [DONE]\n\n"


class CopilotTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.upstream = ThreadingHTTPServer(("127.0.0.1", 0), MockGitHub)
        cls.upstream.daemon_threads = True
        threading.Thread(target=cls.upstream.serve_forever, daemon=True).start()
        cls.origin = "http://127.0.0.1:%s" % cls.upstream.server_port

    @classmethod
    def tearDownClass(cls):
        cls.upstream.shutdown()
        cls.upstream.server_close()

    def setUp(self):
        MockGitHub.reset()
        patcher = mock.patch.multiple(
            copilot,
            DEVICE_CODE_URL=self.origin + "/login/device/code",
            ACCESS_TOKEN_URL=self.origin + "/login/oauth/access_token",
            COPILOT_TOKEN_URL=self.origin + "/copilot_internal/v2/token",
            GITHUB_USER_URL=self.origin + "/user",
            API_BASE=self.origin,
            BUSINESS_API_BASE=self.origin + "/business",
            ENTERPRISE_API_BASE=self.origin + "/enterprise",
        )
        patcher.start()
        self.addCleanup(patcher.stop)
        work = tempfile.TemporaryDirectory(prefix="copilot-tests-")
        self.addCleanup(work.cleanup)
        self.provider = copilot.Copilot(Path(work.name))
        self.addCleanup(self.provider.stop)

    # ----------------------------------------------------------------- helpers
    def add_account(self, account_id="octocat", token=GITHUB_TOKEN):
        self.provider.pool.save({"id": account_id, "access_token": token,
                                 "refresh_token": token, "expires_at": 0,
                                 "label": "The Octocat"})
        return account_id

    def allow_poll(self, login_id):
        with self.provider.lock:
            self.provider.pending[login_id]["next_poll"] = 0

    def login(self):
        started = self.provider.start_login()
        self.allow_poll(started["login_id"])
        return started["login_id"], self.provider.poll_login(started["login_id"])

    # -------------------------------------------------------------- device flow
    def test_device_login_happy_path(self):
        result = self.provider.start_login()
        self.assertEqual(result["user_code"], "ABCD-1234")
        self.assertEqual(result["url"], "https://github.com/login/device")
        self.assertEqual(result["interval"], 1)
        self.assertGreater(result["expires_at"], time.time())
        self.assertNotIn("device_code", result)

        request = MockGitHub.last("/login/device/code")
        self.assertEqual(request["form"]["client_id"], "Iv1.b507a08c87ecfe98")
        self.assertEqual(request["form"]["scope"], "read:user")
        self.assertEqual(request["headers"]["accept"], "application/json")
        self.assertNotIn("code_challenge", request["form"])

        MockGitHub.pending_polls = 1
        self.allow_poll(result["login_id"])
        self.assertEqual(self.provider.poll_login(result["login_id"]),
                         {"status": "pending", "interval": 1})
        self.allow_poll(result["login_id"])
        granted = self.provider.poll_login(result["login_id"])
        self.assertEqual(granted, {"status": "authenticated", "account_id": "octocat"})
        self.assertNotIn(GITHUB_TOKEN, json.dumps(granted))

        rows = [row for _, row in self.provider.pool.records()]
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["access_token"], GITHUB_TOKEN)
        self.assertEqual(rows[0]["refresh_token"], GITHUB_TOKEN)
        self.assertEqual(rows[0]["expires_at"], 0)
        self.assertEqual(rows[0]["label"], "The Octocat")
        # The poll endpoint is called with the RFC 8628 grant type.
        poll = MockGitHub.last("/login/oauth/access_token")
        self.assertEqual(poll["form"]["grant_type"],
                         "urn:ietf:params:oauth:grant-type:device_code")
        self.assertEqual(poll["form"]["device_code"], "device-secret")
        # Login pre-warms the Copilot JWT and leaves the account ready.
        self.assertEqual(MockGitHub.count("/copilot_internal/v2/token"), 1)
        view = self.provider.account_views()[0]
        self.assertTrue(view["ready"])
        self.assertEqual(view["error"], "")

    def test_only_the_github_token_is_persisted(self):
        started = self.provider.start_login()
        self.allow_poll(started["login_id"])
        self.provider.poll_login(started["login_id"])
        path = Path(self.provider.directory) / "accounts" / "octocat.json"
        stored = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(stored["access_token"], GITHUB_TOKEN)
        self.assertEqual(stored["refresh_token"], GITHUB_TOKEN)
        self.assertEqual(stored["expires_at"], 0)
        self.assertNotIn("jwt", json.dumps(stored).lower())
        self.assertEqual(self.provider._tokens["octocat"]["token"].count("."), 2)

    def test_relogin_clears_a_previous_error(self):
        self.add_account()
        self.provider._set_error("octocat", "GitHub 授权已失效，请重新授权 GitHub Copilot")
        started = self.provider.start_login()
        self.allow_poll(started["login_id"])
        self.provider.poll_login(started["login_id"])
        self.assertEqual(self.provider.account_views()[0]["error"], "")

    def test_poll_respects_vendor_interval(self):
        started = self.provider.start_login()
        self.assertEqual(self.provider.poll_login(started["login_id"]),
                         {"status": "pending", "interval": 1})
        # Still inside the interval: the dashboard gets a cheap pending answer.
        self.assertEqual(MockGitHub.count("/login/oauth/access_token"), 0)
        self.allow_poll(started["login_id"])
        self.provider.poll_login(started["login_id"])
        self.assertEqual(MockGitHub.count("/login/oauth/access_token"), 1)

    def test_slow_down_widens_interval(self):
        started = self.provider.start_login()
        MockGitHub.slow_down_polls = 1
        self.allow_poll(started["login_id"])
        result = self.provider.poll_login(started["login_id"])
        self.assertEqual(result, {"status": "pending", "interval": 6})
        # The widened interval now gates the next poll.
        self.assertEqual(MockGitHub.count("/login/oauth/access_token"), 1)
        self.provider.poll_login(started["login_id"])
        self.assertEqual(MockGitHub.count("/login/oauth/access_token"), 1)

    def test_access_denied_is_terminal(self):
        started = self.provider.start_login()
        MockGitHub.error_state = "access_denied"
        self.allow_poll(started["login_id"])
        with self.assertRaises(ProviderError) as caught:
            self.provider.poll_login(started["login_id"])
        self.assertEqual(caught.exception.code, "access_denied")
        self.assertIn("拒绝", str(caught.exception))
        self.assertNotIn(started["login_id"], self.provider.pending)
        with self.assertRaises(ProviderError) as again:
            self.provider.poll_login(started["login_id"])
        self.assertEqual(again.exception.code, "expired_token")

    def test_expired_device_code_is_terminal(self):
        started = self.provider.start_login()
        MockGitHub.error_state = "expired_token"
        self.allow_poll(started["login_id"])
        with self.assertRaises(ProviderError) as caught:
            self.provider.poll_login(started["login_id"])
        self.assertEqual(caught.exception.code, "expired_token")
        self.assertIn("超时", str(caught.exception))

    def test_local_pending_expiry(self):
        started = self.provider.start_login()
        with self.provider.lock:
            self.provider.pending[started["login_id"]]["expires_at"] = time.time() - 1
        with self.assertRaises(ProviderError) as caught:
            self.provider.poll_login(started["login_id"])
        self.assertEqual(caught.exception.code, "expired_token")
        with self.assertRaises(ProviderError) as unknown:
            self.provider.poll_login("missing-login")
        self.assertEqual(unknown.exception.code, "expired_token")

    def test_transient_poll_failure_keeps_session(self):
        started = self.provider.start_login()
        self.allow_poll(started["login_id"])
        unreachable = "http://127.0.0.1:%s/x" % _closed_port()
        with mock.patch.object(copilot, "ACCESS_TOKEN_URL", unreachable):
            self.assertEqual(self.provider.poll_login(started["login_id"]),
                             {"status": "pending", "interval": 1})
        self.assertIn(started["login_id"], self.provider.pending)

    def test_start_login_unreachable_github(self):
        unreachable = "http://127.0.0.1:%s/x" % _closed_port()
        with mock.patch.object(copilot, "DEVICE_CODE_URL", unreachable):
            with self.assertRaises(ProviderError) as caught:
                self.provider.start_login()
        self.assertEqual(caught.exception.status, 502)
        self.assertIn("GitHub", str(caught.exception))

    # ---------------------------------------------------------------- JWT cache
    def test_jwt_is_cached_between_requests(self):
        self.add_account()
        self.assertEqual(self.provider.fetch_models(), ["gpt-4.1", "claude-sonnet-4"])
        self.provider.fetch_models()
        self.assertEqual(MockGitHub.count("/copilot_internal/v2/token"), 1)
        self.assertEqual(MockGitHub.count("/models"), 2)
        models = MockGitHub.last("/models")
        self.assertTrue(models["headers"]["authorization"].startswith("Bearer "))
        self.assertEqual(models["headers"]["copilot-integration-id"], copilot.INTEGRATION_ID)
        self.assertEqual(models["headers"]["editor-version"], copilot.EDITOR_VERSION)
        self.assertEqual(models["headers"]["x-github-api-version"], copilot.GITHUB_API_VERSION)
        self.assertEqual(models["headers"]["user-agent"], copilot.USER_AGENT)

    def test_jwt_refreshes_when_inside_margin(self):
        account_id = self.add_account()
        self.provider.fetch_models()
        with self.provider.lock:
            self.provider._tokens[account_id]["expires_at"] = time.time() + 10
        self.provider.fetch_models()
        self.assertEqual(MockGitHub.count("/copilot_internal/v2/token"), 2)

    def test_401_forces_one_refresh_and_retry(self):
        self.add_account()
        self.provider.fetch_models()
        MockGitHub.min_chat_nonce = 2
        frames = list(self.provider.chat({"model": "gpt-4.1", "messages": [
            {"role": "user", "content": "你好"}]}))
        self.assertEqual(parsed_frames(frames)[0]["choices"][0]["delta"]["content"], "你好")
        self.assertEqual(MockGitHub.count("/chat/completions"), 2)
        self.assertEqual(MockGitHub.count("/copilot_internal/v2/token"), 2)

    def test_concurrent_requests_refresh_jwt_once(self):
        account_id = self.add_account()
        with self.provider.lock:
            self.provider._tokens[account_id] = {"token": make_jwt({"nonce": 0}),
                                                 "expires_at": time.time() - 1}
        MockGitHub.token_delay = 0.1
        with ThreadPoolExecutor(max_workers=6) as executor:
            results = list(executor.map(lambda _: self.provider.fetch_models(), range(6)))
        self.assertEqual(MockGitHub.count("/copilot_internal/v2/token"), 1)
        for rows in results:
            self.assertEqual(rows, ["gpt-4.1", "claude-sonnet-4"])

    def test_revoked_github_token_rotates_without_leaking(self):
        self.add_account("aaa", "gho-broken")
        self.add_account("bbb", GITHUB_TOKEN)
        MockGitHub.exchange_denied = {"gho-broken"}
        response = self.provider.request("/models", None, method="GET")
        try:
            self.assertEqual(response.gateway_account, "bbb")
        finally:
            response.close()
        errors = {view["id"]: view["error"] for view in self.provider.account_views()}
        self.assertIn("GitHub 授权已失效", errors["aaa"])
        self.assertNotIn("gho-broken", json.dumps(errors))

    def test_refresh_account_renews_the_jwt(self):
        account_id = self.add_account()
        self.assertEqual(self.provider.refresh_account(account_id),
                         {"ok": True, "message": "凭证已刷新"})
        self.assertEqual(MockGitHub.count("/copilot_internal/v2/token"), 1)
        row = [row for _, row in self.provider.pool.records()][0]
        self.assertEqual(row["access_token"], GITHUB_TOKEN)
        self.assertEqual(row["expires_at"], 0)

    # ------------------------------------------------------------------- models
    def test_models_filtering(self):
        self.add_account()
        self.assertEqual(self.provider.fetch_models(), ["gpt-4.1", "claude-sonnet-4"])

    def test_models_fallback_on_upstream_error(self):
        self.add_account()
        MockGitHub.models_status = 500
        self.assertEqual(self.provider.fetch_models(), list(copilot.FALLBACK_MODELS))

    def test_models_fallback_without_accounts(self):
        self.assertEqual(self.provider.fetch_models(), list(copilot.FALLBACK_MODELS))

    # -------------------------------------------------------------------- chat
    def test_chat_relays_frames_and_headers(self):
        self.add_account()
        payload = {"model": "gpt-4.1", "messages": [
            {"role": "user", "content": "你好"}, {"role": "assistant", "content": "在"}]}
        frames = list(self.provider.chat(payload))
        self.assertEqual(frames[-1], b"data: [DONE]\n\n")
        self.assertEqual(parsed_frames(frames), CHAT_FRAMES)

        sent = MockGitHub.last("/chat/completions")
        self.assertTrue(sent["body"]["stream"])
        self.assertEqual(sent["body"]["stream_options"], {"include_usage": True})
        self.assertEqual(sent["body"]["model"], "gpt-4.1")
        self.assertEqual(sent["headers"]["x-initiator"], "agent")
        self.assertEqual(sent["headers"]["accept"], "text/event-stream")
        self.assertTrue(sent["headers"]["x-request-id"])
        self.assertEqual(sent["headers"]["x-vscode-user-agent-library-version"],
                         "electron-fetch")
        self.assertTrue(sent["headers"]["authorization"].startswith("Bearer "))

    def test_chat_marks_plain_user_turn(self):
        self.add_account()
        list(self.provider.chat({"model": "gpt-4.1", "messages": [
            {"role": "user", "content": "hi"}]}))
        self.assertEqual(MockGitHub.last("/chat/completions")["headers"]["x-initiator"], "user")

    def test_chat_upstream_error_is_provider_error(self):
        self.add_account()
        MockGitHub.chat_status = 400
        with self.assertRaises(ProviderError) as caught:
            list(self.provider.chat({"model": "gpt-4.1", "messages": [
                {"role": "user", "content": "hi"}]}))
        self.assertEqual(caught.exception.status, 400)

    def test_messages_translation_through_base_class(self):
        self.add_account()
        events = b"".join(self.provider.messages({
            "model": "gpt-4.1", "max_tokens": 64,
            "messages": [{"role": "user", "content": "你好"}]}))
        self.assertIn(b"event: message_start", events)
        self.assertIn(b"content_block_delta", events)
        self.assertIn("你好".encode("utf-8"), events)
        self.assertIn(b"event: message_stop", events)

    # -------------------------------------------------------------- seat routing
    def test_api_base_from_jwt(self):
        self.assertEqual(self.provider._api_base(make_jwt({
            "endpoints": {"api": "https://api.business.githubcopilot.com/"}})),
            "https://api.business.githubcopilot.com")
        self.assertEqual(self.provider._api_base(make_jwt({"sku": "copilot_enterprise_seat"})),
                         copilot.ENTERPRISE_API_BASE)
        self.assertEqual(self.provider._api_base(make_jwt({"sku": "copilot_business_seat"})),
                         copilot.BUSINESS_API_BASE)
        self.assertEqual(self.provider._api_base(make_jwt({"individual": True})),
                         copilot.API_BASE)
        self.assertEqual(self.provider._api_base("not-a-jwt"), copilot.API_BASE)

    # -------------------------------------------------------------- credentials
    def test_import_credentials(self):
        result = self.provider.import_credentials({"access_token": "gho-imported"})
        self.assertEqual(result, {"ok": True, "count": 1})
        row = [row for _, row in self.provider.pool.records()][0]
        self.assertEqual(row["access_token"], "gho-imported")
        self.assertEqual(row["refresh_token"], "gho-imported")
        self.assertEqual(row["expires_at"], 0)
        self.assertTrue(self.provider.account_views()[0]["ready"])


def _closed_port():
    """Bind and release an ephemeral port so connecting to it fails fast."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


if __name__ == "__main__":
    unittest.main()
