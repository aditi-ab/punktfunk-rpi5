// The "Waking <host>…" modal shown while HostWaker brings a sleeping host back — a spinner + a
// live elapsed counter, escalating to a retry/cancel prompt on timeout. Presented over BOTH the
// touch and gamepad home (a wake only ever starts on macOS today, where WoL is ungated), and it
// drives from either a pointer (the buttons) or a controller (B cancels, A retries once timed out).

import PunktfunkKit
import SwiftUI

struct WakeOverlay: View {
    @ObservedObject var waker: HostWaker

    var body: some View {
        if let w = waker.waking {
            ZStack {
                // Dim + swallow input to the home behind it.
                Rectangle().fill(.black.opacity(0.6)).ignoresSafeArea()
                    .contentShape(Rectangle())
                    .onTapGesture {}
                card(w)
                    .frame(maxWidth: 380)
                    .padding(28)
                    .consoleGlass(RoundedRectangle(cornerRadius: 22, style: .continuous))
                    .overlay(
                        RoundedRectangle(cornerRadius: 22, style: .continuous)
                            .strokeBorder(.white.opacity(0.12), lineWidth: 1))
                    .padding(40)
            }
            .environment(\.colorScheme, .dark)
            .transition(.opacity)
            #if os(iOS) || os(macOS)
            .background { WakeControllerInput(waker: waker) }
            #endif
        }
    }

    @ViewBuilder private func card(_ w: HostWaker.Waking) -> some View {
        VStack(spacing: 14) {
            if w.timedOut {
                Image(systemName: "moon.zzz.fill")
                    .font(.system(size: 34)).foregroundStyle(.white.opacity(0.85))
                Text("\(w.hostName) didn't wake")
                    .font(.geist(19, .bold, relativeTo: .title3)).foregroundStyle(.white)
                Text("It may still be booting, or it's powered off / off this network.")
                    .font(.geist(13, relativeTo: .caption)).foregroundStyle(.white.opacity(0.6))
                    .multilineTextAlignment(.center)
                HStack(spacing: 12) {
                    Button("Cancel") { waker.cancel() }.buttonStyle(.bordered)
                    Button("Try Again") { waker.retry() }.glassProminentButtonStyle()
                }
                .padding(.top, 6)
            } else {
                ProgressView().controlSize(.large).tint(.white)
                Text("Waking \(w.hostName)…")
                    .font(.geist(19, .bold, relativeTo: .title3)).foregroundStyle(.white)
                Text("Waiting for it to come online · \(w.seconds)s")
                    .font(.geistFixed(13)).foregroundStyle(.white.opacity(0.6))
                    .monospacedDigit()
                Button(w.connectsAfter ? "Cancel" : "Stop Waiting") { waker.cancel() }
                    .buttonStyle(.bordered)
                    .padding(.top, 6)
            }
        }
    }
}

#if os(iOS) || os(macOS)
/// Controller binding for the overlay: B cancels; A retries once it has timed out. A zero-size
/// backing view owning a `GamepadMenuInput` for the overlay's lifetime (the home carousel/list is
/// gated inactive while a wake is up, so nothing else is consuming the pad).
private struct WakeControllerInput: View {
    @ObservedObject var waker: HostWaker
    @State private var input = GamepadMenuInput(manager: .shared)

    var body: some View {
        Color.clear
            .onAppear {
                input.onBack = { waker.cancel() }
                input.onConfirm = { if waker.waking?.timedOut == true { waker.retry() } }
                input.start()
            }
            .onDisappear { input.stop() }
    }
}
#endif
