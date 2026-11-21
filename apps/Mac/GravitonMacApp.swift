import GravitonKit
import SwiftUI

@main
struct GravitonMacApp: App {
    @State private var store: EngineStore
    @State private var shells = ShellSessions()
    @State private var hosts: SharedHosts
    @State private var router = AppRouter()
    @State private var dialogs = Dialogs()
    @AppStorage("showsMenuBarExtra") private var showsMenuBarExtra = true

    init() {
        // GRAVITON_DEMO=1 shows demo data and leaves the engine and paired Macs alone (for screenshots).
        let demo = ProcessInfo.processInfo.environment["GRAVITON_DEMO"] == "1"
        let store = EngineStore(useDaemon: !demo)
        let hosts = SharedHosts()
        // Show the Mac picked last time.
        if !demo, let mac = hosts.current {
            store.use(RemoteEngine(host: mac.remoteHost, token: mac.token), named: mac.name)
        }
        _store = State(initialValue: store)
        _hosts = State(initialValue: hosts)
        // Poll for the app's whole life, not just while a window is open: the menu bar item shows it too.
        Task { await store.run() }
    }

    var body: some Scene {
        WindowGroup("Graviton", id: "main") {
            RootView()
                .environment(store)
                .environment(shells)
                .environment(router)
                .environment(hosts)
                .environment(dialogs)
                .preferredColorScheme(.dark)
                .tint(G.accent)
                .frame(minWidth: 980, minHeight: 620)
        }
        .defaultSize(width: 1440, height: 900)
        .windowStyle(.hiddenTitleBar)
        .commands {
            CommandGroup(after: .toolbar) {
                Button("Refresh") { Task { await store.refresh() } }
                    .keyboardShortcut("r")
                Toggle("Show in Menu Bar", isOn: $showsMenuBarExtra)
            }
            HostCommands(store: store, hosts: hosts, shells: shells, router: router)
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
    @Environment(Dialogs.self) private var dialogs
    @State private var section: Section = .containers
    @State private var pairing = false
    @Environment(SharedHosts.self) private var hosts
    @Environment(ShellSessions.self) private var shells

    var body: some View {
        HStack(spacing: 0) {
            Sidebar(selection: $section)
                .frame(width: 232)
                .fixedSize(horizontal: true, vertical: false)
            Rectangle().fill(G.border).frame(width: 1)
            Group {
                switch section {
                case .containers: ContainersScreen()
                case .images: ImagesScreen()
                case .volumes: VolumesScreen()
                case .networks: NetworksScreen()
                }
            }
            .frame(minWidth: 0, maxWidth: .infinity, maxHeight: .infinity)
            .clipped()
            .background(G.background)
        }
        .background(G.background)
        .ignoresSafeArea()
        .gravitonDialogs(dialogs)
        .onChange(of: router.pending, initial: true) { _, request in
            switch request {
            case .images:
                section = .images
                router.pending = nil
            case .runContainer, .container:
                section = .containers // ContainersScreen handles the rest.
            case .pairMac:
                pairing = true
                router.pending = nil
            case nil:
                break
            }
        }
        .sheet(isPresented: $pairing) {
            PairMacSheet { mac in switchHost(to: mac, store: store, hosts: hosts, shells: shells) }
        }
        .onChange(of: store.lastError, initial: true) { _, error in
            guard let error else { return }
            dialogs.alert("Something went wrong", message: error)
            store.lastError = nil
        }
    }
}

struct Sidebar: View {
    @Environment(EngineStore.self) private var store
    @Binding var selection: Section

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Room for the window's traffic lights, which sit over the sidebar.
            Color.clear.frame(height: 40).windowDraggable()

            HStack(spacing: 10) {
                GravitonMark(size: 26)
                VStack(alignment: .leading, spacing: 0) {
                    Text("Graviton").font(.system(size: 14, weight: .semibold)).foregroundStyle(G.text)
                    Text("Containers on your Macs").font(.system(size: 11)).foregroundStyle(G.faint)
                }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 22)
            .windowDraggable()

            GCaption("Workspace").padding(.horizontal, 18).padding(.bottom, 8)
            VStack(spacing: 2) {
                ForEach(Array(Section.allCases.enumerated()), id: \.element) { i, item in
                    SidebarItem(item: item, count: count(for: item), selected: selection == item) {
                        selection = item
                    }
                    .keyboardShortcut(KeyEquivalent(Character("\(i + 1)")), modifiers: .command)
                }
            }
            .padding(.horizontal, 10)

            Spacer(minLength: 12)
            EngineCard().padding(12)
        }
        .frame(maxHeight: .infinity, alignment: .top)
        .background(G.sidebar)
    }

    private func count(for item: Section) -> Int? {
        switch item {
        case .containers: store.containers.count
        case .images: store.images.count
        case .volumes: store.volumes.count
        case .networks: store.runningCount
        }
    }
}

private struct SidebarItem: View {
    var item: Section
    var count: Int?
    var selected: Bool
    var action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 10) {
                Image(systemName: item.symbol)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(selected ? G.accent : G.muted)
                    .frame(width: 18)
                Text(item.rawValue)
                    .font(.system(size: 13, weight: selected ? .semibold : .medium))
                    .foregroundStyle(selected ? G.text : G.textSecondary)
                Spacer()
                if let count, count > 0 {
                    GBadge("\(count)", color: selected ? G.accent : G.muted)
                }
            }
            .padding(.horizontal, 10)
            .frame(height: 32)
            .background {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(selected ? Color(hex: 0x1B1D24) : (hovering ? Color.white.opacity(0.035) : .clear))
                    .overlay {
                        if selected {
                            RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(Color.white.opacity(0.06))
                        }
                    }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hovering)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

struct EngineCard: View {
    @Environment(EngineStore.self) private var store

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HostMenu()
            HStack(spacing: 8) {
                switch store.connection {
                case .live:
                    StatusDot(running: true)
                    Text(store.isLocal ? "Engine running" : "Connected").fontWeight(.medium)
                case .demo:
                    Circle().fill(G.warning).frame(width: 8, height: 8)
                    Text("Demo data").fontWeight(.medium)
                case .connecting:
                    GSpinner(size: 10)
                    Text("Connecting…").fontWeight(.medium)
                case .unreachable:
                    Circle().fill(G.danger).frame(width: 8, height: 8)
                    Text(store.isLocal ? "Can't reach gravitond" : "Can't reach \(store.liveName ?? "it")").fontWeight(.medium)
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
