"""The installed type stubs match the runtime, and type-check typical use."""

import importlib.util
from pathlib import Path
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

import covalence

TESTS = Path(__file__).resolve().parent


def mypy(module, script):
    """A command running mypy's `module`, or None if mypy is not installed.

    Uses mypy from this interpreter if it has it, else the `script` on PATH
    (as in the Nix dev shell, where glu's test venv cannot import mypy).
    """
    if importlib.util.find_spec("mypy") is not None:
        return [sys.executable, "-m", module]
    path = shutil.which(script)
    return None if path is None else [path]


class StubTests(unittest.TestCase):
    def run_mypy(self, module, script, *args):
        command = mypy(module, script)
        if command is None:
            self.skipTest("mypy is not installed; the Nix dev shell provides it")
        # A mypy outside this venv still imports the installed covalence.
        site_packages = str(Path(covalence.__file__).resolve().parent.parent)
        env = {**os.environ, "PYTHONPATH": site_packages}
        with tempfile.TemporaryDirectory() as cwd:
            result = subprocess.run(
                [*command, *args], cwd=cwd, env=env, capture_output=True, text=True
            )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_stubtest(self):
        self.run_mypy("mypy.stubtest", "stubtest", "covalence")

    def test_strict_usage(self):
        self.run_mypy(
            "mypy",
            "mypy",
            "--strict",
            "--cache-dir",
            os.devnull,
            "--python-executable",
            sys.executable,
            str(TESTS / "typed_usage.py"),
        )


if __name__ == "__main__":
    unittest.main()
