// PunktfunkShared holds what the app AND the widget extension both need — the stored-host model,
// the settings-key names, the App-Group constant, the deep-link grammar, and the Live Activity
// attributes — in a module that links neither the Rust core nor the presentation layer.
//
// Re-export it so every existing consumer of PunktfunkKit (`import PunktfunkKit`) keeps seeing
// `StoredHost`, `DefaultsKey`, `punktfunkDefaultMgmtPort`, `DeepLink`, etc. with no call-site churn.
// (Files INSIDE PunktfunkKit still `import PunktfunkShared` explicitly — Swift imports are
// file-scoped; the re-export only reaches downstream modules.)
@_exported import PunktfunkShared
