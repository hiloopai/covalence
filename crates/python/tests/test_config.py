"""The Rust-backed configuration API preserves paths, TOML values and errors."""

import datetime
import errno
import inspect
import math
import os
from pathlib import Path
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch

from covalence import config


class DirectoryTests(unittest.TestCase):
    def test_public_functions_are_named_for_the_public_module(self):
        for name in config.__all__:
            with self.subTest(name=name):
                function = getattr(config, name)
                self.assertEqual(function.__module__, "covalence.config")
                self.assertEqual(function.__name__, name)
                self.assertTrue(inspect.isbuiltin(function))

    def test_explicit_paths_are_unchanged_and_never_created(self):
        with tempfile.TemporaryDirectory() as temp:
            for lookup in self.lookups():
                path = Path(temp) / lookup.__name__ / "absent"
                with self.subTest(lookup=lookup.__name__):
                    self.assertEqual(lookup("../ignored", override_dir=path), path)
                    self.assertIsInstance(lookup("example", override_dir=str(path)), Path)
                    self.assertFalse(path.exists())
                    self.assertEqual(
                        lookup("", override_dir="relative/directory"),
                        Path("relative/directory"),
                    )

    def test_invalid_names_raise_value_error(self):
        for lookup in self.lookups():
            for name in ("", ".", "..", "../escape", "/absolute", "a/b", "a\\b", "C:drive", "a\0b"):
                with self.subTest(lookup=lookup.__name__, name=name):
                    with self.assertRaises(ValueError):
                        lookup(name)

    @staticmethod
    def lookups():
        return (config.config_dir, config.cache_dir, config.data_dir, config.state_dir, config.runtime_dir)

    @unittest.skipIf(os.name == "nt", "XDG directories are used on Unix")
    def test_xdg_values_and_runtime_fallback(self):
        with tempfile.TemporaryDirectory() as temp:
            env = {
                "HOME": temp,
                "XDG_CONFIG_HOME": str(Path(temp) / "config"),
                "XDG_CACHE_HOME": str(Path(temp) / "cache"),
                "XDG_DATA_HOME": str(Path(temp) / "data"),
                "XDG_STATE_HOME": str(Path(temp) / "state"),
                "XDG_RUNTIME_DIR": str(Path(temp) / "runtime"),
            }
            with patch.dict(os.environ, env, clear=True):
                for name in ("config", "cache", "data", "state", "runtime"):
                    with self.subTest(name=name):
                        self.assertEqual(getattr(config, name + "_dir")("example"), Path(temp) / name / "example")
                del os.environ["XDG_RUNTIME_DIR"]
                self.assertEqual(config.runtime_dir("example"), Path(temp) / "state" / "example")
            self.assertEqual(list(Path(temp).iterdir()), [])


class TomlTests(unittest.TestCase):
    def test_python_value_types_match_tomllib(self):
        text = '''
text = "hello"
flag = true
minimum = -9223372036854775808
maximum = 9223372036854775807
number = 3.5
local_date = 1979-05-27
local_time = 07:32:00.123456789
local_datetime = 1979-05-27T07:32:00.123456789
utc = 1979-05-27T07:32:00Z
offset = 1979-05-27T07:32:00-07:30
mixed = [1, "two", false]
[section]
items = [{name = "one"}, {name = "two"}]
'''
        result = config.parse(text)
        expected = tomllib.loads(text)
        self.assertEqual(result, expected)
        for name, value in result.items():
            with self.subTest(name=name):
                self.assertIs(type(value), type(expected[name]))
        self.assertIsInstance(result["local_date"], datetime.date)
        self.assertEqual(result["local_time"].microsecond, 123456)
        self.assertEqual(result["local_datetime"].microsecond, 123456)
        self.assertEqual(result["offset"].utcoffset(), datetime.timedelta(hours=-7, minutes=-30))

    def test_float_special_values_are_preserved(self):
        result = config.parse("infinity = inf\nnegative = -inf\nnot_a_number = nan\nzero = -0.0")
        self.assertEqual(result["infinity"], math.inf)
        self.assertEqual(result["negative"], -math.inf)
        self.assertTrue(math.isnan(result["not_a_number"]))
        self.assertEqual(math.copysign(1, result["zero"]), -1)

    def test_invalid_toml_and_integer_overflow_raise_value_error(self):
        for text in ("x =", "x = 1\nx = 2", "x = 9223372036854775808", "x = -9223372036854775809", "x = 1979-02-30"):
            with self.subTest(text=text):
                with self.assertRaises(ValueError):
                    config.parse(text)
        with self.assertRaises(TypeError):
            config.parse(b"x = 1")

    def test_read_defaults_only_for_missing_files(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "missing-parent" / "settings.toml"
            self.assertEqual(config.read(path), {})
            self.assertFalse(path.parent.exists())
            with self.assertRaises(FileNotFoundError) as raised:
                config.read_required(path)
            self.assertEqual(raised.exception.errno, errno.ENOENT)
            self.assertEqual(Path(raised.exception.filename), path)
            path.parent.mkdir()
            path.write_text('name = "example"', encoding="utf-8")
            self.assertEqual(config.read(path), {"name": "example"})
            self.assertEqual(config.read_required(str(path)), {"name": "example"})
            path.write_text("name =", encoding="utf-8")
            for read in (config.read, config.read_required):
                with self.subTest(read=read.__name__):
                    with self.assertRaisesRegex(ValueError, "settings.toml"):
                        read(path)
            path.write_bytes(b"\xff")
            with self.assertRaises(OSError):
                config.read(path)
            with self.assertRaises(OSError):
                config.read_required(path.parent)

    def test_path_overrides_preserve_non_utf8_names_on_unix(self):
        if os.name == "nt":
            self.skipTest("surrogate-escaped filenames are a Unix convention")
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / os.fsdecode(b"settings-\xff.toml")
            self.assertEqual(config.config_dir("example", override_dir=path), path)

    @unittest.skipUnless(sys.platform.startswith("linux"), "macOS filesystems reject invalid UTF-8 filenames")
    def test_read_preserves_non_utf8_names_on_linux(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / os.fsdecode(b"settings-\xff.toml")
            path.write_text("enabled = true", encoding="utf-8")
            self.assertEqual(config.read(path), {"enabled": True})


if __name__ == "__main__":
    unittest.main()
