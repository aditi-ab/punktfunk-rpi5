// The host grid's cards: a saved host (tap to connect, context menu) and an mDNS-discovered
// host (tap to save + connect). Both share the "monogram module" look — a squared brand-purple
// monogram tile + a left-aligned bold Geist name over monospaced technical metadata
// (address, status), framed by a hairline panel border. Industrial, not soft.

import PunktfunkKit
import SwiftUI

/// Shared host-card sizing — touch-first on iOS, compact on macOS, roomy on tvOS.
private struct CardMetrics {
    let tile: CGFloat       // monogram tile side
    let monogram: CGFloat   // monogram letter point size
    let name: CGFloat       // host-name point size
    let meta: CGFloat       // address (mono) point size
    let status: CGFloat     // status-label (mono) point size
    let padding: CGFloat
    let spacing: CGFloat    // tile ↔ text gap
    let radius: CGFloat

    static var current: CardMetrics {
        #if os(iOS)
        CardMetrics(tile: 54, monogram: 26, name: 19, meta: 13, status: 11,
                    padding: 16, spacing: 14, radius: 12)
        #elseif os(tvOS)
        // 10-foot sizes — the 24pt-name tier read like a phone card from the couch.
        CardMetrics(tile: 84, monogram: 42, name: 30, meta: 20, status: 17,
                    padding: 24, spacing: 22, radius: 18)
        #else
        CardMetrics(tile: 44, monogram: 21, name: 15, meta: 12, status: 10.5,
                    padding: 13, spacing: 12, radius: 10)
        #endif
    }
}

/// First letter of a host name, uppercased — the monogram glyph. Falls back to a bullet.
private func monogram(_ name: String) -> String {
    guard let first = name.trimmingCharacters(in: .whitespacesAndNewlines).first else { return "•" }
    return String(first).uppercased()
}

/// The squared host tile. `filled` = a solid brand-purple chip (saved hosts); otherwise a tinted
/// outline (discovered hosts). Shows a spinner in place of the glyph while connecting.
///
/// `mark` is the host's OS mark, and it REPLACES the monogram when we have one: it identifies the
/// machine better than its initial ever did, and on a row of similarly-named boxes the initial says
/// nothing the name beneath it doesn't already say. A host that advertises no OS chain — or one we
/// ship no art for — keeps its letter, so a mixed row still reads as one set.
private func monogramTile(
    _ letter: String, osChain: String?, m: CardMetrics, connecting: Bool, filled: Bool
) -> some View {
    let shape = RoundedRectangle(cornerRadius: m.radius - 3, style: .continuous)
    return ZStack {
        shape.fill(filled
            ? AnyShapeStyle(LinearGradient(
                colors: [Color.brand, Color.brand.opacity(0.72)],
                startPoint: .top, endPoint: .bottom))
            : AnyShapeStyle(Color.brand.opacity(0.14)))
        if connecting {
            ProgressView().tint(filled ? .white : Color.brand)
        } else if let mark = osIconImage(for: osChain) {
            // Template asset — tints from foregroundStyle exactly like the letter it stands in for.
            // Labelled, because this is where the OS is now announced: it used to ride the status
            // row below, which no longer carries it.
            mark
                .resizable()
                .scaledToFit()
                .frame(width: m.monogram, height: m.monogram)
                .foregroundStyle(filled ? Color.white : Color.brand)
                .accessibilityLabel(osChain ?? "")
        } else {
            // Fixed size (not Dynamic Type): the glyph is pinned inside a fixed tile, so it must
            // not scale up and spill out at large accessibility text sizes. minimumScaleFactor +
            // the clip below are belt-and-suspenders for an unusually wide glyph.
            Text(letter)
                .font(.geistFixed(m.monogram, .bold))
                .minimumScaleFactor(0.5)
                .lineLimit(1)
                .foregroundStyle(filled ? Color.white : Color.brand)
        }
    }
    .frame(width: m.tile, height: m.tile)
    .clipShape(shape)
    .overlay {
        if !filled {
            shape.strokeBorder(Color.brand.opacity(0.45), lineWidth: 1)
        }
    }
}

/// Everything a card's profile affordances need: the catalog to offer, what this host is bound
/// and pinned to, and the acts a menu can perform (design/client-settings-profiles.md §5.2/§5.2a).
///
/// Passed as one value rather than eight closures because every surface that renders a host card
/// has to offer the SAME set — a menu that quietly lacks "Pin as card" on one screen is how a
/// feature becomes folklore.
struct HostProfileMenu {
    var profiles: [StreamProfile]
    /// The host's default profile — the chip, and the checkmark in "Connect with ▸".
    var boundID: String?
    var pinnedIDs: [String]
    /// A ONE-OFF connect. Never rebinds: rebinding is `setDefault`, an explicit act (§5.2).
    var connectWith: (ProfileSelection) -> Void
    var setDefault: (String?) -> Void
    var togglePin: (String) -> Void
    /// Copy a `punktfunk://` link for this host, optionally carrying a profile.
    var copyLink: (String?) -> Void
}

/// A saved host. A left accent bar marks the most-recently-connected one; the context menu
/// pairs / speed-tests / forgets / removes. Disabled while a session is busy.
///
/// The same view renders a PINNED host+profile card (§5.2a): same host, same live status, with
/// the profile as the prominent subtitle. A pinned card is a shortcut, not a second host, so its
/// menu carries only connect-shaped actions — edit/pair/forget/remove stay on the primary card,
/// where the thing they act on actually lives.
struct HostCardView: View {
    let host: StoredHost
    /// Currently advertising on the LAN (matched against live mDNS discovery). False means
    /// "not seen on this network" — off, or a remote/cross-subnet host we can't observe.
    let isOnline: Bool
    let isConnecting: Bool
    let isMostRecent: Bool
    let isBusy: Bool
    let onConnect: () -> Void
    let onPair: () -> Void
    let onSpeedTest: () -> Void
    let onForget: () -> Void
    let onRemove: () -> Void
    /// Open this host's game library — a MENU action. `nil` — no library affordance at all — when
    /// the setting is off or the host is unpaired (the library plane needs the pinned identity).
    ///
    /// Never the card's primary tap: tapping a host connects to it, on every surface. Browsing is
    /// one step further in, exactly where the console shell keeps it (Y on a tile).
    var onBrowseLibrary: (() -> Void)? = nil
    /// Send a Wake-on-LAN magic packet. Shown only when the host is offline and we have a stored
    /// MAC to target (a tap-to-connect already auto-wakes; this is the explicit "just wake it").
    var onWake: (() -> Void)? = nil
    /// Open the edit sheet (name / address / port / Wake-on-LAN MAC).
    var onEdit: (() -> Void)? = nil
    /// Upload this device's recent log to the host (`SendLogs`). `nil` when the host is unpaired —
    /// the upload is authenticated by the pairing, so there is nothing to offer before it.
    var onSendLogs: (() -> Void)? = nil
    /// What this host says this device may do TO it — sleep, restart, shut it down
    /// (`design/host-actions.md` §7). Empty on every surface that doesn't offer them, and on
    /// every host that hasn't answered or hasn't granted them.
    var hostActions: [HostAction] = []
    /// Run one of the above. `nil` alongside a non-empty `hostActions` would be a bug, so the
    /// rows render disabled in that case rather than silently doing nothing.
    var onHostAction: ((HostAction) -> Void)? = nil
    /// This card's profile affordances — nil on surfaces that don't offer them.
    var profileMenu: HostProfileMenu? = nil
    /// Set on a PINNED card: the profile this card connects with. nil = the host's primary card,
    /// which follows the binding.
    var pinnedProfile: StreamProfile? = nil

    /// The profile this card announces: a pinned card's own, else the host's binding.
    private var shownProfile: StreamProfile? {
        pinnedProfile ?? profileMenu.flatMap { menu in
            menu.boundID.flatMap { id in menu.profiles.first { $0.id == id } }
        }
    }

    var body: some View {
        let m = CardMetrics.current
        return Button(action: onConnect) {
            HStack(spacing: m.spacing) {
                monogramTile(monogram(host.displayName), osChain: host.osChain,
                             m: m, connecting: isConnecting, filled: true)
                VStack(alignment: .leading, spacing: 4) {
                    // The chip rides the TITLE line, anchored to the card's trailing edge — not
                    // trailing the name, where it read as part of the title, and not on a line of
                    // its own, where it made cards with a profile taller than cards without and a
                    // pinned card stuck out of its grid row. The spacer is what anchors it: the
                    // name truncates against that gap instead of ever running into the chip.
                    HStack(spacing: 6) {
                        Text(host.displayName)
                            .font(.geist(m.name, .bold, relativeTo: .title3))
                            .foregroundStyle(.primary)
                            .lineLimit(1)
                        Spacer(minLength: 8)
                        if let profile = shownProfile {
                            ProfileChip(
                                profile: profile, size: m.status,
                                prominent: pinnedProfile != nil)
                                // The name gives up width first: a truncated host name still reads,
                                // a truncated profile name is the one thing the chip exists to say.
                                .layoutPriority(1)
                        }
                    }
                    Text("\(host.address):\(String(host.port))")
                        .font(.geist(m.meta, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    statusRow(m)
                }
                // Fills the card so the title row's trailing spacer reaches the real edge; without
                // it the text column hugs its content and "trailing" means "just after the name".
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(m.padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            #if !os(tvOS)
            // tvOS: the .card button style owns platter + focus motion; extra chrome mutes it.
            // Elsewhere: a flat material panel with a hairline border (industrial, not a soft blob),
            // and a brand accent bar down the leading edge for the most-recent host.
            .background(.regularMaterial)
            .overlay(alignment: .leading) {
                if isMostRecent {
                    Rectangle().fill(Color.brand).frame(width: 3)
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: m.radius, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: m.radius, style: .continuous)
                    .strokeBorder(.quaternary, lineWidth: 1)
            }
            #endif
        }
        #if os(tvOS)
        .buttonStyle(.card)
        #elseif os(iOS)
        .buttonStyle(HostCardButtonStyle(cornerRadius: m.radius))
        #else
        .buttonStyle(.plain)
        #endif
        .disabled(isBusy)
        .contextMenu { menuItems }
    }

    @ViewBuilder private var menuItems: some View {
        if let pinned = pinnedProfile, let menu = profileMenu {
            // A pinned card is a shortcut, not a second host: only connect-shaped actions, plus
            // the way to remove the shortcut itself. Unpinning touches neither the profile nor
            // the host's default binding.
            connectWithMenu(menu)
            // Browsing IS a connect-shaped action — it is this card's connect with a title picked
            // first — so a pinned card offers it and opens its own shelf, whose launches carry the
            // pinned profile. (Pair / speed test / wake / forget stay on the host's card: those
            // are about the machine, and a shortcut has no business claiming them.)
            if let onBrowseLibrary {
                Button("Browse Library…", action: onBrowseLibrary)
            }
            if LinkClipboard.isAvailable {
                Button("Copy Link") { menu.copyLink(pinned.id) }
            }
            Button("Unpin Card", systemImage: "pin.slash", role: .destructive) {
                menu.togglePin(pinned.id)
            }
        } else {
            if let onEdit {
                Button("Edit…", systemImage: "pencil", action: onEdit)
            }
            if let menu = profileMenu {
                connectWithMenu(menu)
                pinMenu(menu)
                if LinkClipboard.isAvailable {
                    Button("Copy Link") { menu.copyLink(nil) }
                }
            }
            Button("Pair with PIN…", action: onPair)
            Button("Test Network Speed…", action: onSpeedTest)
            if let onBrowseLibrary {
                Button("Browse Library…", action: onBrowseLibrary)
            }
            if let onSendLogs {
                Button("Send Logs to Host", action: onSendLogs)
            }
            if !isOnline, !host.wakeMacs.isEmpty, PunktfunkConnection.wakeOnLANAvailable, let onWake {
                Button("Wake Host", systemImage: "power", action: onWake)
            }
            // …and the other half of that round trip: what the HOST says this device may do to
            // it. Empty unless it answered and this device's access carries the grant, so no row
            // here can be refused for permission. A destructive one confirms in the caller.
            ForEach(hostActions) { action in
                Button(
                    action.available ? action.label : "\(action.label) (Unavailable)",
                    systemImage: "power",
                    role: action.danger ? .destructive : nil
                ) { onHostAction?(action) }
                .disabled(onHostAction == nil)
            }
            if host.pinnedSHA256 != nil {
                // Dropping the pin does NOT downgrade to TOFU: the next connect must re-pair via
                // PIN (unless the host advertises pair=optional). Wording reflects that.
                Button("Forget Identity (re-pair to reconnect)", action: onForget)
            }
            Button("Remove", role: .destructive, action: onRemove)
        }
    }

    /// "Connect with ▸" — a ONE-OFF pick that never rebinds the host, with a checkmark on what a
    /// plain click would use. Its last item is the explicit way to rebind, for the users who do
    /// want that from here.
    @ViewBuilder private func connectWithMenu(_ menu: HostProfileMenu) -> some View {
        if !menu.profiles.isEmpty {
            Menu("Connect with") {
                Button {
                    menu.connectWith(.defaults)
                } label: {
                    checkable("Default settings", on: menu.boundID == nil)
                }
                ForEach(menu.profiles) { profile in
                    Button {
                        menu.connectWith(.profile(profile.id))
                    } label: {
                        checkable(profile.name, on: menu.boundID == profile.id)
                    }
                }
                Divider()
                Menu("Set Default Profile") {
                    Button("Default settings") { menu.setDefault(nil) }
                    ForEach(menu.profiles) { profile in
                        Button(profile.name) { menu.setDefault(profile.id) }
                    }
                }
            }
        }
    }

    /// "Pin as card ▸" — a host+profile combo gets its own one-click card in the grid, which is
    /// what turns a regularly-used profile from a menu dive into a press (§5.2a).
    @ViewBuilder private func pinMenu(_ menu: HostProfileMenu) -> some View {
        if !menu.profiles.isEmpty {
            Menu("Pin as Card") {
                ForEach(menu.profiles) { profile in
                    Button {
                        menu.togglePin(profile.id)
                    } label: {
                        checkable(profile.name, on: menu.pinnedIDs.contains(profile.id))
                    }
                }
            }
        }
    }

    /// A menu row that carries a checkmark when it is the current choice. Built as two shapes
    /// rather than one `Label` with an empty symbol name — `Image(systemName: "")` is not a
    /// blank image, it is an invalid one.
    @ViewBuilder private func checkable(_ title: String, on: Bool) -> some View {
        if on {
            Label(title, systemImage: "checkmark")
        } else {
            Text(title)
        }
    }

    /// Technical status line: a square presence pip + monospaced ONLINE/OFFLINE, and PAIRED when a
    /// certificate is pinned (the lock state, spelled out).
    @ViewBuilder private func statusRow(_ m: CardMetrics) -> some View {
        HStack(spacing: 6) {
            // The OS mark used to lead this row; it is the tile's glyph now (see monogramTile).
            RoundedRectangle(cornerRadius: 1.5)
                .fill(isOnline ? Color.green : Color.secondary.opacity(0.4))
                .frame(width: 6, height: 6)
                // The state is spelled out in the adjacent text, so the pip is decorative —
                // otherwise VoiceOver reads the status twice ("Online, ONLINE …").
                .accessibilityHidden(true)
            Text(isOnline ? "ONLINE" : "OFFLINE")
            if host.pinnedSHA256 != nil {
                Text("· PAIRED")
            }
        }
        .font(.geist(m.status, .medium, relativeTo: .caption2))
        .tracking(0.8)
        .foregroundStyle(.secondary)
        .lineLimit(1)
    }
}

/// The profile a card connects with, as a tinted pill. Quiet on a bound primary card (it only
/// answers "what will a click do?"); prominent on a pinned card, where the profile IS the reason
/// the card exists — which is where the catalog's `accent` earns its keep.
///
/// Prominence is fill and weight only, never TYPE SIZE: the chip sits on the card's title line,
/// and a chip taller than the name would make pinned cards taller than their host's.
struct ProfileChip: View {
    let profile: StreamProfile
    let size: CGFloat
    var prominent = false

    var body: some View {
        let tint = profile.accentColor
        return HStack(spacing: 5) {
            Circle()
                .fill(tint)
                .frame(width: size * 0.55, height: size * 0.55)
                .accessibilityHidden(true) // the name is right there
            Text(profile.name)
                .font(.geist(size, prominent ? .bold : .semibold, relativeTo: .caption2))
                .lineLimit(1)
        }
        .foregroundStyle(tint)
        .padding(.horizontal, 7)
        .padding(.vertical, 2)
        .background(Capsule().fill(tint.opacity(prominent ? 0.24 : 0.12)))
        .accessibilityLabel("Profile \(profile.name)")
    }
}

/// A host found on the LAN but not yet saved. A tinted-outline monogram + dashed panel border
/// distinguish it from saved cards; tapping saves it and connects (or pairs, if required).
struct DiscoveredCardView: View {
    let discovered: DiscoveredHost
    let isBusy: Bool
    let onConnect: () -> Void

    var body: some View {
        let m = CardMetrics.current
        return Button(action: onConnect) {
            HStack(spacing: m.spacing) {
                monogramTile(monogram(discovered.name), osChain: discovered.osChain,
                              m: m, connecting: false, filled: false)
                VStack(alignment: .leading, spacing: 4) {
                    Text(discovered.name)
                        .font(.geist(m.name, .bold, relativeTo: .title3))
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    Text("\(discovered.host):\(String(discovered.port))")
                        .font(.geist(m.meta, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    HStack(spacing: 6) {
                        // The advert's OS mark is the tile's glyph now (see monogramTile).
                        Image(systemName: discovered.requiresPairing
                            ? "lock.fill" : "antenna.radiowaves.left.and.right")
                            .font(.system(size: m.status))
                            .accessibilityHidden(true) // decorative; the adjacent text says the state
                        Text(discovered.requiresPairing ? "PAIRING REQUIRED" : "DISCOVERED")
                    }
                    .font(.geist(m.status, .medium, relativeTo: .caption2))
                    .tracking(0.8)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                }
                Spacer(minLength: 0)
            }
            .padding(m.padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            #if !os(tvOS)
            .background(.regularMaterial)
            .clipShape(RoundedRectangle(cornerRadius: m.radius, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: m.radius, style: .continuous)
                    .strokeBorder(
                        Color.secondary.opacity(0.3),
                        style: StrokeStyle(lineWidth: 1, dash: [4, 3]))
            }
            #endif
        }
        #if os(tvOS)
        .buttonStyle(.card)
        #elseif os(iOS)
        .buttonStyle(HostCardButtonStyle(cornerRadius: m.radius))
        #else
        .buttonStyle(.plain)
        #endif
        .disabled(isBusy)
    }
}

#if os(iOS)
/// The iOS host-card press/hover treatment, one style for both idioms:
/// - iPhone: a subtle scale-down on press + a light impact haptic on press-down. (`hoverEffect` is
///   inert without a pointer.)
/// - iPad: the system pointer "magnet" — the cursor morphs into a highlight that conforms to the
///   card's rounded rect on hover. (`sensoryFeedback` is inert without a Taptic Engine, and the
///   press scale doubles as click feedback.)
struct HostCardButtonStyle: ButtonStyle {
    var cornerRadius: CGFloat

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.96 : 1)
            .animation(.spring(response: 0.3, dampingFraction: 0.65), value: configuration.isPressed)
            // Conform the pointer highlight to the card's rounded rect, not its square bounds.
            .contentShape(.hoverEffect, RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
            .hoverEffect(.highlight)
            // Light tap on press-down (nil on release so it fires once, on touch). No haptic
            // hardware on iPad → silently ignored there.
            .sensoryFeedback(trigger: configuration.isPressed) { _, pressed in
                pressed ? .impact(weight: .light) : nil
            }
    }
}
#endif
