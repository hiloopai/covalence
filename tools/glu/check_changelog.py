"""Check this repository's Keep a Changelog conventions, without network access."""

import datetime
from pathlib import Path
import re
import sys

REPOSITORY = "https://github.com/hiloopai/covalence"
FORMAT = "https://keepachangelog.com/en/2.0.0/"
CATEGORIES = {"Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"}
VERSION = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
# The first Python release predates the public source tags.
PYPI_ONLY = "0.1.0-dev.1"
PYPI_LINK = "https://pypi.org/project/covalence/0.1.0.dev1/"


def version_key(version):
    match = VERSION.fullmatch(version)
    if not match:
        raise ValueError(f"invalid SemVer version: {version}")
    major, minor, patch, prerelease = match.groups()
    identifiers = prerelease.split(".") if prerelease else []
    if any(part.isdigit() and len(part) > 1 and part.startswith("0") for part in identifiers):
        raise ValueError(f"numeric prerelease identifiers cannot have leading zeros: {version}")
    suffix = (0, tuple((0, int(p)) if p.isdigit() else (1, p) for p in identifiers))
    return (int(major), int(minor), int(patch), suffix if prerelease else (1, ()))


def check(text):
    """Raise ValueError for malformed headings, entries, ordering or release links."""
    lines = text.splitlines()
    if not lines or lines[0] != "# Changelog" or FORMAT not in text:
        raise ValueError(f"start with '# Changelog' and link to {FORMAT}")
    sections, links = [], {}
    category = None
    for number, line in enumerate(lines, 1):
        reference = re.fullmatch(r"\[([^]]+)\]:\s+(\S+)\s*", line)
        if reference:
            name, url = reference.groups()
            if name in links:
                raise ValueError(f"line {number}: duplicate link reference {name}")
            links[name] = url
        elif line.startswith("## "):
            heading = re.fullmatch(r"## \[([^]]+)\](?: - (\d{4}-\d{2}-\d{2})(?: \[YANKED\])?)?", line)
            if not heading:
                raise ValueError(f"line {number}: invalid release heading")
            version, date = heading.groups()
            if version == "Unreleased":
                if sections or date:
                    raise ValueError("Unreleased must appear once, first, without a date")
            else:
                version_key(version)
                if not date:
                    raise ValueError(f"{version}: missing ISO release date")
                datetime.date.fromisoformat(date)
            sections.append({"version": version, "date": date, "categories": {}})
            category = None
        elif line.startswith("### "):
            category = line[4:]
            if not sections or category not in CATEGORIES or category in sections[-1]["categories"]:
                raise ValueError(f"line {number}: unknown, misplaced or duplicate category")
            sections[-1]["categories"][category] = 0
        elif line.startswith("#") and number != 1:
            raise ValueError(f"line {number}: invalid heading")
        elif line.startswith("- "):
            if category is None or not line[2:].strip():
                raise ValueError(f"line {number}: put nonempty entries under a change category")
            sections[-1]["categories"][category] += 1
    versions = [section["version"] for section in sections]
    if len(versions) < 2 or versions[0] != "Unreleased" or len(set(versions)) != len(versions):
        raise ValueError("require one leading Unreleased and unique released versions")
    releases = sections[1:]
    for newer, older in zip(releases, releases[1:]):
        if version_key(newer["version"]) <= version_key(older["version"]):
            raise ValueError("release versions must be newest first")
        if newer["date"] < older["date"]:
            raise ValueError("release dates must be newest first")
    for index, section in enumerate(sections):
        version, categories = section["version"], section["categories"]
        if (index and not categories) or any(count == 0 for count in categories.values()):
            raise ValueError(f"{version}: change categories must contain entries")
        if version == "Unreleased":
            expected = {f"{REPOSITORY}/compare/v{versions[1]}...HEAD"}
        elif version == PYPI_ONLY:
            expected = {PYPI_LINK}
        else:
            if index + 1 < len(versions) and versions[index + 1] != PYPI_ONLY:
                expected = {f"{REPOSITORY}/compare/v{versions[index + 1]}...v{version}"}
            else:
                expected = {f"{REPOSITORY}/releases/tag/v{version}"}
        if links.get(version) not in expected:
            raise ValueError(f"{version}: missing or stale release/comparison link")


if __name__ == "__main__":
    path = Path(__file__).resolve().parents[2] / "CHANGELOG.md"
    try:
        check(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        sys.exit(f"{path}: {error}")
    print("Changelog format checked (release accuracy still requires review).")
