//! The stored decoder preference's forward-compatibility rule, on its own because the
//! Skia console's settings screen reads it on Android too, where `video` (the decoder
//! rungs themselves) does not build. `video` re-exports it under its old name.

/// The pre-M10 decoder-preference spellings, mapped onto the rung that replaced them.
///
/// `vulkan`, `vaapi` and `d3d11va` named **libavcodec's** rungs specifically — the whole
/// point of the `native-*` names was that they were the OTHER ones — and M10 deleted them.
/// But those three are not developer-only env values: all three desktop Settings UIs
/// offered them (`clients/linux`'s "VAAPI", the WinUI shell's "Hardware (Direct3D 11 /
/// DXVA)", the console UI's list), so they sit in shipped users' settings files right now.
/// Refusing them would turn an upgrade into a dead session for anyone who ever touched
/// that dropdown, and the message would tell them to edit something they never edited.
///
/// So they MIGRATE rather than refuse, and the mapping is exact in the sense that
/// matters: the user asked for a hardware FAMILY (Vulkan Video / VAAPI / DXVA) and gets
/// that family, on the implementation that still exists. The UI labels stay true
/// word-for-word — native Vulkan Video is still Vulkan Video. What does change is the
/// failure mode: a libavcodec pin that failed to open was a hard session error, while
/// every `native-*` pin logs and falls through to the standard ladder. For a value read
/// out of a settings file that is the right direction; a pin that cannot open must not be
/// the reason a user's client stops working.
///
/// ⚠ It is `pub` and PURE — no log line — for the Settings dialogs, not for the pump.
/// Their decoder combos look the stored string up in their own preset list, and a value
/// that matches nothing shows as "Automatic"; save the dialog without touching that row
/// and the user's Vulkan/VAAPI preference is silently rewritten to `auto`. So they
/// migrate on the WAY IN too, and a function that logged would then warn once per dialog
/// open. [`Decoder::new`] does the logging, where "a session started on a migrated
/// preference" is the fact worth recording.
///
/// Nothing rewrites the STORE. The value is migrated on every read, so a user who
/// downgrades to an older client still finds the preference they set.
pub fn migrate_decoder_pref(pref: &str) -> String {
    match pref {
        "vulkan" => "native-vulkan".to_string(),
        "vaapi" => "native-vaapi".to_string(),
        "d3d11va" => "native-d3d11va".to_string(),
        _ => pref.to_string(),
    }
}
