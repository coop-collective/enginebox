#!/usr/bin/env python3
"""Build and smoke-test local Python artifacts; never publish or install host services."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
RUNTIME = ROOT / "runtime"


def run(*argv, cwd=ROOT, capture=False):
    return subprocess.run([str(item) for item in argv], cwd=cwd, check=True,
                          stdin=subprocess.DEVNULL, text=True, capture_output=capture,
                          timeout=600)


def python_in(venv: Path) -> Path:
    return venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")


def executable_in(venv: Path, name: str) -> Path:
    return venv / (f"Scripts/{name}.exe" if os.name == "nt" else f"bin/{name}")


def source_fingerprint() -> str:
    """Fingerprint package source inputs, including uncommitted edits; no credentials."""
    config = tomllib.loads((RUNTIME / "pyproject.toml").read_text())
    files = [RUNTIME / "pyproject.toml", RUNTIME / "uv.lock"]
    files += [RUNTIME / f"{name}.py" for name in config["tool"]["setuptools"]["py-modules"]]
    files += [RUNTIME / name for names in config["tool"]["setuptools"]["data-files"].values() for name in names]
    files += [RUNTIME / name for name in config["project"].get("license-files", [])]
    digest = hashlib.sha256()
    for path in sorted(set(files)):
        digest.update(str(path.relative_to(ROOT)).encode() + b"\0" + path.read_bytes() + b"\0")
    return digest.hexdigest()


def check_artifacts(wheel: Path, source: Path) -> None:
    config = tomllib.loads((RUNTIME / "pyproject.toml").read_text())
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        for module in config["tool"]["setuptools"]["py-modules"]:
            if f"{module}.py" not in names:
                raise ValueError(f"Wheel is missing declared module: {module}")
        for license_file in config["project"].get("license-files", []):
            if not any(name.endswith("/licenses/" + license_file) for name in names):
                raise ValueError(f"Wheel is missing license notice: {license_file}")
    with tarfile.open(source, "r:gz") as archive:
        names += archive.getnames()
    for name in names:
        parts = Path(name).parts
        if (".state" in parts or ".." in parts or Path(name).is_absolute()
                or any(part.startswith(".env") for part in parts)
                or Path(name).suffix in {".pem", ".key", ".qcow2", ".img", ".iso"}):
            raise ValueError("Artifact contains a forbidden private or machine-state path.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, required=True, help="New artifact directory; existing directories are refused.")
    args = parser.parse_args()
    destination = args.out_dir.resolve()
    if destination.exists():
        parser.error("Use a new output directory; existing artifacts are never overwritten.")
    if not shutil.which("uv"):
        parser.error("Install uv before building the package.")
    before = source_fingerprint()
    run("uv", "sync", "--project", RUNTIME, "--locked")
    test_python = python_in(RUNTIME / ".venv")
    run(test_python, "-m", "unittest", "discover", "-s", "tests", "-v")
    run(test_python, "-m", "unittest", "discover", "-s", "scripts/control-plane", "-p", "test_*.py", "-v")
    with tempfile.TemporaryDirectory(prefix="enginebox-package-") as temporary:
        scratch = Path(temporary)
        artifacts = scratch / "artifacts"
        run("uv", "build", RUNTIME, "--out-dir", artifacts)
        wheels = list(artifacts.glob("*.whl"))
        sources = list(artifacts.glob("*.tar.gz"))
        if len(wheels) != 1 or len(sources) != 1:
            raise ValueError("Expected exactly one wheel and source distribution.")
        wheel, source = wheels[0], sources[0]
        check_artifacts(wheel, source)
        venv = scratch / "venv"
        run("uv", "venv", "--python", sys.executable, venv)
        python = python_in(venv)
        # Install pinned, hashed dependencies before the built wheel. Source tree
        # and editable installs are not used for the smoke checks below.
        run("uv", "pip", "install", "--python", python, "--require-hashes", "-r", RUNTIME / "requirements.lock")
        run("uv", "pip", "install", "--python", python, "--no-deps", wheel)
        run("uv", "pip", "check", "--python", python)
        for command in [("ebox", "--help"), ("ebox", "doctor", "--help"),
                        ("enginebox-api", "--help"), ("enginebox-host", "--help")]:
            run(executable_in(venv, command[0]), *command[1:], cwd=scratch, capture=True)
        config = tomllib.loads((RUNTIME / "pyproject.toml").read_text())
        # -I ignores PYTHONPATH and user site packages. Import every declared
        # module so missing package dependencies/files fail before artifact copy.
        modules = config["tool"]["setuptools"]["py-modules"]
        run(python, "-I", "-c", "import importlib; "
            + f"[importlib.import_module(name) for name in {modules!r}]", cwd=scratch, capture=True)
        if source_fingerprint() != before:
            raise ValueError("Package source changed during validation. Retry after edits finish.")
        receipt = {"schema_version": 1, "source_sha256": before,
                   "validation_platform": {"system": platform.system(), "architecture": platform.machine(),
                                           "python": platform.python_version()},
                   "checks": ["runtime offline suite", "control-plane installer suite", "wheel and sdist contents",
                              "clean environment locked dependency install", "CLI dispatch", "all module imports"],
                   "host_installation_tested": False, "published": False,
                   "artifacts": {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in (wheel, source)}}
        # Never leave something looking like a validated release on a failed run.
        destination.mkdir(parents=True, exist_ok=False)
        for path in (wheel, source):
            shutil.copy2(path, destination / path.name)
        (destination / "validation.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"Validated local package artifacts: {destination}")
    print("Host service installation, platform qualification and package publication remain separate gates.")


if __name__ == "__main__":
    main()
