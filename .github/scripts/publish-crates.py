"""Publish the workspace crates to crates.io, resuming after rate limits.

crates.io allows a burst of 5 new crates, then 1 every 10 minutes.
`cargo publish` stops at the first 429 and refuses to upload a version that
already exists. So each round excludes the crates whose version is already in
the index and runs `cargo publish --workspace` for the rest. After a 429 it
waits and tries again; any other failure stops the script.
"""

import json
import subprocess
import sys
import time
import urllib.error
import urllib.request

RETRY_SECONDS = 11 * 60


def publishable():
    """Return (name, version) for each workspace member that may be published."""
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    meta = json.loads(out)
    members = set(meta["workspace_members"])
    # `publish = false` gives []; `publish = true` gives null.
    return [
        (p["name"], p["version"])
        for p in meta["packages"]
        if p["id"] in members and p["publish"] is None
    ]


def index_path(name):
    """Return a crate's path in the sparse index."""
    if len(name) <= 2:
        return f"{len(name)}/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


def in_index(name, version):
    """Return whether crates.io's index lists this version of the crate."""
    url = f"https://index.crates.io/{index_path(name.lower())}"
    try:
        with urllib.request.urlopen(url, timeout=30) as response:
            lines = response.read().decode().splitlines()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return False
        raise
    return any(json.loads(line)["vers"] == version for line in lines if line)


def main():
    crates = publishable()
    while True:
        done = [name for name, version in crates if in_index(name, version)]
        pending = [name for name, _ in crates if name not in done]
        if not pending:
            print("all crates are published")
            return 0
        print(f"publishing: {' '.join(pending)}", flush=True)
        command = ["cargo", "publish", "--workspace", "--locked"]
        for name in done:
            command += ["--exclude", name]
        result = subprocess.run(command, stderr=subprocess.PIPE, text=True)
        sys.stderr.write(result.stderr)
        if result.returncode == 0:
            continue
        if "status 429" not in result.stderr:
            return result.returncode
        print(f"rate limited; retrying in {RETRY_SECONDS} s", flush=True)
        time.sleep(RETRY_SECONDS)


if __name__ == "__main__":
    sys.exit(main())
