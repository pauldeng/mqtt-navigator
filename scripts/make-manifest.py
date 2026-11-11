#!/usr/bin/env python3
"""Build latest.json for a release (ADR 0005, D3). Standard library only.

Usage: python3 -I scripts/make-manifest.py --version 0.1.0 --base-url URL --out latest.json \
           TRIPLE=PATH [TRIPLE=PATH ...]

PATH is the update asset of that target. Its file name is appended to --base-url.
The output bytes are the bytes that CI signs: do not rewrite the file after signing.
"""
import argparse
import hashlib
import json
import os
import re
import sys

SEMVER = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$")
TRIPLE = re.compile(r"^[a-z0-9_]+-[a-z0-9_]+-[a-z0-9_]+(-[a-z0-9_]+)?$")


def entry(base_url, path):
    h = hashlib.sha256()
    size = 0
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
            size += len(chunk)
    if size == 0:
        sys.exit(f"empty artifact: {path}")
    return {
        "url": f"{base_url.rstrip('/')}/{os.path.basename(path)}",
        "sha256": h.hexdigest(),
        "size": size,
    }


def build(version, base_url, artifacts):
    if not SEMVER.match(version):
        sys.exit(f"bad version: {version}")
    if not base_url.startswith("https://"):
        sys.exit("base-url must start with https://")
    targets = {}
    for item in artifacts:
        triple, sep, path = item.partition("=")
        if not sep or not TRIPLE.match(triple):
            sys.exit(f"expected TRIPLE=PATH, got: {item}")
        if triple in targets:
            sys.exit(f"duplicate target: {triple}")
        targets[triple] = entry(base_url, path)
    if not targets:
        sys.exit("no artifacts")
    doc = {"version": version, "targets": targets}
    return (json.dumps(doc, indent=2, sort_keys=True) + "\n").encode()


def self_check():
    import tempfile

    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "a.zip")
        with open(p, "wb") as f:
            f.write(b"abc")
        out = json.loads(build("1.2.3", "https://x.test/dl/", [f"x86_64-pc-windows-msvc={p}"]))
        t = out["targets"]["x86_64-pc-windows-msvc"]
        assert out["version"] == "1.2.3"
        assert t["url"] == "https://x.test/dl/a.zip" and t["size"] == 3
        assert t["sha256"] == hashlib.sha256(b"abc").hexdigest()


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--version")
    ap.add_argument("--base-url")
    ap.add_argument("--out")
    ap.add_argument("--self-check", action="store_true")
    ap.add_argument("artifacts", nargs="*")
    a = ap.parse_args()
    if a.self_check:
        self_check()
        print("ok")
        sys.exit(0)
    if not (a.version and a.base_url and a.out):
        ap.error("--version, --base-url and --out are required")
    data = build(a.version, a.base_url, a.artifacts)
    with open(a.out, "wb") as f:
        f.write(data)
