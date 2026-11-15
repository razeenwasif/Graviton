import GravitonKit
import SwiftUI

@main
struct GravitonIOSApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var push = PushManager.shared
    // The phone talks to a paired Mac's gravitond over TLS, or shows demo data.
    @State private var store = EngineStore(useDaemon: false)
    @State private var hosts = HostStore()
    @State private var browser = HostBrowser()
    @State private var shells = ShellSessions()
    @State private var pairTarget: PairingTarget?
    @State private var tab = 0

    var body: some Scene {
        WindowGroup {
            TabView(selection: $tab) {
                Tab("Containers", systemImage: "shippingbox", value: 0) { ContainerListView(openHosts: { tab = 3 }) }
                Tab("Images", systemImage: "square.stack.3d.up", value: 1) { ImageListView() }
                Tab("Activity", systemImage: "waveform.path.ecg", value: 2) { ActivityView() }
                Tab("Hosts", systemImage: "desktopcomputer", value: 3) {
                    HostsView(pairTarget: $pairTarget)
                }
            }
            .sheet(item: $pairTarget) { PairSheet(target: $0) }
            .onOpenURL { url in
                // The QR code from `grav pair` holds a graviton://pair link.
                if let target = PairingTarget(link: url.absoluteString) {
                    tab = 3
                    pairTarget = target
                }
            }
            .onAppear {
                if let engine = hosts.engineForSelection() {
                    store.use(engine, named: hosts.selected?.name)
                }
                // Alerts only make sense once a Mac is paired.
                if !hosts.hosts.isEmpty { push.start() }
            }
            .onChange(of: hosts.hosts.count) { _, count in
                if count > 0 { push.start() }
                push.register(with: hosts)
            }
            .onChange(of: push.openActivity) { _, open in
                if open {
                    tab = 2
                    push.openActivity = false
                }
            }
            .environment(store)
            .environment(hosts)
            .environment(browser)
            .environment(shells)
            .preferredColorScheme(.dark)
            .tint(G.accent)
            .task { await store.run() }
        }
    }
}

struct Placeholder: View {
    var title: String
    var symbol: String
    var detail: String

    var body: some View {
        NavigationStack {
            ContentUnavailableView(title, systemImage: symbol, description: Text(detail))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(G.background)
                .navigationTitle(title)
        }
    }
}

struct ImageListView: View {
    @Environment(EngineStore.self) private var store
    @State private var pulling = false
    @State private var runImage: ImageInfo?

    var body: some View {
        NavigationStack {
            List(store.images) { image in
                HStack(spacing: 12) {
                    Text(image.monogram)
                        .font(G.mono(13, weight: .medium))
                        .foregroundStyle(G.accent)
                        .frame(width: 36, height: 36)
                        .background(G.control, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                    VStack(alignment: .leading, spacing: 1) {
                        Text(image.repository).font(.system(size: 15, weight: .medium)).lineLimit(1)
                        Text(image.tag).font(G.mono(11.5)).foregroundStyle(G.muted)
                    }
                    Spacer()
                    Text(Format.bytes(image.sizeBytes)).font(G.mono(12)).foregroundStyle(G.muted)
                }
                .listRowBackground(G.card)
                .contentShape(Rectangle())
                .onTapGesture { if store.canManage { runImage = image } }
                .contextMenu {
                    Button("Run…", systemImage: "play.fill") { runImage = image }.disabled(!store.canManage)
                }
            }
            .scrollContentBackground(.hidden)
            .background(G.background)
            .navigationTitle("Images")
            .refreshable { await store.refresh() }
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Button { pulling = true } label: { Image(systemName: "arrow.down.circle") }
                        .disabled(!store.canManage)
                        .accessibilityLabel("Pull image")
                }
            }
            .sheet(isPresented: $pulling) { MobilePullSheet() }
            .sheet(item: $runImage) { image in MobileRunSheet(image: "\(image.repository):\(image.tag)") }
        }
    }
}
