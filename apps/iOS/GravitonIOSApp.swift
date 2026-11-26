import GravitonKit
import SwiftUI

@main
struct GravitonIOSApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @Environment(\.scenePhase) private var scenePhase
    @State private var push = PushManager.shared
    // The phone talks to a paired Mac's gravitond over TLS, or shows demo data.
    @State private var store = EngineStore(useDaemon: false)
    @State private var hosts = HostStore()
    @State private var browser = HostBrowser()
    @State private var shells = ShellSessions()
    @State private var pairTarget: PairingTarget?
    @State private var deepLinkContainerID: String?
    @State private var dialogs = Dialogs()
    @State private var tab: AppTab = .containers

    var body: some Scene {
        WindowGroup {
            // Every tab stays alive, so each keeps its place when you switch back.
            ZStack {
                ForEach(AppTab.allCases) { t in
                    screen(for: t)
                        .opacity(tab == t ? 1 : 0)
                        .allowsHitTesting(tab == t)
                        .accessibilityHidden(tab != t)
                }
            }
            .safeAreaInset(edge: .bottom, spacing: 0) { GravitonTabBar(selection: $tab) }
            .background(G.background.ignoresSafeArea())
            .gravitonDialogs(dialogs)
            .environment(dialogs)
            .sheet(item: $pairTarget) { PairSheet(target: $0) }
            .onOpenURL { url in
                // The QR code from `grav pair` holds a graviton://pair link.
                if let target = PairingTarget(link: url.absoluteString) {
                    tab = .hosts
                    pairTarget = target
                    return
                }
                guard url.scheme == "graviton" else { return }
                switch url.host {
                case "containers":
                    tab = .containers
                case "container":
                    tab = .containers
                    let id = url.path.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
                    if !id.isEmpty { deepLinkContainerID = id }
                case "images":
                    tab = .images
                case "resources":
                    tab = .resources
                case "activity":
                    tab = .activity
                case "hosts":
                    tab = .hosts
                default:
                    break
                }
            }
            .onChange(of: scenePhase) { _, now in
                if now == .active {
                    Task { await store.refresh() }
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
                    tab = .activity
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

    @ViewBuilder private func screen(for tab: AppTab) -> some View {
        switch tab {
        case .containers: ContainerListView(openHosts: { self.tab = .hosts }, deepLinkContainerID: $deepLinkContainerID)
        case .images: ImageListView()
        case .resources: MobileResourcesView()
        case .activity: ActivityView()
        case .hosts: HostsView(pairTarget: $pairTarget)
        }
    }
}

struct ImageListView: View {
    @Environment(EngineStore.self) private var store
    @Environment(Dialogs.self) private var dialogs
    @State private var pulling = false
    @State private var runImage: ImageInfo?
    @State private var search = ""

    private var shown: [ImageInfo] {
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        return q.isEmpty ? store.images : store.images.filter { "\($0.repository):\($0.tag)".lowercased().contains(q) }
    }

    private var totalSize: UInt64 { store.images.map(\.sizeBytes).reduce(0, +) }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    if !store.images.isEmpty {
                        MobileSearchField(prompt: "Search images", text: $search)
                    }
                    if store.images.isEmpty {
                        GEmptyState("No images yet", symbol: "square.stack.3d.up",
                                    detail: "Pull one onto \(store.liveName ?? "the Mac") from Docker Hub, GHCR or any OCI registry.",
                                    actionTitle: store.canManage ? "Pull an image" : nil) { pulling = true }
                            .frame(minHeight: 420)
                    } else if shown.isEmpty {
                        GEmptyState("Nothing matches", symbol: "magnifyingglass", detail: "No image's name contains “\(search)”.")
                            .frame(minHeight: 300)
                    } else {
                        VStack(spacing: 0) {
                            ForEach(Array(shown.enumerated()), id: \.element.id) { i, image in
                                if i > 0 { RowDivider(inset: 64) }
                                ImageRow(image: image, canRun: store.canManage, run: { runImage = image }, remove: { confirmRemove(image) })
                            }
                        }
                        .mobileCard()
                    }
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                MobileHeader("Images", subtitle: "\(store.images.count) on \(store.liveName ?? "the Mac") · \(Format.bytes(totalSize))", large: true) {
                    Button { pulling = true } label: { Label("Pull image", systemImage: "arrow.down") }
                        .buttonStyle(CircleIconStyle(prominent: true))
                        .disabled(!store.canManage)
                }
                .background(G.background)
            }
            .refreshable { await store.refresh() }
            .mobileScreen(pushed: false)
            .sheet(isPresented: $pulling) { MobilePullSheet() }
            .sheet(item: $runImage) { image in MobileRunSheet(image: "\(image.repository):\(image.tag)") }
        }
    }

    private func confirmRemove(_ image: ImageInfo) {
        dialogs.confirm("Remove \(image.repository):\(image.tag)?",
                        message: "Deletes the manifest and layers no other image or container uses.",
                        confirm: "Remove") {
            Task {
                do {
                    _ = try await store.removeImage(image)
                } catch {
                    dialogs.alert("Couldn't remove image", message: error.localizedDescription)
                }
            }
        }
    }
}

private struct ImageRow: View {
    var image: ImageInfo
    var canRun: Bool
    var run: () -> Void
    var remove: () -> Void

    var body: some View {
        Button(action: run) {
            HStack(spacing: 12) {
                Text(image.monogram)
                    .font(G.mono(13, weight: .medium))
                    .foregroundStyle(G.accent)
                    .frame(width: 38, height: 38)
                    .background(G.control, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                VStack(alignment: .leading, spacing: 2) {
                    Text(image.repository).font(.system(size: 15, weight: .medium)).foregroundStyle(G.text).lineLimit(1)
                    HStack(spacing: 6) {
                        Text(image.tag).font(G.mono(11.5)).foregroundStyle(G.muted)
                        Text("·").foregroundStyle(G.faint)
                        Text(Format.bytes(image.sizeBytes)).font(.system(size: 12)).foregroundStyle(G.faint)
                        Text("·").foregroundStyle(G.faint)
                        Text(image.architecture).font(G.mono(11)).foregroundStyle(G.faint)
                    }
                }
                Spacer()
                if image.architecture == "amd64" {
                    GBadge("Rosetta", color: G.accent)
                }
                if image.usedBy > 0 {
                    GBadge("\(image.usedBy) in use", color: G.running)
                }
                Image(systemName: "play.fill")
                    .font(.system(size: 11, weight: .bold))
                    .foregroundStyle(canRun ? G.accent : G.faint)
                    .frame(width: 30, height: 30)
                    .background(G.accent.opacity(canRun ? 0.13 : 0.05), in: Circle())
            }
            .padding(.horizontal, 14)
            .frame(minHeight: 62)
            .contentShape(Rectangle())
        }
        .buttonStyle(PressRowStyle())
        .disabled(!canRun)
        .contextMenu {
            Button("Run…", systemImage: "play.fill", action: run).disabled(!canRun)
            Button("Copy Reference", systemImage: "doc.on.doc") { UIPasteboard.general.string = image.reference }
            Button("Remove Image", systemImage: "trash", role: .destructive, action: remove)
                .disabled(!canRun || image.usedBy > 0)
        }
    }
}

/// Rows that dim a little while pressed.
struct PressRowStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(configuration.isPressed ? Color.white.opacity(0.04) : .clear)
    }
}
