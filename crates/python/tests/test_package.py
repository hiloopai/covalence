"""Acceptance tests for the installed wheel; run from outside the source tree."""

import importlib.metadata
import importlib.resources
from pathlib import Path
import unittest

import covalence
import covalence._native as native_extension


class InstalledPackageTests(unittest.TestCase):
    def test_imports_come_from_installed_distribution(self):
        for module in (covalence, native_extension):
            self.assertIn("site-packages", Path(module.__file__).resolve().parts)
        self.assertTrue(native_extension.__file__.endswith((".so", ".pyd")))

    def test_version_matches_distribution_metadata(self):
        self.assertIsInstance(covalence.__version__, str)
        self.assertEqual(covalence.__version__, native_extension.__version__)
        self.assertEqual(covalence.__version__, importlib.metadata.version("covalence"))

    def test_type_information_is_packaged(self):
        package = importlib.resources.files("covalence")
        self.assertTrue(package.joinpath("py.typed").is_file())
        stub = package.joinpath("_native.pyi").read_text(encoding="utf-8")
        self.assertIn("__version__: str", stub)


if __name__ == "__main__":
    unittest.main()
