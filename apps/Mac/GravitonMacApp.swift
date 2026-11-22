import GravitonKit
import SwiftUI

@main
struct GravitonMacApp: App {
    @State private var store: EngineStore
    @State private var shells = ShellSessions()
    @State private var hosts: SharedHosts
    @State private var router = AppRouter()
    @State private var dialogs = Dialogs()
    @State private var fairy: FairyController
    @AppStorage("showsMenuBarExtra") private var showsMenuBarExtra = true

    init() {
        // GRAVITON_DEMO=1 shows demo data and leaves the engine and paired Macs alone (for screenshots).
        let demo = ProcessInfo.processInfo.environment["GRAVITON_DEMO"] == "1"
        let store = EngineStore(useDaemon: !demo)
        let hosts = SharedHosts()
        let shells = ShellSessions()
        let router = AppRouter()
        // Show the Mac picked last time.
        if !demo, let mac = hosts.current {
            store.use(RemoteEngine(host: mac.remoteHost, token: mac.token), named: mac.name)
        }
        _store = State(initialValue: store)
        _hosts = State(initialValue: hosts)
        _shells = State(initialValue: shells)
        _router = State(initialValue: router)
        _fairy = State(initialValue: FairyController(store: store, hosts: hosts, shells: shells, router: router))
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
                .environment(fairy)
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
                Button(fairy.isOpen ? "Hide Fairy" : "Ask Fairy") {
                    withAnimation(.snappy(duration: 0.22)) { fairy.toggle() }
                }
                .keyboardShortcut("i", modifiers: [.command, .option])
                Toggle("Show in Menu Bar", isOn: $showsMenuBarExtra)
            }
            CommandGroup(replacing: .appSettings) {
                Button("Settings…") { router.pending = .settings }
                    .keyboardShortcut(",", modifiers: .command)
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
    case containers = "Containers", images = "Images", volumes = "Volumes", networks = "Networks", settings = "Settings"
    var id: String { rawValue }

    var symbol: String {
        switch self {
        case .containers: "shippingbox"
        case .images: "square.stack.3d.up"
        case .volumes: "cylinder.split.1x2"
        case .networks: "point.3.connected.trianglepath.dotted"
        case .settings: "gearshape"
        }
    }
}

struct RootView: View {
    @Environment(EngineStore.self) private var store
    @Environment(AppRouter.self) private var router
    @Environment(Dialogs.self) private var dialogs
    @Environment(FairyController.self) private var fairy
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
                case .settings: SettingsScreen()
                }
            }
            .frame(minWidth: 0, maxWidth: .infinity, maxHeight: .infinity)
            .clipped()
            .background(G.background)

            if fairy.isOpen {
                Rectangle().fill(G.border).frame(width: 1)
                FairyPanel()
                    .frame(width: 390)
                    .transition(.move(edge: .trailing))
            }
        }
        .animation(.snappy(duration: 0.22), value: fairy.isOpen)
        .background(G.background)
        .ignoresSafeArea()
        .gravitonDialogs(dialogs)
        .onChange(of: section, initial: true) { _, newValue in
            router.activeSection = newValue.rawValue
        }
        .onChange(of: router.pending, initial: true) { _, request in
            switch request {
            case .containers:
                section = .containers
                router.pending = nil
            case .images:
                section = .images
                router.pending = nil
            case .volumes:
                section = .volumes
                router.pending = nil
            case .networks:
                section = .networks
                router.pending = nil
            case .settings:
                section = .settings
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
    @Environment(FairyController.self) private var fairy
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
            FairySidebarButton().padding(.horizontal, 12).padding(.bottom, 8)
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
        case .settings: nil
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
                let hostColor: Color = e.hostMemoryFraction >= 0.93 ? G.danger
                    : e.hostMemoryFraction >= 0.85 ? G.warning
                    : Color(hex: 0x646E82)
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        Text(e.hasHostMemory ? "Memory" : "VM memory")
                        Spacer()
                        Text("\(Format.bytes(e.totalMemoryUsedBytes)) / \(Format.bytes(e.memoryBytes))").font(G.mono(11))
                    }
                    .font(.system(size: 12))
                    .foregroundStyle(G.muted)
                    StackedMeterBar([
                        .init(e.vmMemoryFraction, color: G.accent),
                        .init(e.otherHostMemoryFraction, color: hostColor),
                    ], height: 5)
                    if e.hasHostMemory {
                        HStack(spacing: 8) {
                            HStack(spacing: 4) {
                                Circle().fill(G.accent).frame(width: 6, height: 6)
                                Text("VMs \(Format.bytes(e.vmMemoryUsedBytes))")
                                    .foregroundStyle(e.vmMemoryUsedBytes > 0 ? G.textSecondary : G.faint)
                            }
                            Spacer()
                            HStack(spacing: 4) {
                                Circle().fill(hostColor).frame(width: 6, height: 6)
                                Text("macOS \(Format.bytes(e.otherHostMemoryUsedBytes))")
                                    .foregroundStyle(G.faint)
                            }
                        }
                        .font(G.mono(10))
                    }
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

private struct FairySidebarButton: View {
    @Environment(FairyController.self) private var fairy
    @State private var hovering = false

    var body: some View {
        Button {
            withAnimation(.snappy(duration: 0.22)) { fairy.toggle() }
        } label: {
            HStack(spacing: 9) {
                FairyEyeMark(size: 16, color: fairy.isOpen || fairy.pendingApproval != nil ? G.accent : G.muted)
                Text("Ask \(fairy.fairyName)")
                    .font(.system(size: 12.5, weight: fairy.isOpen ? .semibold : .medium))
                    .foregroundStyle(fairy.isOpen ? G.text : G.textSecondary)
                Spacer()
                if fairy.pendingApproval != nil {
                    Circle().fill(G.warning).frame(width: 7, height: 7)
                } else {
                    Text("⌥⌘I").font(G.mono(10.5)).foregroundStyle(G.faint)
                }
            }
            .padding(.horizontal, 11)
            .frame(height: 34)
            .background(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .fill(fairy.isOpen ? G.accent.opacity(0.14) : (hovering ? Color.white.opacity(0.04) : Color(hex: 0x15161A)))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .strokeBorder(fairy.isOpen ? G.accent.opacity(0.35) : G.border)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help("Open or close the Fairy agent panel (⌥⌘I)")
    }
}
