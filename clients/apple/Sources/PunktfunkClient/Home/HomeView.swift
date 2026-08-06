// The home screen: a grid of saved hosts + an "On this network" section of mDNS-discovered
// hosts, with the add/settings toolbar and the pairing / speed-test / add / settings
// navigation. The connect logic lives in ContentView (it reads the @AppStorage stream mode) and
// is passed in as closures.

import PunktfunkKit
import SwiftUI
// The tvOS slide transition ships with the Xcode PROJECT only — its manifest breaks SwiftPM's
// whole-graph validation, so `swift build` never sees it. `canImport` rather than `os(tvOS)` so
// the tvOS sources still TYPECHECK from the command line (the hand recipe in the Apple client's
// README), where the module is genuinely absent; in the app it is present and the transition
// applies exactly as before.
#if os(tvOS) && canImport(SwiftUINavigationTransitions)
import SwiftUINavigationTransitions
#endif

struct HomeView: View {
    @ObservedObject var store: HostStore
    @ObservedObject var model: SessionModel
    @ObservedObject var discovery: HostDiscovery
    /// The profile catalog — the source of the card chips, the "Connect with ▸" menu, and the
    /// pinned host+profile cards the grid renders alongside their host (design §5.2a).
    @ObservedObject private var profiles = ProfileStore.shared
    @Binding var showAddHost: Bool
    @Binding var pairingTarget: StoredHost?
    @Binding var speedTestTarget: StoredHost?
    @Binding var libraryTarget: StoredHost?
    #if !os(macOS)
    @Binding var showSettings: Bool
    #endif
    /// Start a session with this host, using the given profile selection — `.inherit` for a plain
    /// card tap (the host's binding), an explicit pick from "Connect with ▸" or a pinned card.
    let connect: (StoredHost, ProfileSelection) -> Void
    let connectDiscovered: (DiscoveredHost) -> Void
    /// Pairing succeeded (tvOS PairSheet route) — pin + connect (ContentView guards staleness).
    let onPaired: (StoredHost, Data) -> Void
    /// Picked a title in the (experimental) library — start a session that launches it.
    let onLaunchTitle: (StoredHost, String) -> Void
    /// Explicit Wake-on-LAN of an offline host — fires the packet and waits for it to come online
    /// (the "Waking…" overlay), without connecting. Routed through ContentView's HostWaker.
    let wake: (StoredHost) -> Void
    /// Game-library browser (default ON; the Settings toggle opts out) — the host-card
    /// "Browse Library…" action.
    @AppStorage(DefaultsKey.libraryEnabled) private var libraryEnabled = true
    /// The host being edited (name / address / port / Wake-on-LAN MAC) — drives the edit sheet.
    @State private var editTarget: StoredHost?
    // How this device shows its own list. `.added` is the default because it is what the grid
    // did before it could sort at all — an update should not rearrange anyone's hosts.
    @AppStorage(DefaultsKey.hostSort) private var sortRaw = HostSort.added.rawValue
    @AppStorage(DefaultsKey.hostGrouping) private var groupingRaw = HostGrouping.none.rawValue

    var body: some View {
        NavigationStack {
            Group {
                if store.hosts.isEmpty && discoveredUnsaved.isEmpty {
                    #if os(tvOS)
                    emptyState // no pull-to-refresh on a remote; the action row carries Refresh
                    #else
                    // Inside a ScrollView purely so the pull gesture works on the ONE screen
                    // where a rescan matters most: the one that found nothing.
                    ScrollView {
                        emptyState
                            .frame(maxWidth: .infinity)
                            .containerRelativeFrame(.vertical)
                    }
                    .refreshable { await discovery.rescan() }
                    #endif
                } else {
                    ScrollView {
                        if !store.hosts.isEmpty {
                            LazyVStack(alignment: .leading, spacing: gridSpacing) {
                                ForEach(hostGroups) { group in
                                    if let title = group.title {
                                        groupHeader(title, accent: group.accent)
                                    }
                                    LazyVGrid(columns: gridColumns, spacing: gridSpacing) {
                                        ForEach(group.cards) { card in
                                            hostCard(card.host, pinned: card.pinned)
                                        }
                                    }
                                }
                            }
                            .padding()
                            // Mirror of the action row's focusSection below: an UPWARD move from
                            // the centered buttons must land back in the grid even when no card
                            // sits in the buttons' columns (a lone top-left card, say). The grid
                            // spans the row, so the section catches every upward ray.
                            #if os(tvOS)
                            .focusSection()
                            #endif
                        }
                        if !discoveredUnsaved.isEmpty {
                            discoveredSection
                        }
                        #if os(tvOS)
                        // Actions live below the hosts, not between them.
                        HStack(spacing: 32) {
                            Button {
                                showAddHost = true
                            } label: {
                                Label("Add Host", systemImage: "plus")
                            }
                            Button {
                                showSettings = true
                            } label: {
                                Label("Settings", systemImage: "gearshape")
                            }
                            refreshButton
                        }
                        .padding(.top, 24)
                        // One FULL-WIDTH focus target for any downward move out of the grid.
                        // focusSection alone is not enough: the engine tests the section's
                        // FRAME, and a content-hugging centered HStack only overlaps the middle
                        // columns — a swipe down from an outer card dead-ends and the actions
                        // are unreachable by remote. Stretching the section across the row means
                        // every column's downward ray hits it.
                        .frame(maxWidth: .infinity)
                        .focusSection()
                        #endif
                    }
                    #if !os(tvOS)
                    .refreshable { await discovery.rescan() }
                    #endif
                }
            }
            .navigationTitle("Punktfunk")
            // Browse the LAN for advertised hosts only while the grid is up — not during a
            // session. The home appears/disappears as the stream swaps in and out.
            .onAppear { discovery.start() }
            .onDisappear { discovery.stop() }
            // Reachability sweep while the grid is up: a saved host reached only over a routed
            // network (Tailscale/VPN) never advertises on mDNS, so `advertises` can't see it. Probe
            // every non-advertising saved host ~every 10 s and publish the reachable set for the
            // pips (`isOnline` above OR's it in). The `.task` is cancelled on disappear, matching
            // `discovery.stop()`.
            .task {
                while !Task.isCancelled {
                    await store.refreshReachability(discovery: discovery)
                    try? await Task.sleep(for: .seconds(10))
                }
            }
            #if os(tvOS)
            // Pushed routes — the Settings-app navigation feel (push animation, Menu
            // pops) instead of modal overlays.
            .navigationDestination(isPresented: $showAddHost) {
                AddHostSheet { store.add($0) }
            }
            .navigationDestination(isPresented: $showSettings) {
                SettingsView()
            }
            .navigationDestination(item: $pairingTarget) { host in
                PairSheet(host: host) { fingerprint in onPaired(host, fingerprint) }
            }
            .navigationDestination(item: $speedTestTarget) { host in
                SpeedTestSheet(host: host)
            }
            .navigationDestination(item: $libraryTarget) { host in
                LibraryView(store: store, host: host, onLaunch: { onLaunchTitle(host, $0) })
            }
            #endif
            #if !os(tvOS)
            .toolbar {
                #if os(iOS)
                // Adjacent trailing items share one glass pill (the system default).
                ToolbarItem(placement: .topBarTrailing) { settingsButton }
                if showsArrangeMenu {
                    ToolbarItem(placement: .topBarTrailing) { arrangeMenu }
                }
                ToolbarItem(placement: .topBarTrailing) { refreshButton }
                ToolbarItem(placement: .topBarTrailing) { addHostButton }
                #else
                if showsArrangeMenu {
                    ToolbarItem(placement: .primaryAction) {
                        arrangeMenu
                            .help("Sort and group the host list")
                    }
                }
                ToolbarItem(placement: .primaryAction) {
                    refreshButton
                        .help("Scan the network for hosts again")
                }
                ToolbarItem(placement: .primaryAction) {
                    addHostButton
                        .help("Add a host")
                }
                ToolbarItem {
                    SettingsLink {
                        Label("Settings", systemImage: "gearshape")
                    }
                    .help("Stream mode and settings")
                }
                #endif
            }
            #endif
        }
        #if os(macOS)
        .frame(minWidth: 480, minHeight: 360)
        #endif
        #if os(tvOS) && canImport(SwiftUINavigationTransitions)
        // The Settings-app slide for every push in this stack (top-level routes AND
        // the pickers' drill-ins) — SwiftUI's default on tvOS is a bare crossfade.
        // Spring-driven (UISpringTimingParameters): ~0.87 damping ratio — settles fast
        // with just a hint of life, no visible overshoot ping-pong.
        .customNavigationTransition(
            .slide.animation(.interpolatingSpring(stiffness: 300, damping: 30)))
        #endif
        #if !os(tvOS)
        .sheet(isPresented: $showAddHost) {
            AddHostSheet { store.add($0) }
        }
        .sheet(item: $editTarget) { host in
            // Prefill the MAC from the live advert when the host hasn't stored one yet.
            AddHostSheet(
                existing: host,
                suggestedMacs: discovery.hosts.first { host.matches($0) }?.macAddresses ?? [],
                onSave: { store.update($0) })
        }
        #if os(iOS)
        // SettingsView owns its own NavigationSplitView (sidebar + detail) and Done button, so it
        // is presented directly — wrapping it in a NavigationStack here would nest a split view in
        // a stack (double title bars). `settingsSheetSizing()` widens the sheet on iPad for the
        // two-column layout.
        .sheet(isPresented: $showSettings) {
            SettingsView()
                .settingsSheetSizing()
        }
        #endif
        #endif
    }

    // MARK: - Cards

    /// The grid's bands, ordered and divided per this device's preference — cards and all, so a
    /// pinned card can be filed under the profile it connects with rather than under its host's
    /// binding (`HostArrangement`).
    private var hostGroups: [HostGroup] {
        HostArrangement.groups(
            hosts: store.hosts, catalog: profiles.catalog,
            online: Set(store.hosts.filter(isOnline).map(\.id)),
            sort: HostSort(rawValue: sortRaw) ?? .added,
            grouping: HostGrouping(rawValue: groupingRaw) ?? .none)
    }

    /// Online = advertising on mDNS OR answered the reachability probe (a routed/VPN host never
    /// advertises). One definition, used by the cards and by the Status grouping alike.
    private func isOnline(_ host: StoredHost) -> Bool {
        discovery.advertises(host) || store.probedOnline.contains(host.id)
    }

    private func groupHeader(_ title: String, accent: String?) -> some View {
        HStack(spacing: 7) {
            Circle()
                .fill(Color(hex: accent ?? "") ?? Color.secondary.opacity(0.5))
                .frame(width: 7, height: 7)
                .accessibilityHidden(true) // the title says it
            Text(title)
                .font(.geist(13, .semibold, relativeTo: .subheadline))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)
                .tracking(0.6)
        }
        .padding(.top, 4)
    }

    private func hostCard(_ host: StoredHost, pinned: StreamProfile?) -> some View {
        let onBrowseLibrary: (() -> Void)? = libraryEnabled ? { libraryTarget = host } : nil
        // A pinned card connects with ITS profile; the primary card follows the binding.
        let selection: ProfileSelection = pinned.map { .profile($0.id) } ?? .inherit
        return HostCardView(
            host: host,
            isOnline: isOnline(host),
            isConnecting: model.phase == .connecting && model.activeHost?.id == host.id,
            // The accent ring marks the most recent HOST; pinned cards stay quiet so the grid
            // doesn't grow several "most recent" bars for one machine.
            isMostRecent: pinned == nil && host.id == mostRecentHostID,
            isBusy: model.isBusy,
            onConnect: { connect(host, selection) },
            onPair: { if !model.isBusy { pairingTarget = host } },
            onSpeedTest: { if !model.isBusy { speedTestTarget = host } },
            onForget: { store.forgetIdentity(host) },
            onRemove: { store.remove(host) },
            onBrowseLibrary: onBrowseLibrary,
            onWake: { wake(host) },
            onEdit: { editTarget = host },
            profileMenu: profileMenu(for: host),
            pinnedProfile: pinned)
    }

    /// The profile affordances every host card carries (§5.2/§5.2a).
    private func profileMenu(for host: StoredHost) -> HostProfileMenu {
        HostProfileMenu(
            profiles: profiles.profiles,
            boundID: host.profileID,
            pinnedIDs: host.pinnedProfileIDs ?? [],
            connectWith: { selection in connect(host, selection) },
            setDefault: { store.setProfile(host.id, profileID: $0) },
            togglePin: { id in
                let pinned = (host.pinnedProfileIDs ?? []).contains(id)
                store.setPinned(host.id, profileID: id, pinned: !pinned)
            },
            copyLink: { profileID in
                LinkClipboard.copy(DeepLink.forHost(host, profile: profileID).urlString)
            })
    }

    private var discoveredSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("On this network", systemImage: "antenna.radiowaves.left.and.right")
                .font(.geist(15, .semibold, relativeTo: .headline))
                .foregroundStyle(.secondary)
                .padding(.horizontal)
            LazyVGrid(columns: gridColumns, spacing: gridSpacing) {
                ForEach(discoveredUnsaved) { discovered in
                    DiscoveredCardView(
                        discovered: discovered, isBusy: model.isBusy,
                        onConnect: { connectDiscovered(discovered) })
                }
            }
        }
        .padding([.horizontal, .bottom])
        .padding(.top, store.hosts.isEmpty ? 0 : 8)
        // Same reachability contract as the saved grid above — see its focusSection comment.
        #if os(tvOS)
        .focusSection()
        #endif
    }

    /// Discovered hosts not already saved (see `HostDiscovery.unsaved` — shared with the gamepad
    /// launcher so both screens classify hosts identically).
    private var discoveredUnsaved: [DiscoveredHost] {
        discovery.unsaved(among: store.hosts)
    }

    /// The host of the most recent session — its card carries the accent ring.
    private var mostRecentHostID: UUID? {
        store.hosts
            .compactMap { host in host.lastConnected.map { (host.id, $0) } }
            .max { $0.1 < $1.1 }?.0
    }

    // MARK: - Chrome

    private var emptyState: some View {
        ContentUnavailableView {
            Label("No Hosts", systemImage: "rectangle.connected.to.line.below")
        } description: {
            Text("Add your punktfunk host with the + button, or scan the network again.")
        } actions: {
            Button("Add Host") { showAddHost = true }
                .glassProminentButtonStyle()
                #if os(iOS)
                .controlSize(.large)
                #endif
            // The screen a host SHOULD have appeared on is where a rescan is worth offering
            // outright rather than hiding behind a pull gesture.
            Button("Scan Again") { discovery.refresh() }
                .disabled(discovery.isScanning)
                #if os(iOS)
                .controlSize(.large)
                #endif
            #if os(tvOS)
            Button("Settings") { showSettings = true }
            #endif
        }
    }

    private var addHostButton: some View {
        Button {
            showAddHost = true
        } label: {
            Label("Add Host", systemImage: "plus")
        }
    }

    /// Re-run mDNS discovery from scratch. Discovery heals itself now (`HostDiscovery`'s sweep),
    /// so this is the fallback the field asked for — and the fastest way past the iOS
    /// local-network permission gate, which only a NEW browser can clear.
    private var refreshButton: some View {
        Button {
            discovery.refresh()
        } label: {
            Label("Refresh", systemImage: "arrow.clockwise")
        }
        .disabled(discovery.isScanning)
    }

    #if !os(tvOS)
    /// One host has no order and nothing to divide, so the control stays out of the way until
    /// there is a list to arrange.
    private var showsArrangeMenu: Bool { store.hosts.count > 1 }

    /// Sort and group, as two inline pickers in one menu — both are about the same list, and
    /// splitting them across two toolbar items would say otherwise.
    private var arrangeMenu: some View {
        Menu {
            Picker("Sort By", selection: $sortRaw) {
                ForEach(HostSort.allCases) { option in
                    Label(option.label, systemImage: option.symbol).tag(option.rawValue)
                }
            }
            .pickerStyle(.inline)
            Divider()
            Picker("Group By", selection: $groupingRaw) {
                ForEach(HostGrouping.allCases) { option in
                    Label(option.label, systemImage: option.symbol).tag(option.rawValue)
                }
            }
            .pickerStyle(.inline)
        } label: {
            Label("Sort and Group", systemImage: "line.3.horizontal.decrease.circle")
        }
        #if os(macOS)
        // A Menu draws its label in the ACCENT colour where a Button draws it in the label
        // colour, so next to Add Host and Settings this one came out brand-purple. macOS only:
        // on iOS every toolbar item is accent-tinted, and pinning this one to primary would make
        // it the odd one out there instead.
        .tint(.primary)
        #endif
    }
    #endif

    #if !os(macOS)
    private var settingsButton: some View {
        Button {
            showSettings = true
        } label: {
            Label("Settings", systemImage: "gearshape")
        }
    }
    #endif

    /// macOS caps card width (a huge window shouldn't yield huge cards); on iOS the columns FILL
    /// the width so the cards stay edge-aligned with the title and bars — sized touch-first: one
    /// column on iPhone portrait, 3–4 generous cards on iPad.
    private var gridColumns: [GridItem] {
        // Wider than before: the monogram card is a horizontal module (tile + address line), so
        // it needs room for a monospaced "IP:port" without truncating.
        #if os(macOS)
        [GridItem(.adaptive(minimum: 250, maximum: 320), spacing: 16)]
        #elseif os(tvOS)
        // Tracks CardMetrics' 10-foot sizes — at the 30pt name a 320pt column truncates
        // every hostname longer than ~10 characters.
        [GridItem(.adaptive(minimum: 460), spacing: 48)]
        #else
        [GridItem(.adaptive(minimum: 280), spacing: 16)]
        #endif
    }

    private var gridSpacing: CGFloat {
        #if os(tvOS)
        48 // the focused card scales up — give it room instead of overlapping siblings
        #else
        16
        #endif
    }
}
