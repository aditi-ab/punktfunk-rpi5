# Google Play listing

Play Console copy that has to match the app. `ci/play-upload.py` uploads bundles only, so the
listing is edited by hand in Play Console.

## Accessibility service

Punktfunk uses the AccessibilityService API (`KeyCaptureService`) and is not an accessibility
tool, so `isAccessibilityTool` stays unset. Play then requires three things at every submission:

1. The full description says what the API is used for (below).
2. A prominent disclosure in normal use, not only in Settings. `KeyCaptureDisclosure` shows it
   once at launch when a hardware keyboard is attached, and Settings → Input → Keyboard shortcuts
   shows it again. Keep its text and the paragraph below saying the same thing.
3. A current video link in the Accessibility API declaration
   (App content → Sensitive permissions and APIs).

### Full description paragraph

Append to the full description (4000-character cap), in every language the listing has.

English:

> Keyboard shortcuts (AccessibilityService API): Android keeps Alt+Tab, the Windows key and your
> keyboard's language key for itself. To send them to the computer you stream from, Punktfunk can
> use Android's AccessibilityService API. It is optional and off until you turn it on: the app
> asks once when a hardware keyboard is attached, and Settings → Input → Keyboard shortcuts offers
> it any time. With it on, Punktfunk receives the keys you press on a hardware keyboard and sends
> them to that computer only while a stream is on screen. Outside a stream every key passes
> through untouched. The service can't see the screen or other apps' content, and stores nothing.

German:

> Tastenkürzel (AccessibilityService-API): Android behält Alt+Tab, die Windows-Taste und die
> Sprachtaste deiner Tastatur für sich. Damit sie den Rechner erreichen, von dem du streamst, kann
> Punktfunk die AccessibilityService-API von Android nutzen. Das ist freiwillig und aus, bis du es
> einschaltest: Die App fragt einmal, wenn eine Hardware-Tastatur angeschlossen ist, und unter
> Settings → Input → Keyboard shortcuts jederzeit. Ist es an, empfängt Punktfunk die Tasten deiner
> Hardware-Tastatur und schickt sie nur während eines laufenden Streams an diesen Rechner. Außerhalb
> eines Streams gehen alle Tasten unverändert durch. Der Dienst sieht weder den Bildschirm noch
> Inhalte anderer Apps und speichert nichts.

### Declaration answers

- Accessibility tool: no.
- What it does: forwards system-reserved keyboard shortcuts (Alt+Tab, Windows key, language key)
  from a hardware keyboard to the user's own computer during a remote-desktop stream. It requests
  key filtering only (`flagRequestFilterKeyEvents`), only while a stream is on screen, no events
  and no window content.

### Review video

One take on a phone or tablet with a hardware keyboard and a paired host. Reset the one-time
prompt first with `adb shell pm clear io.unom.punktfunk`. Record every step:

1. Launch the app. The prompt appears over the home screen. Hold on the full text.
2. Tap No thanks. The app keeps working; start a stream and show Alt+` standing in for Alt+Tab.
3. Open Settings → Input → Keyboard shortcuts. The same prompt appears. Tap Agree.
4. Android's Accessibility page opens. Turn on Punktfunk keyboard shortcuts and accept Android's
   own dialog.
5. Start a stream and press Alt+Tab. The host's task switcher opens and Android's recents do not.
6. Leave the stream and press Alt+Tab again. Android's own switcher opens: nothing is captured
   outside a stream.
