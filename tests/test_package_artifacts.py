"""Package output cannot silently omit modules/notices or ship machine state."""
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("package_builder", Path(__file__).resolve().parents[1] / "scripts/build-tested-package.py")
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


class PackageArtifactTests(unittest.TestCase):
    def artifacts(self, directory, *, module=True, notice=True, extra=None):
        root = Path(directory)
        (root / "pyproject.toml").write_text('[project]\nlicense-files = ["NOTICE"]\n[tool.setuptools]\npy-modules = ["enginebox"]\n')
        wheel, source = root / "example.whl", root / "example.tar.gz"
        with zipfile.ZipFile(wheel, "w") as archive:
            if module:
                archive.writestr("enginebox.py", "# fixture")
            if notice:
                archive.writestr("example.dist-info/licenses/NOTICE", "fixture notice")
        with tarfile.open(source, "w:gz") as archive:
            info = tarfile.TarInfo(extra or "example/enginebox.py")
            content = b"fixture"
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
        return root, wheel, source

    def test_declared_modules_and_license_notices_are_required(self):
        for missing in [{"module": False}, {"notice": False}]:
            with self.subTest(missing=missing), tempfile.TemporaryDirectory() as directory:
                root, wheel, source = self.artifacts(directory, **missing)
                with patch.object(builder, "RUNTIME", root), self.assertRaisesRegex(ValueError, "missing"):
                    builder.check_artifacts(wheel, source)

    def test_credentials_machine_images_and_traversal_are_rejected(self):
        for path in ["example/.state/accounts.json", "example/.env.production", "example/secret.key",
                     "example/vm.qcow2", "example/../outside.py", "/outside.py"]:
            with self.subTest(path=path), tempfile.TemporaryDirectory() as directory:
                root, wheel, source = self.artifacts(directory, extra=path)
                with patch.object(builder, "RUNTIME", root), self.assertRaisesRegex(ValueError, "forbidden"):
                    builder.check_artifacts(wheel, source)

    def test_clean_artifacts_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            root, wheel, source = self.artifacts(directory)
            with patch.object(builder, "RUNTIME", root):
                builder.check_artifacts(wheel, source)


if __name__ == "__main__":
    unittest.main()
