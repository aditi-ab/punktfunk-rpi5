//! The audio-format vocabulary shared by every settings surface: the stored `audio_format`
//! values, the `(value, label)` table the settings screens render, and the wire pair a stored
//! value asks the host for. Portable (no session/decoder dependencies) because the Skia console
//! shell renders the same row on Android, where the session pump does not build; `session`
//! re-exports everything here under its old names.

/// The `audio_format` setting's stored value for the Opus plane — the default, and byte for byte
/// the session every build before the lossless plane ran.
pub const AUDIO_FORMAT_OPUS: &str = "opus";

/// Bit-exact PCM at 48 kHz / 24-bit (~2.3 Mbps). The honest win even without a hi-res interface:
/// no lossy stage at all, and no double resample on a host whose engine already runs at 48 kHz.
pub const AUDIO_FORMAT_LOSSLESS_48: &str = "lossless48";

/// Bit-exact PCM at 96 kHz / 24-bit (~4.6 Mbps), and only real if the host's capture endpoint
/// genuinely runs at 96 kHz — the host declines rather than upsampling to meet the request.
pub const AUDIO_FORMAT_LOSSLESS_96: &str = "lossless96";

/// `(stored value, label)` for the requested audio format — the cross-client table both desktop
/// settings UIs render, so the two shells can never drift from each other or from the wire.
///
/// ⚠ **The stored values are shared VERBATIM with the Apple client's `AudioFormatChoice` raw
/// values and the Android client's `AUDIO_FORMAT_*`.** One profile catalog round-trips through all
/// four clients (`profiles.rs`), and a spelling that differs by a single character fails in the
/// worst possible way: the key is carried through untouched, so a profile written on a phone would
/// keep working on a TV and silently inherit the global default here. Change these only in lockstep
/// with `clients/apple/Sources/PunktfunkShared/EffectiveSettings.swift` and
/// `clients/android/app/src/main/kotlin/io/unom/punktfunk/Settings.kt`.
///
/// **The ladder is 48/96 kHz only, and that is arithmetic rather than bandwidth.** Core's jitter
/// policy sizes every buffer as `ms × samples-per-ms` with an INTEGER per-ms: 48 000 → 48 and
/// 96 000 → 96 are exact, 44 100 → 44.1 truncates to 44 — a silent 2.3 % error in every target and
/// every reported depth. 44.1 kHz and its multiples are deferred behind reworking that arithmetic
/// (`design/hi-res-audio.md` §4.1), and the host would decline them regardless.
///
/// Lossless at 48 kHz / **16**-bit is deliberately absent from the menu even though the env
/// override below can still ask for it: it spends ~1.5 Mbps to sound like the transparent 256 kbps
/// Opus it replaces. 24-bit is where the plane earns its bandwidth.
pub const AUDIO_FORMATS: &[(&str, &str)] = &[
    (AUDIO_FORMAT_OPUS, "Standard (Opus)"),
    (AUDIO_FORMAT_LOSSLESS_48, "Lossless 48 kHz / 24-bit"),
    (AUDIO_FORMAT_LOSSLESS_96, "Lossless 96 kHz / 24-bit"),
];

/// The `(rate_hz, bits)` a stored [`AUDIO_FORMATS`] value asks the host for; `None` = the Opus
/// plane, which the caller must turn into the unspecified `0`/`0` pair on the wire rather than an
/// explicit 48 000/16 — core reads any non-zero pair as "this client is asking for the lossless
/// plane", so a literal legacy pair would advertise hi-res on every ordinary session.
///
/// An unrecognized value — a newer client's row, or a corrupted settings file — resolves to Opus
/// rather than blocking the connect, matching what the Apple and Android ports do with the same
/// string. Deriving the pair FROM the stored value is what stops the menu row and the format ever
/// disagreeing.
pub fn audio_format_wire(setting: &str) -> Option<(u32, u8)> {
    use punktfunk_core::audio::pcm::BITS_24;
    match setting {
        AUDIO_FORMAT_LOSSLESS_48 => Some((48_000, BITS_24)),
        AUDIO_FORMAT_LOSSLESS_96 => Some((96_000, BITS_24)),
        _ => None,
    }
}
