# Privacy — what to link from App Store Connect

## The situation

You already have a privacy policy at **punktfunk.unom.io/legal/privacy**. It is good, current
(Stand: 28. Juni 2026), and localised DE/EN. But it is a **website** privacy policy: it covers
server log files, Plausible Analytics on `analytics.unom.io`, the `PARAGLIDE_LOCALE` cookie, and
self-hosted fonts. It does not mention the apps at all.

That is a problem for App Store Connect in two directions:

1. Apple requires the linked policy to describe **the app's** data practices. A reviewer following
   the link finds a page about a website.
2. It reads as *contradicting* a "Data Not Collected" declaration. The page prominently describes
   analytics and a cookie. A reviewer who skims it sees "Reichweitenmessung mit Plausible
   Analytics" and has every reason to question the App Privacy answers.

**Recommendation:** keep the existing page and append an app-specific section to it (the text
below), so one URL covers both. The alternative — a separate `/legal/privacy-apps` route — also
works, but one URL is less to keep in sync.

The page is CMS-driven (`src/routes/legal/privacy.tsx` renders Payload `RichText` blocks from the
`pages` collection, slug `legal/privacy`, tenant `punktfunk`), so this is a CMS edit rather than a
code change.

## Confirming the "collects no data" framing

Checked against the source rather than taken on trust, and it holds:

- **No analytics, telemetry, or crash-reporting SDK.** `Package.swift` declares no such dependency.
  A case-insensitive sweep of `Sources/` for `sentry|firebase|analytics|telemetry|amplitude|
  mixpanel|crashlytics|posthog|plausible` returns 43 hits — 43 of them the word "amplitude" in
  haptics code (rumble amplitude), and one the English word "plausible" in a comment.
- **No outbound calls to us.** The only `URLSession` use is `LibraryClient`, fetching cover art
  **from the paired host**, over a TLS session that pins the host's own certificate. The only
  external URLs anywhere in the Swift sources are three UI links the user can tap: the docs site,
  the source on `git.unom.io`, and the Discord invite.
- **No account system.** Identity is a client keypair in the device keychain
  (`keychain-access-groups`, `ClientIdentityStore`); pairing is SPAKE2 with a PIN, host-to-device.
- **Data stays on device.** Saved hosts and settings live in a shared `UserDefaults` suite
  (`group.io.unom.punktfunk`) so the widget can read them. Nothing syncs; there is no CloudKit
  entitlement.
- **No ATT.** No `NSUserTrackingUsageDescription` anywhere, consistent with no tracking.

So **App Privacy → "Data Not Collected"** is accurate for all four platforms. Two caveats worth
stating in the policy text anyway, because they are true and pre-empt questions:

- The microphone uplink **is** audio leaving the device — but only to the host the user paired with,
  encrypted, and never to us. Apple's questionnaire asks about data collected *by you or your
  third-party partners*; streaming to the user's own machine is not collection. Saying so plainly
  is better than staying silent about a microphone permission.
- The apps are distributed through the App Store, so **Apple** collects its own analytics. That is
  Apple's processing, not yours, but naming it avoids looking like an omission.

---

## Text to append — Deutsch

> ## Die Punktfunk-Apps
>
> Dieser Abschnitt betrifft die Punktfunk-Apps für iPhone, iPad, Apple TV, Mac, Windows, Linux und
> Android – im Unterschied zu den vorstehenden Abschnitten, die sich auf diese Website beziehen.
>
> **Die Apps erheben keine personenbezogenen Daten.** Es gibt keine Benutzerkonten, keine
> Registrierung und keine Anmeldung. Die Apps enthalten keine Analyse-, Tracking-, Werbe- oder
> Absturzbericht-Bibliotheken von Drittanbietern. Es findet kein Tracking im Sinne des App
> Tracking Transparency Frameworks statt, und es werden keine Daten an uns oder an Dritte
> übermittelt.
>
> **Wohin die Daten fließen.** Punktfunk verbindet Ihr Gerät direkt mit einem Host-Rechner, den Sie
> selbst betreiben – in der Regel in Ihrem eigenen Netzwerk. Video, Ton, Maus-, Tastatur- und
> Controller-Eingaben sowie – sofern Sie ihn einschalten – Ihr Mikrofon werden ausschließlich
> zwischen Ihrem Gerät und diesem Host übertragen, verschlüsselt und ohne Umweg über einen Server
> von uns. Wir betreiben für den Streaming-Betrieb keine Vermittlungs-, Relay- oder Cloud-Dienste
> und haben zu keinem Zeitpunkt Zugriff auf die Inhalte einer Sitzung.
>
> **Was auf dem Gerät bleibt.** Die App speichert lokal auf Ihrem Gerät: die von Ihnen
> hinzugefügten oder im Netzwerk gefundenen Hosts, Ihre Einstellungen und Profile sowie einen
> kryptografischen Schlüssel, mit dem sich Ihr Gerät gegenüber einem gekoppelten Host ausweist
> (auf Apple-Geräten im Schlüsselbund). Diese Daten verlassen Ihr Gerät nicht und werden gelöscht,
> wenn Sie die App entfernen.
>
> **Berechtigungen.** Die App fragt nur Berechtigungen ab, die für den Betrieb nötig sind: den
> Zugriff auf das lokale Netzwerk, um Hosts zu finden und sich mit ihnen zu verbinden, und – nur
> wenn Sie die Mikrofonübertragung nutzen – das Mikrofon. Das Mikrofonsignal wird an den von Ihnen
> gekoppelten Host übertragen, wo es als virtuelles Mikrofon erscheint; es wird nicht
> aufgezeichnet und nicht an uns gesendet.
>
> **Verteilung über App-Stores.** Wenn Sie die App über den App Store oder Google Play beziehen,
> verarbeiten Apple bzw. Google im Rahmen der Auslieferung eigene Daten (etwa Kauf-, Installations-
> und Absturzstatistiken). Darauf haben wir keinen Einfluss; es gelten die
> Datenschutzbestimmungen des jeweiligen Anbieters. Aggregierte Statistiken, die uns Apple oder
> Google in ihren Entwicklerkonsolen anzeigen, lassen keinen Rückschluss auf einzelne Personen zu.
>
> **Der Host.** Der Punktfunk-Host ist quelloffene Software, die Sie selbst auf Ihrem eigenen
> Rechner betreiben. Welche Daten dabei anfallen – etwa lokale Protokolldateien –, bleibt
> vollständig unter Ihrer Kontrolle; wir erhalten davon nichts. Der Quellcode ist unter
> git.unom.io/unom/punktfunk einsehbar.

---

## Text to append — English

> ## The Punktfunk apps
>
> This section concerns the Punktfunk apps for iPhone, iPad, Apple TV, Mac, Windows, Linux, and
> Android — as distinct from the sections above, which concern this website.
>
> **The apps collect no personal data.** There are no user accounts, no registration, and no sign-in.
> The apps contain no third-party analytics, tracking, advertising, or crash-reporting libraries.
> No tracking within the meaning of Apple's App Tracking Transparency framework takes place, and no
> data is transmitted to us or to any third party.
>
> **Where your data goes.** Punktfunk connects your device directly to a host machine that you run
> yourself, normally on your own network. Video, audio, mouse, keyboard, and controller input — and
> your microphone, if you switch it on — travel only between your device and that host, encrypted,
> without passing through any server of ours. We operate no brokering, relay, or cloud service for
> streaming, and we have no access to the contents of a session at any point.
>
> **What stays on your device.** The app stores locally on your device: the hosts you have added or
> discovered on your network, your settings and profiles, and a cryptographic key your device uses
> to identify itself to a paired host (in the keychain, on Apple devices). This data does not leave
> your device and is removed when you delete the app.
>
> **Permissions.** The app requests only the permissions it needs to work: access to the local
> network, in order to find hosts and connect to them, and — only if you use microphone streaming —
> the microphone. The microphone signal is sent to the host you paired with, where it appears as a
> virtual microphone; it is not recorded and is not sent to us.
>
> **Distribution through app stores.** If you obtain the app from the App Store or Google Play,
> Apple or Google process their own data as part of distributing it (such as purchase, installation,
> and crash statistics). We have no influence over this, and the respective provider's privacy
> policy applies. The aggregated statistics Apple and Google show us in their developer consoles do
> not allow any individual to be identified.
>
> **The host.** The Punktfunk host is open source software that you run on your own machine. Any
> data it produces — local log files, for instance — remains entirely under your control, and none
> of it reaches us. The source is available at git.unom.io/unom/punktfunk.

---

## Also update

- Bump **Stand: / Effective date:** on the page when you add this.
- App Store Connect → App Privacy → **Data Not Collected** for all four platforms.
- The same URL works for Google Play's Data safety declaration; the wording above already covers it.
