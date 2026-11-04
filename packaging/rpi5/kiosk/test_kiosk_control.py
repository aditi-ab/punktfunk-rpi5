"""Run with python3 -m unittest discover -s packaging/rpi5/kiosk."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import hashlib
import tempfile
from pathlib import Path
from subprocess import CompletedProcess
import unittest
from unittest.mock import patch, mock_open, MagicMock

loader = importlib.machinery.SourceFileLoader("kiosk_control", str(Path(__file__).parent / "runtime/kiosk-control"))
spec = importlib.util.spec_from_loader(loader.name, loader)
control = importlib.util.module_from_spec(spec)
loader.exec_module(control)


class KioskControlTests(unittest.TestCase):
    def test_virtualhere_download_is_atomic_and_does_not_enable_sharing(self):
        payload = b"reviewed ARM64 server"
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "virtualhere/server"
            response = MagicMock()
            response.__enter__.return_value = response
            response.geturl.return_value = control.VH_URL
            response.read.side_effect = [payload, b""]
            with patch.object(control, "VH_BINARY", binary), patch.object(control, "VH_SHA256", hashlib.sha256(payload).hexdigest()), patch.object(control.urllib.request, "urlopen", return_value=response), patch.object(control, "command") as command:
                control.change_virtualhere("virtualhere-install")
                self.assertEqual(binary.read_bytes(), payload)
                self.assertEqual(binary.stat().st_mode & 0o777, 0o755)
                command.assert_not_called()
                control.install_virtualhere()
                self.assertEqual(response.read.call_count, 2)

    def test_virtualhere_rejects_changed_download_and_cleans_temporary_files(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "server"
            response = MagicMock()
            response.__enter__.return_value = response
            response.geturl.return_value = control.VH_URL
            response.read.side_effect = [b"unexpected", b""]
            with patch.object(control, "VH_BINARY", binary), patch.object(control.urllib.request, "urlopen", return_value=response):
                with self.assertRaisesRegex(RuntimeError, "changed its download"):
                    control.install_virtualhere()
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_virtualhere_rejects_redirects(self):
        with tempfile.TemporaryDirectory() as directory:
            response = MagicMock()
            response.__enter__.return_value = response
            response.geturl.return_value = "http://example.com/server"
            with patch.object(control, "VH_BINARY", Path(directory) / "server"), patch.object(control.urllib.request, "urlopen", return_value=response):
                with self.assertRaisesRegex(RuntimeError, "location changed"):
                    control.install_virtualhere()
                response.read.assert_not_called()

    def test_virtualhere_preserves_license_and_disable_does_not_remove_it(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "server"
            binary.touch()
            config = Path(directory) / "private/config.ini"
            with patch.object(control, "VH_BINARY", binary), patch.object(control, "VH_CONFIG", config), patch.object(control, "command", return_value=CompletedProcess([], 0, "", "")) as command:
                control.change_virtualhere("virtualhere-enable")
                self.assertEqual(config.parent.stat().st_mode & 0o777, 0o700)
                self.assertEqual(config.stat().st_mode & 0o777, 0o600)
                config.write_text("License=SECRET\n")
                control.change_virtualhere("virtualhere-enable")
                control.change_virtualhere("virtualhere-disable")
                self.assertEqual(config.read_text(), "License=SECRET\n")
                state = control.virtualhere_status()
                self.assertTrue(state["virtualhere_license_saved"])
                self.assertNotIn("SECRET", json.dumps(state))
                command.assert_any_call("/usr/bin/systemctl", "disable", "--now", control.VH_UNIT)

    def test_virtualhere_cannot_start_before_installation(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(control, "VH_BINARY", Path(directory) / "absent"), patch.object(control, "command") as command:
                with self.assertRaisesRegex(RuntimeError, "Install VirtualHere"):
                    control.change_virtualhere("virtualhere-enable")
                command.assert_not_called()

    def invoke(self, args):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = control.main(args)
        return code, json.loads(output.getvalue())

    def test_extra_arguments_never_dispatch(self):
        with patch.object(control, "command") as command:
            for args in ([], ["ssh-enable", "extra"], ["shell"], ["status; id"]):
                self.assertEqual(self.invoke(args)[0], 2)
            command.assert_not_called()

    def test_unprivileged_call_never_dispatches(self):
        with patch.object(control.os, "geteuid", return_value=1000), patch.object(control, "change") as change:
            self.assertEqual(self.invoke(["ssh-enable"])[0], 1)
            change.assert_not_called()

    def test_status_reports_global_addresses_without_secrets(self):
        def command(*args, **kwargs):
            output = ""
            if "address" in args:
                output = json.dumps([{"addr_info": [
                    {"scope": "host", "local": "127.0.0.1"},
                    {"scope": "global", "local": "192.168.1.42"},
                    {"scope": "link", "local": "fe80::1"}]}])
            elif "device" in args:
                output = "wlan0:wifi:connected:Living:Room\nlo:loopback:connected:lo\n"
            return CompletedProcess(args, 0, output, "")
        files = {"/etc/punktfunk-kiosk-release": "RELEASE_TAG=v0.46.0-rpi5.2",
                 "/proc/uptime": "123.45 9.2", "/sys/class/thermal/thermal_zone0/temp": "51000"}
        with patch.object(control, "command", side_effect=command), patch.object(control, "read", side_effect=lambda p, *a: files.get(p, "")):
            result = control.status()
        self.assertEqual(result["ip_addresses"], ["192.168.1.42"])
        self.assertEqual(result["connection"], "Wi-Fi: Living:Room (wlan0)")
        self.assertEqual(result["temperature_c"], 51)
        self.assertEqual(result["uptime_seconds"], 123)
        self.assertNotIn("password", result)

    def test_password_uses_stdin_only(self):
        with patch.object(control.pwd, "getpwnam", side_effect=KeyError), patch.object(control, "command") as command:
            password = control.password()
        self.assertEqual(len(password), 20)
        for call in command.call_args_list:
            self.assertNotIn(password, str(call.args))
        self.assertEqual(command.call_args.kwargs["input"], f"punktfunk-admin:{password}\n")

    def test_subprocess_stderr_never_leaks(self):
        with patch.object(control.subprocess, "run", return_value=CompletedProcess([], 1, "", "SECRET")):
            with self.assertRaisesRegex(RuntimeError, "Couldn't change the service") as error:
                control.command("/usr/sbin/chpasswd", input="SECRET")
        self.assertNotIn("SECRET", str(error.exception))

    def test_enable_failure_stops_service_and_locks_account(self):
        def command(*args, **kwargs):
            if args[:2] == ("/usr/sbin/sshd", "-t"):
                raise RuntimeError("Couldn't change SSH. Try again.")
            return CompletedProcess(args, 0, "", "")
        with patch("builtins.open", mock_open()), patch.object(control.fcntl, "flock"), patch.object(control, "password", return_value="SECRET"), patch.object(control, "command", side_effect=command) as run:
            with self.assertRaises(RuntimeError):
                control.change("ssh-enable")
        run.assert_any_call("/usr/bin/systemctl", "disable", "--now", "ssh.service", required=False)
        run.assert_any_call("/usr/bin/passwd", "--lock", "punktfunk-admin", required=False)

    def test_reset_while_disabled_changes_nothing(self):
        with patch("builtins.open", mock_open()), patch.object(control.fcntl, "flock"), patch.object(control, "active", return_value=False), patch.object(control, "password") as password:
            with self.assertRaisesRegex(RuntimeError, "Turn on SSH"):
                control.change("ssh-reset-password")
            password.assert_not_called()

    def test_password_only_returned_by_mutation(self):
        with patch.object(control.os, "geteuid", return_value=0), patch.object(control, "status", return_value={"available": True}), patch.object(control, "change", return_value="SECRET") as change:
            self.assertNotIn("password", self.invoke(["status"])[1])
            change.assert_not_called()
            self.assertEqual(self.invoke(["ssh-enable"])[1]["password"], "SECRET")

    def test_disable_revokes_login_and_sessions(self):
        with patch("builtins.open", mock_open()), patch.object(control.fcntl, "flock"), patch.object(control.pwd, "getpwnam"), patch.object(control, "command") as run:
            self.assertIsNone(control.change("ssh-disable"))
        run.assert_any_call("/usr/bin/systemctl", "disable", "--now", "ssh.service", "ssh.socket")
        run.assert_any_call("/usr/bin/passwd", "--lock", "punktfunk-admin")
        run.assert_any_call("/usr/bin/loginctl", "terminate-user", "punktfunk-admin", required=False)


if __name__ == "__main__":
    unittest.main()
