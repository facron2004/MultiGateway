"""Qoder native bridge process tests; no real accounts or upstream requests."""
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from gateway.vendor.qoder import qoder_accounts


class QoderRuntimeInfoTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows console behavior")
    def test_runtime_info_process_has_no_console_and_receives_account(self):
        probe = (
            "import ctypes, json, sys; "
            "ctypes.windll.kernel32.GetConsoleWindow.restype = ctypes.c_void_p; "
            "print(json.dumps({'console_window': ctypes.windll.kernel32.GetConsoleWindow() or 0, "
            "'account': json.load(sys.stdin)['account']}))"
        )
        worker = textwrap.dedent("""
            import json, subprocess, sys
            from pathlib import Path
            from unittest import mock
            sys.path.insert(0, sys.argv[1])
            from gateway.vendor.qoder import qoder_accounts
            run = subprocess.run

            def run_probe(command, **kwargs):
                # Hide a regressed console while still detecting its allocation.
                startupinfo = subprocess.STARTUPINFO()
                startupinfo.dwFlags |= subprocess.STARTF_USESHOWWINDOW
                startupinfo.wShowWindow = subprocess.SW_HIDE
                return run([sys.argv[2], "-c", sys.argv[4]], startupinfo=startupinfo, **kwargs)

            with mock.patch.object(qoder_accounts, "runtime_info_exe", return_value=sys.argv[2]), \\
                    mock.patch.object(subprocess, "run", side_effect=run_probe):
                result = qoder_accounts.run_runtime_info("cn", "account-测试")
            Path(sys.argv[3]).write_text(json.dumps(result), encoding="utf-8")
        """)
        # pythonw reproduces the packaged app's lack of a parent console.
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "bridge.json"
            completed = subprocess.run(
                [str(Path(sys.executable).with_name("pythonw.exe")), "-c", worker,
                 str(Path(__file__).resolve().parents[2]), sys.executable, str(output), probe],
                capture_output=True, timeout=15)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            result = json.loads(output.read_text(encoding="utf-8"))

        self.assertEqual(result, {"console_window": 0, "account": "account-测试"})

    def test_runtime_info_without_windows_flags_preserves_bridge_protocol(self):
        identity = {"machineToken": "test-token", "machineType": "test-type", "machineCode": "test-code"}
        completed = subprocess.CompletedProcess([], 0, json.dumps(identity).encode() + b"\nbridge log", b"")
        with mock.patch.object(qoder_accounts, "runtime_info_exe", return_value="/bridge/runtime-info"), \
                mock.patch.object(subprocess, "CREATE_NO_WINDOW", 0, create=True), \
                mock.patch.object(subprocess, "run", return_value=completed) as run:
            result = qoder_accounts.run_runtime_info("intl")

        self.assertEqual(result, identity)
        run.assert_called_once_with(
            ["/bridge/runtime-info", "prod", "--account-stdin"],
            input=b'{"account": ""} ', stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            timeout=25, creationflags=0, cwd="/bridge")


if __name__ == "__main__":
    unittest.main()
