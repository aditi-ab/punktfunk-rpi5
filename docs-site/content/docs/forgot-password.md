---
title: Forgot your Password?
description: Where the punktfunk web console login password lives — and how to read or reset it — on each host platform.
---

The punktfunk **web console** (status, paired devices, PIN pairing) is protected by a login
password. That password is generated — or, on Windows, chosen — when the console is first set up, and
it lives on the **host**. So if you can't get past the login screen, you recover or change it on the
host machine itself, not from the browser.

New to the console? See [The Web Console](/docs/web-console) to enable it and arm pairing.

> This is **only** the web console login. It is **not** your client/device pairing — if a client
> won't connect, that's [Pairing](/docs/pairing), not this password.

## Find your host

Find your host platform for exactly where the password lives, then read or reset it below:

| Host | Where the password lives | Section |
|------|--------------------------|---------|
| **Linux packages (apt / RPM / Bazzite)** | `~/.config/punktfunk/web-password` | [Login password](/docs/web-console#login-password) |
| **SteamOS (host)** | `~/.config/punktfunk/web.env` | [Login password](/docs/web-console#login-password) |
| **Windows host** | `%ProgramData%\punktfunk\web-password` | [Login password](/docs/web-console#login-password) · [Windows Host](/docs/windows-host) |

## The short version

**Linux packages (apt / RPM / Bazzite).** The password is generated on first start and saved to
`~/.config/punktfunk/web-password`. Read it back:

```sh
# from the init service's journal (printed once, when it was generated):
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
# …or straight from the file:
sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password
```

Change it by editing that file (`PUNKTFUNK_UI_PASSWORD=<your-password>`) and restarting the console:
`systemctl --user restart punktfunk-web`.

**SteamOS / Steam Deck.** Same idea, but the installer writes it to `~/.config/punktfunk/web.env`
and prints it at the end of the install run:

```sh
sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web.env
```

Edit that file and `systemctl --user restart punktfunk-web` to change it.

**Windows.** You pick the password during install (a secure random default is pre-filled and shown
on the installer's final page). It lives in `%ProgramData%\punktfunk\web-password`. To change it,
edit the file and restart the **PunktfunkWeb** task — in an **elevated** PowerShell:

```powershell
notepad "$env:ProgramData\punktfunk\web-password"   # set PUNKTFUNK_UI_PASSWORD=<your-password>
schtasks /End /TN PunktfunkWeb; schtasks /Run /TN PunktfunkWeb
```

Still stuck? See [Troubleshooting](/docs/troubleshooting).
