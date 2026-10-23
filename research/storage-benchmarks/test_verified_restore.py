"""Integrity/publication regression checks for the research streaming restorer."""
import hashlib
import importlib.util
import io
import os
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('verified_restore', Path(__file__).with_name('verified-restore.py'))
restore = importlib.util.module_from_spec(spec)
spec.loader.exec_module(restore)


def manifest(files):
    return {name: {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
            for name, data in files.items()}


def archive(entries):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode='w') as tar:
        for name, data, kind in entries:
            entry = tarfile.TarInfo(name)
            entry.type = kind
            if kind in [tarfile.SYMTYPE, tarfile.LNKTYPE]:
                entry.linkname = '/outside'
            else:
                entry.size = len(data)
            tar.addfile(entry, io.BytesIO(data) if entry.isfile() else None)
    out.seek(0)
    return out


class RestoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.parent = Path(self.temp.name)
        self.dest = self.parent/'restore'
        self.files = {'empty': b'', 'models/a.uasset': b'asset data' * 10000}

    def tearDown(self):
        for current, _, _ in os.walk(self.parent):
            os.chmod(current, 0o700)
        self.temp.cleanup()

    def run_restore(self, entries, expected=None):
        return restore.restore(archive(entries), expected or manifest(self.files),
                               self.dest, 'assets', reserve_bytes=0)

    def entries(self):
        return [('assets/'+n, b, tarfile.REGTYPE) for n, b in self.files.items()]

    def test_publishes_complete_readonly_tree_then_audits_stored_bytes(self):
        result = self.run_restore(self.entries()+[('assets/nested/empty/directory', b'', tarfile.DIRTYPE)])
        root = Path(result['content_root'])
        self.assertTrue(result['content_verified'])
        self.assertFalse(result['readback_verified'])
        for name, data in self.files.items():
            self.assertEqual((root/name).read_bytes(), data)
        for path in [root, *root.rglob('*')]:
            self.assertEqual(path.stat().st_mode & 0o222, 0)
        for workers in [1, 4]:
            self.assertTrue(restore.audit(root, manifest(self.files), workers)['readback_verified'])
        # Simulate storage corruption after ingress verification. Audit must fail.
        target = root/'models/a.uasset'
        os.chmod(target, 0o644)
        target.write_bytes(b'wrong data' * 10000)
        with self.assertRaisesRegex(ValueError, 'checksum'):
            restore.audit(root, manifest(self.files), 4)

    def test_does_not_expose_content_while_reading_archive(self):
        payload = archive(self.entries()).getvalue()
        outer = self

        class ObservedStream(io.BytesIO):
            def read(self, size=-1):
                outer.assertFalse((outer.dest/'content').exists())
                return super().read(size)

        restore.restore(ObservedStream(payload), manifest(self.files), self.dest, 'assets', 0)
        self.assertTrue((self.dest/'content').exists())

    def test_corrupt_input_never_publishes(self):
        entries = self.entries()
        entries[1] = (entries[1][0], b'wrong data' * 10000, tarfile.REGTYPE)
        with self.assertRaisesRegex(ValueError, 'checksum'):
            self.run_restore(entries)
        self.assertFalse((self.dest/'content').exists())
        self.assertFalse((self.dest/'staging').exists())

    def test_missing_duplicate_extra_and_wrong_size_files_never_publish(self):
        cases = [self.entries()[:1], self.entries()+self.entries()[:1],
                 self.entries()+[('assets/extra', b'x', tarfile.REGTYPE)],
                 [('assets/empty', b'not empty', tarfile.REGTYPE)]]
        for i, entries in enumerate(cases):
            with self.subTest(case=i):
                self.dest = self.parent/f'case-{i}'
                with self.assertRaises(ValueError):
                    self.run_restore(entries)
                self.assertFalse((self.dest/'content').exists())

    def test_links_devices_traversal_and_wrong_root_are_rejected(self):
        cases = [('assets/link', b'', tarfile.SYMTYPE),
                 ('assets/link', b'', tarfile.LNKTYPE),
                 ('assets/device', b'', tarfile.CHRTYPE),
                 ('assets/../outside', b'x', tarfile.REGTYPE),
                 ('/absolute', b'x', tarfile.REGTYPE),
                 ('other/file', b'x', tarfile.REGTYPE)]
        for i, entry in enumerate(cases):
            with self.subTest(case=i):
                self.dest = self.parent/f'bad-{i}'
                with self.assertRaises(ValueError):
                    self.run_restore([entry])
                self.assertFalse((self.dest/'content').exists())
        self.assertFalse((self.parent/'outside').exists())

    def test_truncation_and_existing_destination_are_rejected(self):
        payload = archive(self.entries()).getvalue()
        with self.assertRaises((ValueError, tarfile.ReadError)):
            restore.restore(io.BytesIO(payload[:2000]), manifest(self.files), self.dest, 'assets', 0)
        self.assertFalse((self.dest/'content').exists())
        keep = self.dest/'keep'
        keep.write_text('existing workspace')
        with self.assertRaises(FileExistsError):
            self.run_restore(self.entries())
        self.assertEqual(keep.read_text(), 'existing workspace')

    def test_manifest_and_audit_cannot_escape_root(self):
        for name in ['../escape', '/absolute', 'a/../b', 'a//b', 'a\\b']:
            with self.subTest(path=name), self.assertRaises(ValueError):
                restore.validate_manifest(manifest({name: b'x'}))
        with self.assertRaises(ValueError):
            restore.validate_manifest(manifest({'a': b'x', 'a/b': b'y'}))
        result = self.run_restore(self.entries())
        root = Path(result['content_root'])
        os.chmod(root, 0o755)
        (root/'escape').symlink_to(self.parent, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'symlink'):
            restore.audit(root, manifest(self.files))


if __name__ == '__main__':
    unittest.main()
