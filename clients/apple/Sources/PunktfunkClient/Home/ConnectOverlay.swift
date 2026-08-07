// The unified "getting you connected" overlay — one look for BOTH phases of reaching a host, so the
// user gets feedback the instant they pick one and it flows seamlessly into a wake if the host turns
// out to be asleep. The Apple mirror of the Android client's `ConnectOverlay` and the shared console
// UI's connect/wake takeover; it replaces the old centered-card `WakeOverlay`.
//
//   - Connecting (`connectingHostName` non-nil): the dial is in flight. Shown immediately on activate
//     so a host that takes a beat to answer no longer looks like nothing happened.
//   - Waking (`waker.waking` non-nil): the dial failed on a sleeping host, so we're firing
//     Wake-on-LAN and waiting for it to advertise again, escalating to a retry/cancel prompt on
//     timeout.
//
// Presentation is mode-aware: the gamepad ("console") UI gets a full-screen aurora takeover — the
// same living backdrop the console home wears, so it reads as a deliberate 10-foot moment; the
// default touch/desktop UI gets a Liquid Glass modal over a dim scrim, which sits right at home among
// the app's other floating surfaces (the trust card, the HUD) instead of a full-screen aurora that
// looked out of place there.
//
// The two phases hand off within a single view update (HostWaker clears `waking` and starts the
// connect in the same MainActor step), so the overlay never blinks between them. It swallows input to
// the screen behind it, and on iOS/macOS the pad drives it (B cancels, A retries once timed out).

import PunktfunkKit
import SwiftUI

struct ConnectOverlay: View {
    /// Non-nil while a plain dial is in flight (the delegated-approval wait has its own prompt, so it
    /// passes nil here). Drives the "Connecting…" phase.
    let connectingHostName: String?
    @ObservedObject var waker: HostWaker
    /// The console launcher is up → full-screen aurora takeover; otherwise the default UI's Liquid
    /// Glass modal.
    var gamepadUI: Bool
    /// Cancel a dial in flight — tears down the (uncancelable) connect and returns the UI; the late
    /// result is discarded by SessionModel's connect guard.
    var onCancelConnect: () -> Void

    private enum Phase {
        case connecting(name: String)
        case waking(HostWaker.Waking)
    }

    /// Waking takes precedence — it only ever exists after a dial has already failed, so a stray
    /// overlap can't strand the "Connecting…" phase over a wake in progress.
    private var phase: Phase? {
        if let w = waker.waking { return .waking(w) }
        if let name = connectingHostName { return .connecting(name: name) }
        return nil
    }

    @Environment(\.gamepadInk) private var ink

    var body: some View {
        if let phase {
            ZStack {
                if gamepadUI {
                    // Console: an opaque, living aurora over everything, in the chosen palette.
                    // The takeover's own text rides `ink`, so a pale palette flips it here too —
                    // without that this is the one console screen that stays white-on-white.
                    ink.isLight ? Color.white.ignoresSafeArea() : Color.black.ignoresSafeArea()
                    GamepadScreenBackground().ignoresSafeArea()
                    Color.clear.contentShape(Rectangle()).onTapGesture {}
                    content(phase).padding(40).frame(maxWidth: 460)
                } else {
                    // Default UI: a Liquid Glass modal over a dim scrim.
                    Rectangle().fill(.black.opacity(0.5)).ignoresSafeArea()
                        .contentShape(Rectangle()).onTapGesture {}
                    content(phase)
                        .padding(28)
                        .frame(maxWidth: 380)
                        .glassBackground(RoundedRectangle(cornerRadius: 26, style: .continuous))
                        .overlay(
                            RoundedRectangle(cornerRadius: 26, style: .continuous)
                                .strokeBorder(.white.opacity(0.12), lineWidth: 1))
                        .padding(40)
                }
            }
            // The console takeover follows the palette; the default UI's modal stays dark.
            .environment(\.colorScheme, gamepadUI && ink.isLight ? .light : .dark)
            .transition(.opacity)
            #if os(iOS) || os(macOS)
            .background { ConnectControllerInput(waker: waker, onCancelConnect: onCancelConnect) }
            #endif
        }
    }

    /// The overlay's text/glyph colour: the palette's ink in the console takeover — over a pale
    /// aurora, literal white was the one console surface that stayed white-on-white — and white
    /// in the touch modal, whose branch is deliberately forced dark over a black scrim.
    private var overlayFG: Color { gamepadUI ? ink.fg : .white }

    @ViewBuilder private func content(_ phase: Phase) -> some View {
        // The takeover carries larger type than the compact modal.
        let titleSize: CGFloat = gamepadUI ? 24 : 19
        let bodySize: CGFloat = gamepadUI ? 14 : 13
        VStack(spacing: gamepadUI ? 16 : 14) {
            switch phase {
            case .connecting(let name):
                ProgressView().controlSize(.large).tint(overlayFG)
                Text("Connecting to \(name)")
                    .font(.geist(titleSize, .bold, relativeTo: .title3)).foregroundStyle(overlayFG)
                    .multilineTextAlignment(.center)
                Text("Establishing a secure connection…")
                    .font(.geist(bodySize, relativeTo: .caption))
                    .foregroundStyle(overlayFG.opacity(0.6))
                Button("Cancel") { onCancelConnect() }.buttonStyle(.bordered).padding(.top, 6)
            case .waking(let w) where w.timedOut:
                Image(systemName: "moon.zzz.fill")
                    .font(.system(size: gamepadUI ? 40 : 34))
                    .foregroundStyle(overlayFG.opacity(0.9))
                Text("\(w.hostName) didn't wake")
                    .font(.geist(titleSize, .bold, relativeTo: .title3)).foregroundStyle(overlayFG)
                    .multilineTextAlignment(.center)
                Text("It may still be booting, or it's powered off / off this network.")
                    .font(.geist(bodySize, relativeTo: .caption))
                    .foregroundStyle(overlayFG.opacity(0.6))
                    .multilineTextAlignment(.center)
                HStack(spacing: 12) {
                    Button("Cancel") { waker.cancel() }.buttonStyle(.bordered)
                    Button("Try Again") { waker.retry() }.glassProminentButtonStyle()
                }
                .padding(.top, 6)
            case .waking(let w):
                ProgressView().controlSize(.large).tint(overlayFG)
                Text("Waking \(w.hostName)…")
                    .font(.geist(titleSize, .bold, relativeTo: .title3)).foregroundStyle(overlayFG)
                    .multilineTextAlignment(.center)
                Text("Waiting for it to come online · \(w.seconds)s")
                    .font(.geistFixed(bodySize)).foregroundStyle(overlayFG.opacity(0.6))
                    .monospacedDigit()
                // A wake-only wait (no dial after) offers "Stop Waiting"; a wake-&-connect is "Cancel".
                Button(w.connectsAfter ? "Cancel" : "Stop Waiting") { waker.cancel() }
                    .buttonStyle(.bordered).padding(.top, 6)
            }
        }
    }
}

#if os(iOS) || os(macOS)
/// Controller binding for the overlay: B cancels whatever's in flight (a dial or the wake wait); A
/// retries once a wake has timed out. The closures read the live state on each press, so they stay
/// correct across the Connecting ↔ Waking handoff without the view re-mounting. A zero-size backing
/// view owning a `GamepadMenuInput` for the overlay's lifetime (the home is gated inactive while the
/// overlay is up, so nothing else is consuming the pad).
private struct ConnectControllerInput: View {
    @ObservedObject var waker: HostWaker
    var onCancelConnect: () -> Void
    @State private var input = GamepadMenuInput(manager: .shared)

    var body: some View {
        Color.clear
            .onAppear {
                input.onBack = { if waker.waking != nil { waker.cancel() } else { onCancelConnect() } }
                input.onConfirm = { if waker.waking?.timedOut == true { waker.retry() } }
                input.start()
            }
            .onDisappear { input.stop() }
    }
}
#endif
