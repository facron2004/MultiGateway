"""Regression checks for packaged imports, resource paths and maintenance tools."""
import argparse
import io
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import gateway_app
from gateway.vendor.workbuddy import wb_activity, wb_pricing, wb_proxy

ROOT = Path(__file__).resolve().parents[2]


class RepositoryLayoutTests(unittest.TestCase):
    def test_activity_and_pricing_use_the_live_workbuddy_runtime(self):
        with mock.patch.dict(sys.modules, {"__main__": None}), mock.patch.object(wb_proxy, "log") as log:
            wb_activity._log("package log check")
            log.assert_called_once_with("[活动记录] package log check", level="WARN")
            self.assertIs(wb_pricing.gateway_module(), wb_proxy)

    def test_lazy_imports_share_workbuddy_state(self):
        from gateway.vendor.workbuddy import wb_accounts, wb_agents, wb_repack, wb_scheduler, wb_tasks
        with mock.patch.object(wb_proxy, "_AGENTS_MODULE", None):
            self.assertIs(wb_proxy.agents_module(), wb_agents)
        self.assertIs(wb_tasks._accounts, wb_accounts)
        self.assertIs(wb_scheduler.wb_tasks, wb_tasks)
        self.assertIs(wb_repack.wb_updates, wb_proxy.wb_updates)

    def test_default_usage_paths_stay_outside_vendor_sources(self):
        with mock.patch.dict(os.environ, {"WB_PROXY_USAGE_DIR": "", "WB_PRICING_DIR": ""}), \
                mock.patch.object(wb_activity, "_data_dir_override", None), \
                mock.patch.object(wb_pricing, "_data_dir_override", None):
            self.assertEqual(Path(wb_activity.data_dir()), ROOT / "usage")
            self.assertEqual(Path(wb_pricing.data_dir()), ROOT / "usage")

    def test_default_pricing_uses_the_repository_snapshot(self):
        with mock.patch.dict(os.environ, {"WB_PRICING_FILE": ""}):
            self.assertEqual(Path(wb_pricing._candidate_file()), ROOT / "pricing/pricing.json")

    def test_source_launcher_keeps_data_paths_and_finds_the_embedded_panel(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(os.environ), \
                mock.patch.object(gateway_app.logging, "basicConfig"):
            data, accounts, usage = gateway_app.prepare_paths(argparse.Namespace(data_dir=directory))
            self.assertEqual(data, Path(directory).resolve())
            self.assertEqual(accounts, data / "workbuddy/accounts")
            self.assertEqual(usage, data / "workbuddy/usage")
            self.assertTrue(accounts.is_dir())
            self.assertTrue(usage.is_dir())
            self.assertTrue(Path(os.environ["WB_DASHBOARD_HTML"]).is_file())
            self.assertEqual(Path(os.environ["WB_PRICING_FILE"]), ROOT / "pricing/pricing.json")

    def test_frozen_launcher_finds_bundle_resources_and_preserves_panel_override(self):
        with tempfile.TemporaryDirectory() as directory:
            base, bundle = Path(directory) / "installed", Path(directory) / "bundle"
            base.mkdir()
            dashboard = bundle / "gateway/vendor/workbuddy/dashboard.html"
            dashboard.parent.mkdir(parents=True)
            dashboard.write_text("bundled panel", encoding="utf-8")
            with mock.patch.dict(os.environ), mock.patch.object(gateway_app.logging, "basicConfig"), \
                    mock.patch.object(sys, "frozen", True, create=True), \
                    mock.patch.object(sys, "_MEIPASS", str(bundle), create=True), \
                    mock.patch.object(sys, "executable", str(base / "MultiGateway.exe")), \
                    mock.patch.object(sys, "stdout", io.StringIO()), \
                    mock.patch.object(sys, "stderr", io.StringIO()):
                args = argparse.Namespace(data_dir=str(Path(directory) / "data"))
                gateway_app.prepare_paths(args)
                self.assertEqual(Path(os.environ["WB_DASHBOARD_HTML"]), dashboard)
                self.assertEqual(Path(os.environ["WB_PRICING_FILE"]), bundle / "pricing/pricing.json")
                override = base / "dashboard.html"
                override.write_text("custom panel", encoding="utf-8")
                gateway_app.prepare_paths(args)
                self.assertEqual(Path(os.environ["WB_DASHBOARD_HTML"]), override)

    def test_pricing_script_resolves_paths_from_another_working_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([
                sys.executable, "-c",
                "import runpy, sys; script = runpy.run_path(sys.argv[1]); "
                "print(script['OUT']); print(script['EMBED_TARGET'])",
                str(ROOT / "scripts/fetch_pricing.py"),
            ], cwd=directory, env={**os.environ, "PYTHONPATH": ""},
                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            str(ROOT / "pricing/pricing.json"),
            str(ROOT / "gateway/vendor/workbuddy/wb_pricing.py"),
        ])


if __name__ == "__main__":
    unittest.main()
