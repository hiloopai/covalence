"""Regression cases for changelog structure and offline link validation."""

import unittest

from check_changelog import check

VALID = """# Changelog

Based on https://keepachangelog.com/en/2.0.0/.

## [Unreleased]

## [0.1.0-dev.3] - 2026-10-17

### Fixed

- Fix an upload.

## [0.1.0-dev.2] - 2026-10-14

### Added

- Public Rust sources.

## [0.1.0-dev.1] - 2026-10-13

### Added

- Initial Python package.

[Unreleased]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.3...HEAD
[0.1.0-dev.3]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.2...v0.1.0-dev.3
[0.1.0-dev.2]: https://github.com/hiloopai/covalence/releases/tag/v0.1.0-dev.2
[0.1.0-dev.1]: https://pypi.org/project/covalence/0.1.0.dev1/
"""


class ChangelogTests(unittest.TestCase):
    def test_accepts_empty_unreleased_and_historical_pypi_release(self):
        check(VALID)
        check(VALID.replace("2026-10-14", "2026-10-17"))
        check(VALID.replace(" - 2026-10-17", " - 2026-10-17 [YANKED]"))

    def test_rejects_malformed_headings_and_dates(self):
        for old, new in [
            ("# Changelog", "# History"),
            ("https://keepachangelog.com/en/2.0.0/", "https://keepachangelog.com/en/1.1.0/"),
            ("## [Unreleased]", "## [Unreleased] - 2026-10-18"),
            ("## [0.1.0-dev.3]", "## 0.1.0-dev.3"),
            ("2026-10-17", "2026-02-30"),
            ("0.1.0-dev.3", "0.1.0-dev.03"),
        ]:
            with self.subTest(new=new), self.assertRaises(ValueError):
                check(VALID.replace(old, new))

    def test_rejects_duplicate_versions_and_bad_order(self):
        for old, new in [("dev.3", "dev.2"), ("dev.3", "dev.0"), ("2026-10-17", "2026-10-12")]:
            with self.subTest(new=new), self.assertRaises(ValueError):
                check(VALID.replace(old, new))

    def test_rejects_unknown_duplicate_and_empty_categories(self):
        for old, new in [
            ("### Fixed", "### Bug fixes"),
            ("### Fixed", "### Fixed\n\n- First fix.\n\n### Fixed"),
            ("- Fix an upload.", ""),
            ("### Fixed\n\n- Fix an upload.", ""),
        ]:
            with self.subTest(new=new), self.assertRaises(ValueError):
                check(VALID.replace(old, new))

    def test_rejects_missing_stale_and_invented_source_links(self):
        for old, new in [
            ("[Unreleased]:", "[Missing]:"),
            ("...HEAD", "...v0.1.0-dev.2"),
            ("compare/v0.1.0-dev.2...v0.1.0-dev.3", "releases/tag/v0.1.0-dev.3"),
            ("compare/v0.1.0-dev.2...v0.1.0-dev.3", "compare/v0.1.0-dev.1...v0.1.0-dev.3"),
            ("releases/tag/v0.1.0-dev.2", "compare/v0.1.0-dev.1...v0.1.0-dev.2"),
            ("https://pypi.org/", "http://pypi.org/"),
        ]:
            with self.subTest(new=new), self.assertRaises(ValueError):
                check(VALID.replace(old, new))


if __name__ == "__main__":
    unittest.main()
