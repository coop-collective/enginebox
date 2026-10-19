"""Policy attachment and preservation regression tests; live boundary evidence is documented separately."""
from pathlib import Path
import tempfile
import unittest
import sys
import subprocess
import time

from cursor_sandbox import check_cursor_version, check_loaded_profiles, check_preflight, prepare_profile, profile_text, PROFILE_FILE
from bootstrap import PYTHON_ASSETS


class CursorSandboxTests(unittest.TestCase):
    def test_attachment_cannot_expand_a_home_into_other_users_or_policy_syntax(self):
        for home in ["/", "//", "/.", "relative", "/home/*", "/home/../other", "/home/{one,two}", '/home/"bad', "/home/bad\n"]:
            with self.subTest(home=home), self.assertRaises(ValueError):
                profile_text(home)
        text = profile_text("/srv/private/agent-one")
        self.assertIn('"/srv/private/agent-one/.local/share/cursor-agent/versions/*/cursorsandbox"', text)
        self.assertIn("  userns,", text)
        self.assertIn("signal (send, receive) peer=enginebox_cursor_sandbox", text)
        self.assertNotIn("/home/*", text)
        self.assertIn("cursor_sandbox.py", PYTHON_ASSETS)

    def test_dry_run_and_reapply_preserve_matching_operator_policy(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / PROFILE_FILE
            result = prepare_profile("/home/agent", root)
            self.assertTrue(result["created"])
            self.assertFalse(path.exists())
            prepare_profile("/home/agent", root, apply=True)
            self.assertEqual(path.stat().st_mode & 0o777, 0o644)
            value = path.read_text() + "\n# operator comment retained\n"
            path.write_text(value)
            self.assertFalse(prepare_profile("/home/agent", root, apply=True)["created"])
            self.assertEqual(path.read_text(), value)
            with self.assertRaisesRegex(ValueError, "unexpected attachment"):
                prepare_profile("/home/someone-else", root, apply=True)
            self.assertEqual(path.read_text(), value)

    def test_mode_overrides_symlinks_and_foreign_profiles_fail_without_changes(self):
        for mode in ("disable", "force-complain"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / mode).mkdir()
                (root / mode / PROFILE_FILE).symlink_to(root / "nonexistent")
                with self.assertRaisesRegex(ValueError, "mode override"):
                    prepare_profile("/home/agent", root, apply=True)
                self.assertFalse((root / PROFILE_FILE).exists())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / PROFILE_FILE
            path.symlink_to(root / "unrelated")
            with self.assertRaisesRegex(ValueError, "non-regular"):
                prepare_profile("/home/agent", root, apply=True)
            self.assertFalse((root / "unrelated").exists())
            path.unlink()
            path.write_text("profile unrelated { file, }\n")
            path.chmod(0o644)
            with self.assertRaisesRegex(ValueError, "unexpected attachment"):
                prepare_profile("/home/agent", root, apply=True)
            self.assertEqual(path.read_text(), "profile unrelated { file, }\n")
            path.write_text(profile_text("/home/agent"))
            path.chmod(0o666)
            with self.assertRaisesRegex(ValueError, "not writable by others"):
                prepare_profile("/home/agent", root, apply=True)

    def test_existing_loaded_policy_is_not_replaced_from_another_source(self):
        with tempfile.TemporaryDirectory() as directory:
            canonical = Path(directory) / PROFILE_FILE
            with self.assertRaisesRegex(ValueError, "another source"):
                check_loaded_profiles("enginebox_cursor_sandbox (enforce)\n", canonical)
            with self.assertRaisesRegex(ValueError, "operator Cursor"):
                check_loaded_profiles("cursor_sandbox_agent_cli (enforce)\n", canonical)
            prepare_profile("/home/agent", Path(directory), apply=True)
            check_loaded_profiles("enginebox_cursor_sandbox (enforce)\n", canonical)

    def test_version_identity_rejects_an_unrelated_agent_and_bounds_output(self):
        self.assertEqual(check_cursor_version([sys.executable, "-c", "print('2026.10.16-e3cb207')"]), "2026.10.16-e3cb207")
        with self.assertRaisesRegex(ValueError, "does not identify as Cursor"):
            check_cursor_version([sys.executable, "-c", "print('grok 1.0.40')"])
        with self.assertRaisesRegex(ValueError, "Unexpected native version"):
            check_cursor_version([sys.executable, "-c", "print('x'*100000)"])

    def test_hung_model_free_preflight_terminates_its_child_group(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "survived"
            source = "import os, pathlib, sys, time\nif os.fork() == 0:\n time.sleep(.8)\n pathlib.Path(sys.argv[1]).write_text('bad')\n sys.exit(0)\ntime.sleep(10)\n"
            with self.assertRaises(subprocess.TimeoutExpired):
                check_preflight([sys.executable, "-c", source, str(marker)], timeout=.2)
            time.sleep(.8)
            self.assertFalse(marker.exists())


if __name__ == "__main__":
    unittest.main()
