"""Run with python3 -m unittest discover -s packaging/rpi5/kiosk."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
from pathlib import Path
from subprocess import CompletedProcess
import unittest
from unittest.mock import patch, mock_open

loader = importlib.machinery.SourceFileLoader("kiosk_control", str(Path(__file__).parent / "runtime/kiosk-control"))
spec = importlib.util.spec_from_loader(loader.name, loader)
control = importlib.util.module_from_spec(spec)
loader.exec_module(control)


class KioskControlTests(unittest.TestCase):
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
            with self.assertRaisesRegex(RuntimeError, "Couldn't change SSH") as error:
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
