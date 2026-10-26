#!/usr/bin/env python3
"""Regression tests for scripts/verify-macos-dmg.py and scripts/make-test-dmg.py.

The DMG that shipped as 0.11.2 could not be mounted on any Mac, because the
container that produced it stored the compressed blocks of the data fork out
of order, so macOS read the partition from the wrong offsets. These tests pin
that failure and the healthy layout that replaces it, so the packaging step
cannot silently regress again.

Run with: python scripts/test-macos-dmg-layout.py
"""
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
VERIFY = ROOT / "scripts" / "verify-macos-dmg.py"
MAKE = ROOT / "scripts" / "make-test-dmg.py"
SHIPPED_BROKEN = [
    ROOT / "dist" / "MultiGateway-0.11.2-macos-arm64.dmg",
    ROOT / "dist" / "MultiGateway-0.11.2-macos-x64.dmg",
]

# kind -> (expected exit code, substrings that must appear in stderr)
CASES = {
    "good": (0, []),
    "good-apfs": (0, []),
    "hdiutil-layout": (0, []),
    "packed-tables": (1, ["out of alignment"]),
    "unclaimed-bytes": (1, ["no partition block points at"]),
    "short-datafork": (1, ["decompress"]),
    "missing-koly": (1, ["koly"]),
    "no-app": (1, ["MultiGateway.app"]),
    "bad-plist": (1, ["Info.plist"]),
}


def run(script, *args):
    return subprocess.run(
        [sys.executable, str(script), *map(str, args)],
        capture_output=True, text=True, timeout=120,
    )


class DMGLayoutTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory(prefix="dmg-layout-")
        cls.tmp = pathlib.Path(cls._tmp.name)

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    def fixture(self, kind):
        path = self.tmp / f"{kind}.dmg"
        if not path.exists():
            result = run(MAKE, "--kind", kind, "--output", path)
            self.assertEqual(result.returncode, 0, result.stderr)
        return path

    def test_fixture_matrix(self):
        for kind, (expected, needles) in CASES.items():
            with self.subTest(kind=kind):
                result = run(VERIFY, self.fixture(kind))
                self.assertEqual(result.returncode, expected,
                                 f"{kind}: unexpected verdict\n{result.stderr}")
                for needle in needles:
                    self.assertIn(needle.lower(), result.stderr.lower(),
                                  f"{kind}: expected {needle!r} in stderr")

    def test_good_image_reports_success(self):
        result = run(VERIFY, self.fixture("good"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("OK:", result.stdout)
        self.assertIn("Applications", result.stdout)

    def test_good_bundle_metadata_is_accepted(self):
        """Info.plist checks must actually run for a well-formed bundle."""
        result = run(VERIFY, self.fixture("good"))
        self.assertNotIn("no application bundle", result.stdout)

    def test_alignment_defect_is_named(self):
        """The 0.11.2 regression must be reported as an alignment problem."""
        result = run(VERIFY, self.fixture("packed-tables"))
        self.assertEqual(result.returncode, 1)
        self.assertRegex(result.stderr, r"starts at byte \d+ of the table area")

    def test_hdiutil_layout_is_accepted(self):
        """hdiutil keeps its tables inline, which is not an alignment defect."""
        result = run(VERIFY, self.fixture("hdiutil-layout"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("out of alignment", result.stderr)

    def test_unclaimed_data_fork_bytes_are_rejected(self):
        """A block that no table points at is the defect that shipped as 0.11.2."""
        result = run(VERIFY, self.fixture("unclaimed-bytes"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("no partition block points at", result.stderr)

    def test_packed_tables_offset_is_not_a_sector_multiple(self):
        result = run(MAKE, "--kind", "packed-tables", "--output",
                     self.tmp / "packed-tables.dmg", "--check")
        self.assertEqual(result.returncode, 0, result.stderr)

    @unittest.skipUnless(all(path.exists() for path in SHIPPED_BROKEN),
                         "0.11.2 artifacts are not present in dist/")
    def test_shipped_0_11_2_dmgs_stay_rejected(self):
        for path in SHIPPED_BROKEN:
            with self.subTest(dmg=path.name):
                result = run(VERIFY, path)
                self.assertEqual(result.returncode, 1,
                                 f"{path.name} must not be reported as installable")
                self.assertIn("cannot be installed", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
