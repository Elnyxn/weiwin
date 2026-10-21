"""Publish all verified assets together. Called only by the tag workflow's write-scoped job."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


def gh(*args, check=True):
    return subprocess.run(["gh", *args], text=True, capture_output=True, check=check)


def publish(directory, version, tag, repository):
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version) or tag != "v" + version:
        raise ValueError("tag and Cargo version differ")
    names = {f"weiwin-{version}-windows-x64-portable.zip",
             f"weiwin-{version}-windows-x64-setup.exe", "SHA256SUMS.txt"}
    actual = {path.name for path in directory.iterdir()}
    if actual != names:
        raise ValueError("release directory must contain exactly ZIP, installer and SHA256SUMS")
    hashes = {}
    for line in (directory / "SHA256SUMS.txt").read_text(encoding="utf-8").splitlines():
        digest, name = line.split("  ", 1)
        if not re.fullmatch(r"[a-f0-9]{64}", digest) or name not in names - {"SHA256SUMS.txt"} or name in hashes:
            raise ValueError("invalid checksum entry")
        hashes[name] = digest
    if set(hashes) != names - {"SHA256SUMS.txt"}:
        raise ValueError("both distributables require checksums")
    for name, expected in hashes.items():
        path = directory / name
        if path.stat().st_size == 0 or hashlib.sha256(path.read_bytes()).hexdigest() != expected:
            raise ValueError(f"checksum mismatch: {name}")

    def info():
        result = gh("release", "view", tag, "--repo", repository, "--json", "isDraft,assets", check=False)
        if result.returncode:
            return None
        return json.loads(result.stdout)

    def check_remote(release):
        assets = release["assets"]
        if {asset["name"] for asset in assets} != names or len(assets) != len(names):
            raise ValueError("remote release assets are incomplete")
        if any(asset["size"] != (directory / asset["name"]).stat().st_size for asset in assets):
            raise ValueError("remote release asset size mismatch")

    release = info()
    if release is not None and not release["isDraft"]:
        check_remote(release)
        print("complete release already published; leaving it unchanged")
        return
    if release is None:
        arguments = ["release", "create", tag, "--repo", repository, "--verify-tag", "--draft",
                     "--title", f"微win {version}", "--generate-notes"]
        if "-" in version:
            arguments.append("--prerelease")
        gh(*arguments)
    gh("release", "upload", tag, "--repo", repository, "--clobber",
       *(str(directory / name) for name in sorted(names)))
    release = info()
    if release is None or not release["isDraft"]:
        raise ValueError("release must remain a draft until all uploads are checked")
    check_remote(release)
    gh("release", "edit", tag, "--repo", repository, "--draft=false",
       "--prerelease=" + str("-" in version).lower(), "--latest=" + str("-" not in version).lower())
    print("published complete release: portable ZIP + installer + checksums")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--repository", required=True)
    args = parser.parse_args()
    publish(args.directory, args.version, args.tag, args.repository)


if __name__ == "__main__":
    main()
