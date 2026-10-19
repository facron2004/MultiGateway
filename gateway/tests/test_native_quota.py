"""Account-scoped quota reads, parsing and cache regressions (no live HTTP)."""
import json
import sys
import tempfile
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from gateway.native import antigravity, copilot, grok
from gateway.transport import ProviderError


class NativeQuotaTests(unittest.TestCase):
    def setUp(self):
        work = tempfile.TemporaryDirectory(prefix="gateway-quota-tests-")
        self.addCleanup(work.cleanup)
        self.directory = Path(work.name)

    def provider(self, kind):
        provider = kind(self.directory / kind.name)
        self.addCleanup(provider.stop)
        for identifier in ("first", "second"):
            provider.pool.save({"id": identifier, "access_token": "token-" + identifier,
                                "refresh_token": "refresh-" + identifier,
                                "expires_at": time.time() + 3600,
                                "extra": {"project_id": "project-" + identifier}})
        return provider

    def test_copilot_free_uses_chat_instead_of_unentitled_premium(self):
        provider = self.provider(copilot.Copilot)
        payload = {"quota_reset_date_utc": "2026-11-01T00:00:00Z", "quota_snapshots": {
            "premium_interactions": {"has_quota": False, "entitlement": 0, "remaining": 0},
            "chat": {"entitlement": 200, "remaining": 200, "token_based_billing": True},
            "completions": {"entitlement": 2000, "remaining": 1800}}}
        with mock.patch.object(copilot, "json_request", return_value=payload) as request:
            quota = provider.refresh_quota("second")["quota"]
        self.assertEqual(quota["remaining"], 200)
        self.assertEqual(quota["unit"], "额度")
        self.assertEqual([b["remaining"] for b in quota["buckets"]], [200, 1800])
        self.assertEqual(quota["reset_at"], payload["quota_reset_date_utc"])
        self.assertEqual(request.call_args.args[0], copilot.COPILOT_USER_URL)
        self.assertEqual(request.call_args.kwargs["headers"]["Authorization"], "token token-second")
        self.assertIsNone(provider.pool.find("first")[1].get("remaining"))

    def test_copilot_preserves_fractional_quota_and_exhaustion(self):
        provider = self.provider(copilot.Copilot)
        for remaining in (32.125, 0):
            payload = {"quota_snapshots": {"premium_interactions": {
                "entitlement": 300, "remaining": 33, "quota_remaining": remaining,
                "token_based_billing": True}}}
            with self.subTest(remaining=remaining), mock.patch.object(
                    copilot, "json_request", return_value=payload):
                quota = provider.refresh_quota("second")["quota"]
                self.assertEqual(quota["remaining"], remaining)

    def test_copilot_supports_unlimited_percent_and_legacy_quotas(self):
        provider = self.provider(copilot.Copilot)
        cases = [({"quota_snapshots": {"premium_interactions": {"unlimited": True}}},
                  "unlimited", "次"),
                 ({"quota_snapshots": {"premium_interactions": {"percent_remaining": 25}}},
                  25, "%"),
                 ({"limited_user_quotas": {"chat": 12, "completions": 500},
                   "monthly_quotas": {"chat": 50, "completions": 2000}}, 12, "次")]
        for payload, remaining, unit in cases:
            with self.subTest(payload=payload), mock.patch.object(
                    copilot, "json_request", return_value=payload):
                quota = provider.refresh_quota("first")["quota"]
                self.assertEqual((quota["remaining"], quota["unit"]), (remaining, unit))

    def test_grok_queries_subscription_host_for_the_requested_account(self):
        provider = self.provider(grok.Grok)
        provider.pool.save({"tier": 2, "principal_id": "user-second"}, account_id="second")
        payload = {"config": {"creditUsagePercent": 37.5, "currentPeriod": {
            "type": "USAGE_PERIOD_TYPE_WEEKLY", "end": "2026-10-23T14:42:53Z"}}}
        with mock.patch.object(grok, "json_request", return_value=payload) as request:
            quota = provider.refresh_quota("second")["quota"]
        self.assertEqual((quota["remaining"], quota["unit"]), (62.5, "%"))
        self.assertEqual(request.call_args.args[0], grok.CLI_BASE + "/billing?format=credits")
        headers = request.call_args.kwargs["headers"]
        self.assertEqual(headers["Authorization"], "Bearer token-second")
        self.assertEqual(headers["x-userid"], "user-second")
        self.assertIn("X-XAI-Token-Auth", headers)
        self.assertEqual(quota["reset_at"], "2026-10-23T14:42:53Z")

    def test_grok_handles_proto_zero_and_legacy_monthly_billing(self):
        provider = self.provider(grok.Grok)
        cases = [({"currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY"}}, 100),
                 ({"creditUsagePercent": 100, "currentPeriod": {
                     "type": "USAGE_PERIOD_TYPE_WEEKLY"}}, 0),
                 ({"monthlyLimit": {"val": "2000"}, "used": {"val": "500"}}, 75),
                 ({"monthlyLimit": {"val": "2000"}, "used": {}}, 100)]
        for config, remaining in cases:
            with self.subTest(config=config), mock.patch.object(
                    grok, "json_request", return_value={"config": config}):
                quota = provider.refresh_quota("first")["quota"]
                self.assertEqual(quota["remaining"], remaining)

    def test_grok_api_keys_do_not_use_subscription_credentials(self):
        provider = self.provider(grok.Grok)
        provider.pool.save({"kind": "api_key"}, account_id="first")
        with mock.patch.object(grok, "json_request") as request:
            with self.assertRaises(ProviderError):
                provider.refresh_quota("first")
            request.assert_not_called()

    def test_antigravity_keeps_model_buckets_and_zero_is_not_missing(self):
        provider = self.provider(antigravity.Antigravity)
        payload = {"response": {"models": {
            "gemini-pro": {"displayName": "Gemini Pro", "quotaInfo": {
                "remainingFraction": 0.75, "resetTime": "2026-10-19T08:00:00Z"}},
            "claude": {"quotaInfo": {"remainingFraction": 0}},
            "no-quota": {"displayName": "No quota"}}}}
        with mock.patch.object(provider, "_post", return_value=payload) as request:
            quota = provider.refresh_quota("second")["quota"]
        self.assertEqual((quota["remaining"], quota["unit"]), (0, "%"))
        self.assertEqual(len(quota["buckets"]), 2)
        self.assertEqual({b["label"] for b in quota["buckets"]}, {"Gemini Pro", "claude"})
        request.assert_called_once_with(antigravity.FETCH_MODELS,
                                        {"project": "project-second"}, account_id="second")

    def test_antigravity_project_discovery_is_account_scoped(self):
        provider = self.provider(antigravity.Antigravity)
        provider.pool.save({"extra": {}}, account_id="second")
        with mock.patch.object(provider, "_post", side_effect=[
                {"cloudaicompanionProject": "new-second"},
                {"models": {"model": {"quotaInfo": {"remainingFraction": 1}}}}]) as request:
            provider.refresh_quota("second")
        self.assertTrue(all(call.kwargs["account_id"] == "second" for call in request.call_args_list))
        self.assertEqual(request.call_args.args[1], {"project": "new-second"})
        self.assertEqual(provider.pool.find("first")[1]["extra"]["project_id"], "project-first")

    def test_snapshot_fetches_once_and_persists_quota_without_secrets(self):
        provider = self.provider(grok.Grok)
        payload = {"config": {"creditUsagePercent": 10}}
        with mock.patch.object(grok, "json_request", return_value=payload) as request:
            views = provider.account_views()
            provider.account_views()
        self.assertEqual(request.call_count, 2)
        self.assertEqual([v["remaining"] for v in views], [90, 90])
        self.assertNotIn("token-first", json.dumps(views))
        restored = grok.Grok(provider.directory)
        self.addCleanup(restored.stop)
        with mock.patch.object(grok, "json_request") as request:
            self.assertEqual(restored.account_views()[0]["remaining"], 90)
            request.assert_not_called()

    def test_failure_retains_last_good_quota_and_throttles_retries(self):
        provider = self.provider(grok.Grok)
        with mock.patch.object(grok, "json_request", return_value={"config": {"creditUsagePercent": 10}}):
            provider.refresh_quota("first")
        previous = provider.pool.find("first")[1]["quota"]["updated_at"]
        with mock.patch.object(grok, "json_request", side_effect=ProviderError("temporary failure", 503)) as request:
            with self.assertRaises(ProviderError):
                provider.refresh_quota("first")
            views = provider.account_views()
            provider.account_views()
        self.assertEqual(request.call_count, 2)  # One explicit refresh and the other account.
        self.assertEqual(views[0]["remaining"], 90)
        self.assertEqual(views[0]["quota"]["updated_at"], previous)
        self.assertEqual(views[0]["quota"]["error"], "temporary failure")
        self.assertEqual(views[0]["error"], "")

    def test_expired_cache_refreshes_and_concurrent_reads_share_one_query(self):
        provider = self.provider(grok.Grok)
        provider.pool.save({"quota": {"checked_at": time.time() - 600}}, account_id="first")
        def response(*args, **kwargs):
            time.sleep(0.05)
            return {"config": {"creditUsagePercent": 20}}
        with mock.patch.object(grok, "json_request", side_effect=response) as request:
            with ThreadPoolExecutor(max_workers=3) as workers:
                list(workers.map(lambda _: provider.refresh_quota("first", force=False), range(3)))
        self.assertEqual(request.call_count, 1)
        self.assertEqual(provider.pool.find("first")[1]["remaining"], 80)

    def test_malformed_payloads_never_become_zero_or_full_quota(self):
        for kind, module, payload in ((copilot.Copilot, copilot, {}),
                                      (copilot.Copilot, copilot, {"quota_snapshots": [1]}),
                                      (grok.Grok, grok, {"config": {}}),
                                      (grok.Grok, grok, {"config": {"currentPeriod": "invalid"}})):
            with self.subTest(kind=kind), mock.patch.object(module, "json_request", return_value=payload):
                provider = self.provider(kind)
                with self.assertRaises(ProviderError):
                    provider.refresh_quota("first")
                self.assertIsNone(provider.pool.find("first")[1].get("remaining"))


if __name__ == "__main__":
    unittest.main()
