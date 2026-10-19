"""Offline checks for browser identity and concurrent profile protection."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('browser_tools', Path(__file__).resolve().parents[1] / 'runtime/browser_tools.py')
browser = importlib.util.module_from_spec(spec)
spec.loader.exec_module(browser)


class BrowserTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()
        package = self.root / 'package'
        package.mkdir()
        (package / 'chromium-path.txt').write_text('/opt/chromium/browser')
        environment = patch.dict(os.environ, {'ENGINEBOX_BROWSER_HOME': str(self.root / 'private'),
                                             'ENGINEBOX_BROWSER_PACKAGE': str(package)}, clear=True)
        environment.start()
        self.addCleanup(environment.stop)

    def test_profiles_are_private_to_each_client_and_headless_by_default(self):
        codex_command, codex_state = browser.settings('codex')
        _, cursor_state = browser.settings('cursor')
        self.assertNotEqual(codex_state, cursor_state)
        self.assertIn('--headless', codex_command)
        self.assertIn('--sandbox', codex_command)
        self.assertNotIn('--no-sandbox', codex_command)
        self.assertNotIn('--port', codex_command)
        self.assertNotIn('--cdp-endpoint', codex_command)
        self.assertEqual(codex_command[codex_command.index('--snapshot-mode') + 1], 'none')

    def test_custom_profile_supports_concurrency_and_explicit_headed_mode(self):
        command, state = browser.settings('codex', 'task-2', headed=True)
        self.assertEqual(state.name, 'task-2')
        self.assertNotIn('--headless', command)

    def test_traversal_and_shell_text_are_not_profile_names(self):
        for value in ('../private', '/root', '--headless', 'x;id', 'x y', '', 'a' * 65):
            with self.subTest(value=value), self.assertRaises(ValueError):
                browser.settings(value)

    def test_profile_lock_rejects_concurrent_users_and_releases_after_close(self):
        _, state = browser.settings('codex')
        descriptor = browser.acquire_profile(state)
        try:
            with self.assertRaisesRegex(RuntimeError, 'in use'):
                browser.acquire_profile(state)
            self.assertEqual(state.stat().st_mode & 0o777, 0o700)
            self.assertEqual((state / 'profile').stat().st_mode & 0o777, 0o700)
        finally:
            os.close(descriptor)
        os.close(browser.acquire_profile(state))

    def test_profile_symlinks_cannot_escape_private_storage(self):
        private = self.root / 'private'
        private.mkdir()
        (private / 'codex').symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'symlink'):
            browser.settings('codex')


if __name__ == '__main__':
    unittest.main()
