// The widget extension's entry point. ONE extension target (bundle id io.unom.punktfunk.widgets,
// iOS only) hosts both the launcher widgets and the Live Activity UI. It links PunktfunkShared and
// NOTHING else — never PunktfunkKit (Rust staticlib + presentation layer would blow the widget
// process's ~30 MB budget).
//
// These files are NOT part of the SwiftPM package (Package.swift doesn't declare a PunktfunkWidgets
// target, so `swift build` ignores the directory). They compile only in the Xcode widget-extension
// target you add pointing at this folder — see design/apple-live-activities-and-widgets.md §M1 and
// the GUI checklist.

import SwiftUI
import WidgetKit

@main
struct PunktfunkWidgetBundle: WidgetBundle {
    var body: some Widget {
        HostsWidget()
        LibraryWidget()
        PunktfunkSessionLiveActivity()
    }
}
