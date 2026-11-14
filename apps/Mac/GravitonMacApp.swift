import GravitonKit
import SwiftUI

@main
struct GravitonMacApp: App {
    @State private var store: EngineStore
    @State private var shells = ShellSessions()
    @State private var router = AppRouter()
    @AppStorage("showsMenuBarExtra") private var showsMenuBarExtra = true

    init() {
        let store = EngineStore(useDaemon: true)
        _store = State(initialValue: store)
        // Poll for the app's whole life, not just while a window is open: the menu bar item shows it too.
        Task { await store.run() }
    }

    var body: some Scene {
        WindowGroup("Graviton", id: "main") {
            RootView()
                .environment(store)
                .environment(shells)
                .environment(router)
                .preferredColorScheme(.dark)
                .tint(G.accent)
                .frame(minWidth: 980, minHeight: 620)
        }
        .defaultSize(width: 1440, height: 900)
        .windowToolbarStyle(.unified(showsTitle: true))
        .commands {
            CommandGroup(after: .toolbar) {
                Button("Refresh") { Task { await store.refresh() } }
                    .keyboardShortcut("r")
                Toggle("Show in Menu Bar", isOn: $showsMenuBarExtra)
            }
        }

        MenuBarExtra(isInserted: $showsMenuBarExtra) {
            MenuBarPanel()
                .environment(store)
                .environment(router)
        } label: {
            MenuBarLabel()
                .environment(store)
        }
        .menuBarExtraStyle(.window)
    }
}

enum Section: String, Hashable, CaseIterable, Identifiable {
    case containers = "Containers", images = "Images", volumes = "Volumes", networks = "Networks"
    var id: String { rawValue }

    var symbol: String {
        switch self {
        case .containers: "shippingbox"
        case .images: "square.stack.3d.up"
        case .volumes: "cylinder.split.1x2"
        case .networks: "point.3.connected.trianglepath.dotted"
        }
    }
}

struct RootView: View {
    @Environment(EngineStore.self) private var store
    @Environment(AppRouter.self) private var router
    @State private var section: Section? = .containers

    var body: some View {
        NavigationSplitView {
            Sidebar(selection: $section)
                .navigationSplitViewColumnWidth(min: 210, ideal: 232, max: 280)
        } detail: {
            Group {
                switch section ?? .containers {
                case .containers: ContainersScreen()
                case .images: ImagesScreen()
                case .volumes:
                    ComingSoon(title: "Volumes", symbol: Section.volumes.symbol,
                               detail: "Named volumes and bind mounts arrive with milestone 3.")
                case .networks:
                    ComingSoon(title: "Networks", symbol: Section.networks.symbol,
                               detail: "NAT networks and port forwarding arrive with milestone 3.")
                }
            }
            .background(G.background)
        }
        .onChange(of: router.pending, initial: true) { _, request in
            switch request {
            case .images:
                section = .images
                router.pending = nil
            case .runContainer, .container:
                section = .containers // ContainersScreen handles the rest.
            case nil:
                break
            }
        }
        .alert("Something went wrong", isPresented: Binding(
            get: { store.lastError != nil },
            set: { if !$0 { store.lastError = nil } }
        )) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(store.lastError ?? "")
        }
    }
}

struct Sidebar: View {
    @Environment(EngineStore.self) private var store
    @Binding var selection: Section?

    var body: some View {
        List(selection: $selection) {
            HStack(spacing: 10) {
                GravitonMark(size: 24)
                Text("Graviton").font(.system(size: 14, weight: .semibold))
            }
            .padding(.vertical, 6)
            .selectionDisabled()

            SwiftUI.Section("Workspace") {
                ForEach(Section.allCases) { item in
                    Label(item.rawValue, systemImage: item.symbol)
                        .badge(count(for: item))
                        .tag(item)
                }
            }
        }
        .scrollContentBackground(.hidden)
        .background(G.sidebar)
        .safeAreaInset(edge: .bottom) {
            EngineCard().padding(12)
        }
    }

    private func count(for item: Section) -> Int {
        switch item {
        case .containers: store.containers.count
        case .images: store.images.count
        case .volumes, .networks: 0
        }
    }
}

struct EngineCard: View {
    @Environment(EngineStore.self) private var store

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                switch store.connection {
                case .live:
                    StatusDot(running: true)
                    Text("Engine running").fontWeight(.medium)
                case .demo:
                    Circle().fill(G.warning).frame(width: 8, height: 8)
                    Text("Demo data").fontWeight(.medium)
                case .connecting:
                    ProgressView().controlSize(.mini)
                    Text("Connecting…").fontWeight(.medium)
                case .unreachable:
                    Circle().fill(G.danger).frame(width: 8, height: 8)
                    Text("Can't reach gravitond").fontWeight(.medium)
                }
            }
            if let e = store.engine, e.memoryBytes > 0 {
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        Text("Memory")
                        Spacer()
                        Text("\(Format.bytes(e.memoryUsedBytes)) / \(Format.bytes(e.memoryBytes))").font(G.mono(11))
                    }
                    .font(.system(size: 12))
                    .foregroundStyle(G.muted)
                    MeterBar(Double(e.memoryUsedBytes) / Double(e.memoryBytes))
                }
                Text("\(e.kernel) · \(e.arch) · \(e.cpus) vCPU")
                    .font(.system(size: 11.5))
                    .foregroundStyle(G.faint)
            }
            if store.connection == .demo {
                Text("Start `gravitond` to connect.")
                    .font(.system(size: 11.5))
                    .foregroundStyle(G.faint)
            }
        }
        .font(.system(size: 13))
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(Color(hex: 0x15161A))
    }
}

struct ComingSoon: View {
    var title: String
    var symbol: String
    var detail: String

    var body: some View {
        ContentUnavailableView(title, systemImage: symbol, description: Text(detail))
            .navigationTitle(title)
    }
}
