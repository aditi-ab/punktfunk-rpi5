// The home screen: a grid of saved hosts + an "On this network" section of mDNS-discovered
// hosts, with the add/settings toolbar and the pairing / speed-test / add / settings
// navigation. The connect logic lives in ContentView (it reads the @AppStorage stream mode) and
// is passed in as closures.

import PunktfunkKit
import SwiftUI
#if os(tvOS)
import SwiftUINavigationTransitions
#endif

struct HomeView: View {
    @ObservedObject var store: HostStore
    @ObservedObject var model: SessionModel
    @ObservedObject var discovery: HostDiscovery
    @Binding var showAddHost: Bool
    @Binding var pairingTarget: StoredHost?
    @Binding var speedTestTarget: StoredHost?
    #if !os(macOS)
    @Binding var showSettings: Bool
    #endif
    let connect: (StoredHost) -> Void
    let connectDiscovered: (DiscoveredHost) -> Void
    /// Pairing succeeded (tvOS PairSheet route) — pin + connect (ContentView guards staleness).
    let onPaired: (StoredHost, Data) -> Void

    var body: some View {
        NavigationStack {
            Group {
                if store.hosts.isEmpty && discoveredUnsaved.isEmpty {
                    emptyState
                } else {
                    ScrollView {
                        if !store.hosts.isEmpty {
                            LazyVGrid(columns: gridColumns, spacing: gridSpacing) {
                                ForEach(store.hosts) { host in
                                    hostCard(host)
                                }
                            }
                            .padding()
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
                        }
                        .padding(.top, 24)
                        #endif
                    }
                }
            }
            .navigationTitle("Punktfunkempfänger")
            // Browse the LAN for advertised hosts only while the grid is up — not during a
            // session. The home appears/disappears as the stream swaps in and out.
            .onAppear { discovery.start() }
            .onDisappear { discovery.stop() }
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
            #endif
            #if !os(tvOS)
            .toolbar {
                #if os(iOS)
                // Adjacent trailing items share one glass pill (the system default).
                ToolbarItem(placement: .topBarTrailing) { settingsButton }
                ToolbarItem(placement: .topBarTrailing) { addHostButton }
                #else
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
        #if os(tvOS)
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
        #if os(iOS)
        .sheet(isPresented: $showSettings) {
            NavigationStack {
                SettingsView()
                    .navigationTitle("Settings")
                    .toolbar {
                        Button("Done") { showSettings = false }
                    }
            }
        }
        #endif
        #endif
        .alert(
            "Connection failed",
            isPresented: Binding(
                get: { model.errorMessage != nil },
                set: { if !$0 { model.errorMessage = nil } }
            )
        ) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(model.errorMessage ?? "")
        }
    }

    // MARK: - Cards

    private func hostCard(_ host: StoredHost) -> some View {
        HostCardView(
            host: host,
            isConnecting: model.phase == .connecting && model.activeHost?.id == host.id,
            isMostRecent: host.id == mostRecentHostID,
            isBusy: model.isBusy,
            onConnect: { connect(host) },
            onPair: { if !model.isBusy { pairingTarget = host } },
            onSpeedTest: { if !model.isBusy { speedTestTarget = host } },
            onForget: { store.forgetIdentity(host) },
            onRemove: { store.remove(host) })
    }

    private var discoveredSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("On this network", systemImage: "antenna.radiowaves.left.and.right")
                .font(.headline)
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
    }

    /// Discovered hosts not already saved (matched by address+port) — the saved grid shows the
    /// rest, so this section only surfaces genuinely-new hosts on the network.
    private var discoveredUnsaved: [DiscoveredHost] {
        discovery.hosts.filter { d in
            !store.hosts.contains { $0.address == d.host && $0.port == d.port }
        }
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
            Text("Add your punktfunk host with the + button.")
        } actions: {
            Button("Add Host") { showAddHost = true }
                .buttonStyle(.borderedProminent)
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
        #if os(macOS)
        [GridItem(.adaptive(minimum: 180, maximum: 240), spacing: 16)]
        #elseif os(tvOS)
        [GridItem(.adaptive(minimum: 320), spacing: 48)]
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
