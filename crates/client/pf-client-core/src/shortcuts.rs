//! The in-stream controls, as plain text for each client's reference screen.
//!
//! Data only: the Linux and Windows clients and the console draw it in their own UI. Each
//! list is what that client binds, so a row here moves with the binding it names
//! (`pf-presenter`'s `chord_of`, [`crate::gamepad`]'s Select chords and escape chord, the
//! Android client's key and touch handlers, the Apple client's `macChords` and `padChords`,
//! the webOS client's remote and dial, the browser's `input.ts`).

/// A client family with its own bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    /// The Vulkan session binary: Linux, Windows and Steam Deck.
    Desktop,
    Android,
    /// An iPhone, iPad or Apple TV. Its keys are a hardware keyboard's, so the caller lists
    /// them only while one is attached. The Apple About screen lists the remote's rows.
    Apple,
    /// A Mac: the iPad's chords, plus mouse capture, the mouse mode and fullscreen.
    Mac,
    /// The LG TV client: Magic Remote colour keys, and its own dial and escape chord.
    WebOS,
    /// The browser page, with a keyboard, a mouse and a pad.
    Web,
    /// The Samsung TV client: the browser page, with the remote's Back as its menu key.
    Tizen,
    /// The PS5 title: a DualSense, its touchpad click standing in for Create.
    Ps5,
}

/// What to press, then what it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub keys: &'static str,
    pub text: &'static str,
}

/// Which device a group's rows are pressed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Remote,
    Keyboard,
    Touch,
    Pad,
}

#[derive(Clone, Copy, Debug)]
pub struct Group {
    pub kind: Kind,
    pub title: &'static str,
    pub items: &'static [Item],
}

const fn item(keys: &'static str, text: &'static str) -> Item {
    Item { keys, text }
}

const WEBOS_REMOTE: &[Item] = &[
    item("Green", "Cycle the statistics overlay"),
    item("Yellow", "Cycle the log overlay"),
    item("Hold Yellow", "Open the quick actions dial"),
    item("Blue", "Show the on-screen keyboard"),
    item("Red", "Right mouse button"),
    item("Back", "Escape on the host"),
    item("Hold Back", "Open the disconnect dialog"),
    item("Home", "Go to the TV's home screen"),
];

const TIZEN_REMOTE: &[Item] = &[item("Back", "Open the quick actions menu")];

const DESKTOP_KEYS: &[Item] = &[
    item("Ctrl+Alt+Shift+Q", "Release input, or capture it again"),
    item("Ctrl+Alt+Shift+D", "Disconnect"),
    item("Ctrl+Alt+Shift+S", "Cycle the statistics overlay"),
    item("Ctrl+Alt+Shift+O", "Open the quick actions dial"),
    item("Ctrl+Alt+Shift+V", "Mute or unmute the microphone"),
    item("Ctrl+Alt+Shift+M", "Switch the mouse mode"),
    item("F11 or Alt+Enter", "Toggle fullscreen"),
];

const WEB_KEYS: &[Item] = &[
    item("Ctrl+Alt+Shift+Q", "Release the mouse, or capture it again"),
    item("Esc", "Release the mouse, where the page captured it"),
    item("Ctrl+Alt+Shift+D", "Disconnect"),
    item("Ctrl+Alt+Shift+S", "Cycle the statistics overlay"),
    item("Ctrl+Alt+Shift+O", "Open the quick actions menu"),
    item("Ctrl+Alt+Shift+V", "Mute or unmute the microphone"),
    item("Ctrl+Alt+Shift+M", "Switch the mouse mode"),
    item("F11 or Alt+Enter", "Toggle fullscreen"),
];

// A keyboard on the set: Esc still reaches the host, so it is not listed.
const TIZEN_KEYS: &[Item] = &[
    item("Ctrl+Alt+Shift+Q", "Release the mouse, or capture it again"),
    item("Ctrl+Alt+Shift+D", "Disconnect"),
    item("Ctrl+Alt+Shift+S", "Cycle the statistics overlay"),
    item("Ctrl+Alt+Shift+O", "Open the quick actions menu"),
    item("Ctrl+Alt+Shift+V", "Mute or unmute the microphone"),
    item("Ctrl+Alt+Shift+M", "Switch the mouse mode"),
    item("F11 or Alt+Enter", "Toggle fullscreen"),
];

const ANDROID_KEYS: &[Item] = &[
    item(
        "Ctrl+Alt+Shift+Q",
        "Release the pointer, or capture it again",
    ),
    item("Ctrl+Alt+Shift+O", "Open the quick actions dial"),
];

// Spelled out: the console's font has no ⌃ ⌥ ⌘ ⎋. The Apple About screen lists these rows
// in symbols (`ShortcutsReference.swift`), and a test holds the two together.
const MAC_KEYS: &[Item] = &[
    item("Click", "Capture the mouse and keyboard for the stream"),
    item(
        "Control+Option+Shift+Q or Command+Esc",
        "Release the mouse and keyboard back to this Mac",
    ),
    item("Control+Option+Shift+D", "Disconnect"),
    item("Control+Option+Shift+S", "Cycle the statistics overlay"),
    item("Control+Option+Shift+O", "Open the quick actions dial"),
    item("Control+Option+Shift+M", "Switch the mouse mode"),
    item("Control+Option+Shift+C", "Start or stop clipboard sharing"),
    item(
        "Control+Command+F",
        "Toggle fullscreen (after Command+Esc, or with Capture system shortcuts off)",
    ),
    item("Control+Option+Shift+A", "Mute or unmute the microphone"),
];

const APPLE_KEYS: &[Item] = &[
    item(
        "Control+Option+Shift+Q or Command+Esc",
        "Release the pointer back to this device",
    ),
    item("Control+Option+Shift+D", "Disconnect"),
    item("Control+Option+Shift+S", "Cycle the statistics overlay"),
    item("Control+Option+Shift+O", "Open the quick actions dial"),
    item("Control+Option+Shift+C", "Start or stop clipboard sharing"),
    item("Control+Option+Shift+A", "Mute or unmute the microphone"),
];

const DESKTOP_TOUCH: &[Item] = &[
    item("Three-finger tap", "Cycle the statistics overlay"),
    item("Two-finger twist", "Open the quick actions dial"),
];

const ANDROID_TOUCH: &[Item] = &[
    item("Back", "Open the quick actions dial"),
    item("Three-finger tap", "Cycle the statistics overlay"),
    item("Three-finger swipe up", "Show the keyboard"),
    item("Three-finger swipe down", "Hide the keyboard"),
    item("Two-finger twist", "Open the quick actions dial"),
];

const DESKTOP_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions dial"),
    item("Select + X", "Cycle the statistics overlay"),
    item(
        "Hold Select",
        "Press the host's guide button, where Hold Select for guide is on",
    ),
    item(
        "L1 + R1 + Start + Select",
        "Release input; hold to disconnect",
    ),
];

const ANDROID_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions dial"),
    item("Select + X", "Cycle the statistics overlay"),
    item("Select + Y", "Mute or unmute the microphone"),
    item(
        "Hold Select",
        "Press the host's guide button, where Hold Select for guide is on",
    ),
    item("L1 + R1 + Start + Select", "Hold to disconnect"),
];

const APPLE_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions dial"),
    item("Select + X", "Cycle the statistics overlay"),
    item(
        "Hold Select",
        "Press the host's guide button, where Hold Select for guide is on",
    ),
    item("L1 + R1 + Start + Select", "Hold to disconnect"),
];

const WEBOS_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions dial"),
    item("L1 + R1 + Start + Select", "Hold to disconnect"),
];

/// The console keeps Create for itself, so the touchpad click takes Select's place.
const PS5_PAD: &[Item] = &[
    item("Touchpad + Cross", "Open the quick actions dial"),
    item("L1 + R1 + Options + Touchpad", "Hold to disconnect"),
];

const WEB_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions menu"),
    item(
        "L1 + R1 + Start + Select",
        "Release input; hold to disconnect",
    ),
];

/// The groups a client shows, remote and keyboard first. `touchscreen` is false on a TV, which has no
/// screen to touch. An Apple TV binds no keys either.
pub fn groups(client: Client, touchscreen: bool) -> Vec<Group> {
    let (remote, keys, touch, pad): (&[Item], &[Item], &[Item], &[Item]) = match client {
        Client::Desktop => (&[], DESKTOP_KEYS, DESKTOP_TOUCH, DESKTOP_PAD),
        Client::Android => (&[], ANDROID_KEYS, ANDROID_TOUCH, ANDROID_PAD),
        Client::Apple if touchscreen => (&[], APPLE_KEYS, &[], APPLE_PAD),
        Client::Apple => (&[], &[], &[], APPLE_PAD),
        Client::Mac => (&[], MAC_KEYS, &[], APPLE_PAD),
        Client::WebOS => (WEBOS_REMOTE, &[], &[], WEBOS_PAD),
        Client::Web => (&[], WEB_KEYS, &[], WEB_PAD),
        Client::Tizen => (TIZEN_REMOTE, TIZEN_KEYS, &[], WEB_PAD),
        Client::Ps5 => (&[], &[], &[], PS5_PAD),
    };
    let mut out = Vec::new();
    if !remote.is_empty() {
        out.push(Group {
            kind: Kind::Remote,
            title: "Remote",
            items: remote,
        });
    }
    if !keys.is_empty() {
        out.push(Group {
            kind: Kind::Keyboard,
            title: "Keyboard",
            items: keys,
        });
    }
    if touchscreen && !touch.is_empty() {
        out.push(Group {
            kind: Kind::Touch,
            title: "Touchscreen (Trackpad and Direct pointer modes)",
            items: touch,
        });
    }
    out.push(Group {
        kind: Kind::Pad,
        title: if client == Client::Ps5 {
            "DualSense"
        } else {
            "Controller (Select is Back or View)"
        },
        items: pad,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Client; 8] = [
        Client::Desktop,
        Client::Android,
        Client::Apple,
        Client::Mac,
        Client::WebOS,
        Client::Web,
        Client::Tizen,
        Client::Ps5,
    ];

    /// Every client names its dial and escape chord; the PS5's use the touchpad for Select.
    #[test]
    fn every_client_lists_the_controller_chords() {
        for c in ALL {
            let pad = groups(c, true).pop().unwrap();
            let chords = if c == Client::Ps5 {
                ["Touchpad + Cross", "L1 + R1 + Options + Touchpad"]
            } else {
                ["Select + A", "L1 + R1 + Start + Select"]
            };
            for keys in chords {
                assert!(
                    pad.items.iter().any(|i| i.keys == keys),
                    "{c:?} lacks {keys}"
                );
            }
        }
    }

    /// Only the native pad paths bind Select + X; webOS opens stats from the dial or Green,
    /// the browser from the keyboard.
    #[test]
    fn the_stats_chord_is_native_only() {
        let has = |c| {
            groups(c, true)
                .iter()
                .flat_map(|g| g.items)
                .any(|i| i.keys == "Select + X")
        };
        assert!(has(Client::Desktop) && has(Client::Android));
        assert!(has(Client::Apple) && has(Client::Mac));
        assert!(!has(Client::WebOS) && !has(Client::Web) && !has(Client::Tizen));
    }

    /// The Apple About screen's keyboard rows for one platform, symbols spelled out as the
    /// lists here write them. Its file opens each platform's rows with `from` and ends the
    /// keyboard group at the first `groups.append`.
    fn about_rows(from: &str) -> Vec<(String, String)> {
        const ABOUT: &str = include_str!(
            "../../../../clients/apple/Sources/PunktfunkClient/Settings/ShortcutsReference.swift"
        );
        let spell = |s: &str| {
            [
                ("⌃", "Control+"),
                ("⌥", "Option+"),
                ("⇧", "Shift+"),
                ("⌘", "Command+"),
                ("⎋", "Esc"),
            ]
            .iter()
            .fold(s.to_string(), |s, (symbol, word)| s.replace(symbol, word))
        };
        let block = ABOUT.split_once(from).expect("the platform's rows").1;
        let block = block.split_once("groups.append").expect("the group").0;
        block
            .split(".init(keys: \"")
            .skip(1)
            .map(|row| {
                let (keys, rest) = row.split_once("\", text: \"").expect("a row's text");
                let text = rest.split_once("\")").expect("a row's end").0;
                (spell(keys), spell(text))
            })
            .collect()
    }

    /// The console and the About screen name one set of bindings. A row added to either has
    /// to reach the other, in the same order.
    #[test]
    fn the_apple_keys_are_the_rows_the_about_screen_lists() {
        let ours = |items: &[Item]| -> Vec<(String, String)> {
            items
                .iter()
                .map(|i| (i.keys.to_string(), i.text.to_string()))
                .collect()
        };
        assert_eq!(ours(MAC_KEYS), about_rows("#if os(macOS)"));
        assert_eq!(
            ours(APPLE_KEYS),
            about_rows("#elseif os(iOS) || os(visionOS)")
        );
    }

    #[test]
    fn an_apple_tv_lists_no_keys() {
        let keys = |c, touchscreen| {
            groups(c, touchscreen)
                .iter()
                .any(|g| g.kind == Kind::Keyboard)
        };
        assert!(keys(Client::Apple, true) && keys(Client::Mac, true));
        assert!(!keys(Client::Apple, false));
    }

    #[test]
    fn a_tv_has_no_touch_group() {
        assert!(groups(Client::Android, false)
            .iter()
            .all(|g| !g.title.starts_with("Touch")));
        assert!(groups(Client::Android, true)
            .iter()
            .any(|g| g.title.starts_with("Touch")));
    }

    #[test]
    fn only_android_lists_the_pad_mute() {
        let has = |c| {
            groups(c, true)
                .iter()
                .flat_map(|g| g.items)
                .any(|i| i.keys == "Select + Y")
        };
        assert!(has(Client::Android));
        assert!(ALL.iter().filter(|c| has(**c)).count() == 1);
    }

    #[test]
    fn no_label_is_empty_or_ends_in_a_period() {
        for c in ALL {
            for i in groups(c, true).iter().flat_map(|g| g.items) {
                assert!(!i.keys.is_empty() && !i.text.is_empty());
                assert!(!i.text.ends_with('.'), "{}", i.text);
            }
        }
    }
}
