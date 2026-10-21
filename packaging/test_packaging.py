"""Packaging safety checks use synthetic files and mocked gh; no service or GitHub calls."""

import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import publish_release
import verify_payload


class PackagingTests(unittest.TestCase):
    def test_zip_rejects_sensitive_extra_and_nested_files(self):
        with tempfile.TemporaryDirectory() as root:
            archive = Path(root) / "portable.zip"
            with zipfile.ZipFile(archive, "w") as output:
                for name in verify_payload.payload_names("1.2.3"):
                    output.writestr(name, b"test payload")
            verify_payload.check_zip(archive, "1.2.3")
            with zipfile.ZipFile(archive, "a") as output:
                output.writestr("private/device.json", b"must never ship")
            with self.assertRaises(AssertionError):
                verify_payload.check_zip(archive, "1.2.3")

    def test_publishing_keeps_draft_until_both_uploads_are_visible(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            files = ["weiwin-1.2.3-windows-x64-portable.zip", "weiwin-1.2.3-windows-x64-setup.exe"]
            sums = []
            for name in files:
                data = ("synthetic " + name).encode()
                (directory / name).write_bytes(data)
                sums.append(hashlib.sha256(data).hexdigest() + "  " + name)
            (directory / "SHA256SUMS.txt").write_text("\n".join(sums) + "\n")
            calls = []

            def fake_gh(*args, **kwargs):
                calls.append(args)
                if args[:2] == ("release", "view"):
                    if len(calls) == 1:
                        return subprocess.CompletedProcess(args, 1, "", "not found")
                    assets = [{"name": p.name, "size": p.stat().st_size} for p in directory.iterdir()]
                    return subprocess.CompletedProcess(args, 0, json.dumps({"isDraft": True, "assets": assets}), "")
                return subprocess.CompletedProcess(args, 0, "", "")

            with patch.object(publish_release, "gh", fake_gh):
                publish_release.publish(directory, "1.2.3", "v1.2.3", "owner/repo")
            self.assertIn("--draft", calls[1])
            self.assertEqual(calls[2][:2], ("release", "upload"))
            self.assertEqual(calls[-1][:2], ("release", "edit"))
            self.assertIn("--draft=false", calls[-1])

    def test_bad_checksum_never_contacts_github(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            for suffix in ["portable.zip", "setup.exe"]:
                (directory / ("weiwin-1.2.3-windows-x64-" + suffix)).write_bytes(b"changed")
            (directory / "SHA256SUMS.txt").write_text("0" * 64 + "  weiwin-1.2.3-windows-x64-portable.zip\n")
            with patch.object(publish_release, "gh") as command:
                with self.assertRaises(ValueError):
                    publish_release.publish(directory, "1.2.3", "v1.2.3", "owner/repo")
                command.assert_not_called()

    def test_missing_remote_installer_never_publishes_draft(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            sums = []
            for suffix in ["portable.zip", "setup.exe"]:
                name = "weiwin-1.2.3-windows-x64-" + suffix
                (directory / name).write_bytes(b"synthetic")
                sums.append(hashlib.sha256(b"synthetic").hexdigest() + "  " + name)
            (directory / "SHA256SUMS.txt").write_text("\n".join(sums) + "\n")
            calls = []

            def incomplete(*args, **kwargs):
                calls.append(args)
                if args[:2] == ("release", "view"):
                    assets = [{"name": p.name, "size": p.stat().st_size}
                              for p in directory.iterdir() if p.suffix != ".exe"]
                    return subprocess.CompletedProcess(args, 0, json.dumps({"isDraft": True, "assets": assets}), "")
                return subprocess.CompletedProcess(args, 0, "", "")

            with patch.object(publish_release, "gh", incomplete):
                with self.assertRaises(ValueError):
                    publish_release.publish(directory, "1.2.3", "v1.2.3", "owner/repo")
            self.assertFalse(any(args[:2] == ("release", "edit") for args in calls))


if __name__ == "__main__":
    unittest.main()
