// The Live Activity UI (Lock Screen banner + Dynamic Island) for a running session. The app owns
// the Activity's lifecycle (SessionActivityController); this is only its presentation, rendered in
// the widget-extension process from the shared `PunktfunkSessionAttributes`.
//
// The End button runs `EndStreamIntent` (a LiveActivityIntent) IN THE APP's process, which posts
// .punktfunkEndActiveSession → the app disconnects. Elapsed time ticks client-side via
// Text(timerInterval:) — no per-second push.

import ActivityKit
import AppIntents
import SwiftUI
import WidgetKit

import PunktfunkShared

struct PunktfunkSessionLiveActivity: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: PunktfunkSessionAttributes.self) { context in
            LockScreenView(context: context)
                .activitySystemActionForegroundColor(.white)
        } dynamicIsland: { context in
            DynamicIsland {
                DynamicIslandExpandedRegion(.leading) {
                    Label {
                        Text(context.attributes.hostName).font(.caption).lineLimit(1)
                    } icon: {
                        Image(systemName: "play.tv.fill")
                    }
                    .foregroundStyle(.tint)
                }
                DynamicIslandExpandedRegion(.trailing) {
                    Text(timerInterval: context.state.startedAt...Date.distantFuture, countsDown: false)
                        .font(.caption).monospacedDigit()
                        .frame(maxWidth: 56)
                        .foregroundStyle(.secondary)
                }
                DynamicIslandExpandedRegion(.center) {
                    if let title = context.attributes.launchTitle {
                        Text(title).font(.caption2).lineLimit(1).foregroundStyle(.secondary)
                    }
                }
                DynamicIslandExpandedRegion(.bottom) {
                    VStack(spacing: 6) {
                        Text(context.state.modeLine)
                            .font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                        StageLine(state: context.state)
                        EndButton()
                    }
                }
            } compactLeading: {
                Image(systemName: "play.tv.fill").foregroundStyle(.tint)
            } compactTrailing: {
                Text(timerInterval: context.state.startedAt...Date.distantFuture, countsDown: false)
                    .monospacedDigit()
                    .frame(maxWidth: 44)
            } minimal: {
                Image(systemName: "play.tv.fill").foregroundStyle(.tint)
            }
        }
    }
}

// MARK: - Lock Screen banner

private struct LockScreenView: View {
    let context: ActivityViewContext<PunktfunkSessionAttributes>

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: "play.tv.fill")
                .font(.title2)
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(context.attributes.hostName).font(.headline).lineLimit(1)
                    Spacer()
                    Text(timerInterval: context.state.startedAt...Date.distantFuture, countsDown: false)
                        .font(.subheadline).monospacedDigit()
                        .foregroundStyle(.secondary)
                }
                if let title = context.attributes.launchTitle {
                    Text(title).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                Text(context.state.modeLine)
                    .font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                StageLine(state: context.state)
            }
            if context.state.stage == .background {
                EndButton()
            }
        }
        .padding()
    }
}

// MARK: - Shared pieces

/// The stage badge + (while backgrounded) the auto-disconnect countdown.
private struct StageLine: View {
    let state: PunktfunkSessionAttributes.ContentState

    var body: some View {
        switch state.stage {
        case .streaming:
            EmptyView()
        case .background:
            if let deadline = state.backgroundDeadline {
                HStack(spacing: 3) {
                    Text("Keeps running for")
                    Text(timerInterval: Date()...deadline, countsDown: true)
                        .monospacedDigit()
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
            } else {
                badge("Running in background", .orange)
            }
        case .reconnecting:
            badge("Reconnecting…", .yellow)
        case .ending:
            badge("Session ended", .secondary)
        }
    }

    private func badge(_ text: String, _ color: Color) -> some View {
        Text(text).font(.caption2).foregroundStyle(color)
    }
}

/// End-stream button — runs EndStreamIntent in the app process (LiveActivityIntent).
private struct EndButton: View {
    var body: some View {
        Button(intent: EndStreamIntent()) {
            Label("End", systemImage: "stop.fill")
                .font(.caption).bold()
        }
        .tint(.red)
        .buttonStyle(.bordered)
    }
}
