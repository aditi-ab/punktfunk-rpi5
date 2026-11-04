"""Exercise update integrity, atomic activation and failure recovery without services."""
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
from subprocess import CompletedProcess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

loader = importlib.machinery.SourceFileLoader("kiosk_update", str(Path(__file__).parent / "runtime/kiosk-update"))
spec = importlib.util.spec_from_loader(loader.name, loader)
update = importlib.util.module_from_spec(spec)
loader.exec_module(update)


class UpdateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        paths = {"PREFIX": self.root / "current", "RELEASES": self.root / "versions",
                 "RUNTIME": self.root / "runtime", "STATE": self.root / "private/state.json",
                 "LOCK": self.root / "update.lock"}
        for key, value in paths.items():
            self.enterContext(patch.object(update, key, value))
        update.RUNTIME.write_text("raspios-trixie-arm64-v1\n")
        update.RELEASES.mkdir()
        self.previous = self.app("0.46.0-rpi5.28")
        update.PREFIX.symlink_to(self.previous)
        self.manifest = {"format": 1, "platform": "rpi5-arm64", "runtime": "raspios-trixie-arm64-v1",
                         "version": "0.46.0-rpi5.29", "commit": "a" * 40,
                         "archive": "punktfunk-0.46.0-rpi5.29-linux-arm64.tar.gz"}
        self.payload = self.archive()
        self.manifest.update(size=len(self.payload), sha256=hashlib.sha256(self.payload).hexdigest())
        self.enterContext(patch.object(update.platform, "machine", return_value="aarch64"))
        self.enterContext(patch.object(update.shutil, "disk_usage", return_value=type("Disk", (), {"free": 5 * 1024**3})()))
        self.command = self.enterContext(patch.object(update, "command", return_value=CompletedProcess([], 0, "", "")))
        self.enterContext(patch.object(update, "fetch", side_effect=self.fetch))
        self.health = self.enterContext(patch.object(update, "healthy", return_value=True))

    def app(self, version):
        path = update.RELEASES / version
        path.mkdir()
        (path / "build-version").write_text(version)
        (path / "fork-commit").write_text("a" * 40)
        for name in ("punktfunk", "punktfunk-session"):
            (path / name).write_text("#!/bin/sh\nexit 0\n")
            (path / name).chmod(0o755)
        return path

    def archive(self, malicious=False):
        buffer = io.BytesIO()
        base = "punktfunk-0.46.0-rpi5.29-linux-arm64"
        files = {"build-version": b"0.46.0-rpi5.29", "fork-commit": b"a" * 40,
                 "punktfunk": b"#!/bin/sh\nexit 0\n", "punktfunk-session": b"#!/bin/sh\nexit 0\n"}
        if malicious:
            files["../../../../escaped"] = b"must not escape"
        with tarfile.open(fileobj=buffer, mode="w:gz") as package:
            for name, data in files.items():
                info = tarfile.TarInfo(f"{base}/{name}")
                info.size = len(data)
                info.mode = 0o755
                package.addfile(info, io.BytesIO(data))
        return buffer.getvalue()

    def fetch(self, url, limit, output=None):
        if url == update.API:
            data = json.dumps({"tag_name": "v0.46.0-rpi5.29", "draft": False, "prerelease": False,
                               "assets": [{"name": self.manifest["archive"] + ".update.json"}]}).encode()
        elif url.endswith(".update.json"):
            data = json.dumps(self.manifest).encode()
        else:
            data = self.payload
        if output is not None:
            output.write(data)
        return data if output is None else b"", len(data), hashlib.sha256(data).hexdigest()

    def offer(self):
        update.check()
        self.assertTrue(update.status()["update_available"])

    def test_update_keeps_old_version_and_private_state(self):
        self.offer()
        old_mask = os.umask(0o077)
        try:
            update.worker()
        finally:
            os.umask(old_mask)
        self.assertEqual(update.installed(), "0.46.0-rpi5.29")
        self.assertTrue(self.previous.is_dir())
        self.assertEqual(update.status()["update_phase"], "done")
        self.assertFalse(update.status()["update_available"])
        self.assertEqual(update.STATE.stat().st_mode & 0o777, 0o600)
        self.assertEqual(update.PREFIX.resolve().stat().st_mode & 0o777, 0o755)
        self.assertEqual((update.PREFIX / "punktfunk-session").stat().st_mode & 0o777, 0o755)

    def test_hash_failure_never_stops_the_current_app(self):
        self.offer()
        self.payload += b"changed"
        with self.assertRaisesRegex(RuntimeError, "checksum"):
            update.worker()
        self.assertEqual(update.PREFIX.resolve(), self.previous)
        self.command.assert_not_called()

    def test_incompatible_os_does_not_offer_an_update(self):
        self.manifest["runtime"] = "different-os"
        with self.assertRaisesRegex(RuntimeError, "newer kiosk OS"):
            update.check()
        self.assertFalse(update.status()["update_available"])
        self.command.assert_not_called()

    def test_unsafe_archive_never_replaces_the_app(self):
        self.payload = self.archive(malicious=True)
        self.manifest.update(size=len(self.payload), sha256=hashlib.sha256(self.payload).hexdigest())
        self.offer()
        with self.assertRaises(tarfile.FilterError):
            update.worker()
        self.assertEqual(update.PREFIX.resolve(), self.previous)
        self.command.assert_not_called()
        self.assertFalse((self.root / "escaped").exists())

    def test_crashing_app_restores_previous_version(self):
        self.offer()
        self.health.return_value = False
        with self.assertRaisesRegex(RuntimeError, "previous version was restored"):
            update.worker()
        self.assertEqual(update.PREFIX.resolve(), self.previous)
        self.assertEqual(update.status()["update_phase"], "error")
        self.command.assert_any_call("/usr/bin/systemctl", "start", update.SERVICE)

    def test_boot_recovery_restores_without_starting_a_dependent_service(self):
        next_app = self.app("0.46.0-rpi5.29")
        update.activate(next_app)
        update.save({"phase": "testing", "previous": str(self.previous)})
        update.recover()
        self.assertEqual(update.PREFIX.resolve(), self.previous)
        self.command.assert_not_called()
        self.assertEqual(update.status()["update_phase"], "error")

    def test_killed_worker_restores_and_starts_the_old_app(self):
        next_app = self.app("0.46.0-rpi5.29")
        update.activate(next_app)
        update.save({"phase": "switching", "previous": str(self.previous)})
        update.cleanup()
        self.assertEqual(update.PREFIX.resolve(), self.previous)
        self.command.assert_any_call("/usr/bin/systemctl", "stop", update.SERVICE, required=False)
        self.command.assert_any_call("/usr/bin/systemctl", "start", update.SERVICE)

    def test_rollback_rejects_a_path_outside_managed_releases(self):
        update.save({"phase": "testing", "previous": str(self.root)})
        with self.assertRaisesRegex(RuntimeError, "layout needs repair"):
            update.recover()
        self.command.assert_not_called()

    def test_no_downgrade_is_offered(self):
        self.previous.rename(update.RELEASES / "0.46.0-rpi5.30")
        new = update.RELEASES / "0.46.0-rpi5.30"
        (new / "build-version").write_text(new.name)
        update.PREFIX.unlink()
        update.PREFIX.symlink_to(new)
        update.check()
        self.assertFalse(update.status()["update_available"])
        self.assertEqual(update.status()["update_message"], "Punktfunk is up to date.")

    def test_start_queues_a_service_instead_of_stopping_its_own_cgroup(self):
        self.offer()
        def start_worker(*args):
            with update.locked():
                self.assertEqual(update.status()["update_phase"], "queued")
        self.command.side_effect = start_worker
        update.start()
        self.command.assert_called_once_with("/usr/bin/systemctl", "start", update.WORKER)
        self.assertEqual(update.status()["update_phase"], "queued")

    def test_manifest_rejects_foreign_archive_names(self):
        for name in ("../../etc/passwd", "other.tar.gz", "https://example.com/app.tar.gz"):
            with self.subTest(name=name):
                self.manifest["archive"] = name
                with self.assertRaisesRegex(RuntimeError, "invalid app update"):
                    update.validate_manifest(self.manifest, "v0.46.0-rpi5.29")

    def test_running_update_rejects_a_second_request(self):
        self.offer()
        update.start()
        with self.assertRaisesRegex(RuntimeError, "already running"):
            update.check()


if __name__ == "__main__":
    unittest.main()
