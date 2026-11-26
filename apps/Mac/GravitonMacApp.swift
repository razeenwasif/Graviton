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
    @State private var updates: UpdateChecker
    @AppStorage("showsMenuBarExtra") private var showsMenuBarExtra = true

    init() {
        // GRAVITON_DEMO=1 shows demo data and leaves the engine and paired Macs alone (for screenshots).
        let demo = ProcessInfo.processInfo.environment["GRAVITON_DEMO"] == "1"
        let store = EngineStore(useDaemon: !demo)
        let hosts = SharedHosts()
        let shells = ShellSessions()
        let router = AppRouter()
        let updates = UpdateChecker()
        if !demo, let mac = hosts.current {
            store.use(RemoteEngine(host: mac.remoteHost, token: mac.token), named: mac.name)
        }
        _store = State(initialValue: store)
        _hosts = State(initialValue: hosts)
        _shells = State(initialValue: shells)
        _router = State(initialValue: router)
        _fairy = State(initialValue: FairyController(store: store, hosts: hosts, shells: shells, router: router))
        _updates = State(initialValue: updates)
        Task { await store.run() }
        Task {
            try? await Task.sleep(for: .seconds(4))
            await updates.checkIfDue()
        }
    }

    @MainActor
    private func handleWidgetURL(_ url: URL) {
        guard url.scheme?.lowercased() == "graviton" else { return }
        let host = url.host?.lowercased() ?? ""
        let segments = url.pathComponents.filter { $0 != "/" }
        switch host {
        case "container":
            if let id = segments.first, !id.isEmpty {
                router.pending = .container(id, .logs)
            } else {
                router.pending = .containers
            }
        case "images": router.pending = .images
        case "volumes": router.pending = .volumes
        case "networks": router.pending = .networks
        case "settings": router.pending = .settings
        default: router.pending = .containers
        }
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
                .environment(updates)
            .preferredColorScheme(.dark)
            .tint(G.indigo)
            .frame(minWidth: 980, minHeight: 620)
            .onOpenURL { url in
                handleWidgetURL(url)
            }
        }
        .defaultSize(width: 1440, height: 900)
        .windowStyle(.hiddenTitleBar)
        .commands {
            CommandGroup(after: .appInfo) {
                Button("Check for Updates…") {
                    router.pending = .settings
                    Task { await updates.checkNow(userInitiated: true) }
                }
            }
            CommandGroup(after: .toolbar) {
                Button("Command Palette…") {
                    withAnimation(.easeOut(duration: 0.14)) {
                        router.isCommandPaletteOpen.toggle()
                    }
                }
                .keyboardShortcut("k", modifiers: .command)

                Button(router.isSidebarCollapsed ? "Show Sidebar" : "Hide Sidebar") {
                    withAnimation(.snappy(duration: 0.2)) {
                        router.isSidebarCollapsed.toggle()
                    }
                }
                .keyboardShortcut("\\", modifiers: .command)

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
                .environment(updates)
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
        case .volumes: "internaldrive"
        case .networks: "network"
        case .settings: "gearshape"
        }
    }

    var accent: Color {
        switch self {
        case .containers: G.indigo
        case .images: G.sky
        case .volumes: G.amber
        case .networks: G.mint
        case .settings: G.slate
        }
    }

    var subtitle: String {
        switch self {
        case .containers: "Micro-VM containers, Compose stacks, logs & terminals"
        case .images: "OCI images, builds & layer cache"
        case .volumes: "Persistent named volumes & host bind mounts"
        case .networks: "Switch networks, embedded DNS & published ports"
        case .settings: "Registries, hosts, updates & daemon bridge"
        }
    }

    var shortcutLabel: String? {
        guard let idx = Self.allCases.firstIndex(of: self) else { return nil }
        return "⌘\(idx + 1)"
    }
}

struct RootView: View {
    @Environment(EngineStore.self) private var store
    @Environment(AppRouter.self) private var router
    @Environment(Dialogs.self) private var dialogs
    @Environment(FairyController.self) private var fairy
    @Environment(SharedHosts.self) private var hosts
    @Environment(ShellSessions.self) private var shells

    @State private var section: Section = .containers
    @State private var pairing = false
    @State private var paletteRunImage: String?
    @State private var paletteBuild = false
    @State private var paletteCleanUp = false
    @State private var palettePull = false
    @State private var toast: (text: String, isError: Bool)?

    var body: some View {
        @Bindable var bindableRouter = router
        ZStack(alignment: .bottomTrailing) {
            HStack(spacing: 0) {
                if !router.isSidebarCollapsed {
                    Sidebar(selection: $section)
                        .frame(width: 232)
                        .fixedSize(horizontal: true, vertical: false)
                        .transition(.move(edge: .leading).combined(with: .opacity))
                }

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
                .background(G.bg)
                .overlay(alignment: .topLeading) {
                    if router.isSidebarCollapsed {
                        Button {
                            withAnimation(.snappy(duration: 0.2)) {
                                router.isSidebarCollapsed = false
                            }
                        } label: {
                            Image(systemName: "sidebar.left")
                                .font(.system(size: 13, weight: .medium))
                                .foregroundStyle(G.textSecondary)
                                .frame(width: 28, height: 28)
                                .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
                        }
                        .buttonStyle(.plain)
                        .padding(.top, 18)
                        .padding(.leading, 82)
                        .help("Show Sidebar (⌘\\)")
                    }
                }

                if fairy.isOpen {
                    FairyPanel()
                        .frame(width: 380)
                        .transition(.move(edge: .trailing))
                }
            }
            .animation(.snappy(duration: 0.22), value: fairy.isOpen)
            .animation(.snappy(duration: 0.2), value: router.isSidebarCollapsed)
            .background(WindowAccessor())
            .background(G.bg)

            if let toast {
                GToastBanner(message: toast.text, isError: toast.isError) {
                    withAnimation(.easeOut(duration: 0.15)) { self.toast = nil }
                }
                .padding(22)
                .transition(.move(edge: .bottom).combined(with: .opacity))
            }

            if router.isCommandPaletteOpen {
                CommandPaletteView(
                    isPresented: $bindableRouter.isCommandPaletteOpen,
                    selection: $section,
                    onOpenContainer: { id, tab in
                        router.pending = .container(id, tab)
                    },
                    onRunContainer: { img in
                        paletteRunImage = img
                    },
                    onPullImage: { palettePull = true },
                    onBuildImage: { paletteBuild = true },
                    onCleanUp: { paletteCleanUp = true },
                    onPairMac: { pairing = true },
                    onToggleSidebar: {
                        withAnimation(.snappy(duration: 0.2)) { router.isSidebarCollapsed.toggle() }
                    },
                    onToast: { text, isError in showToast(text, isError: isError) }
                )
                .transition(.opacity)
            }
        }
        .background(G.bg)
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
                section = .containers
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
        .sheet(item: Binding(
            get: { paletteRunImage.map { PaletteImageSelection(ref: $0) } },
            set: { paletteRunImage = $0?.ref }
        )) { sel in
            RunContainerSheet(initialImage: sel.ref) { c in
                section = .containers
                router.pending = .container(c.id, .logs)
            }
        }
        .sheet(isPresented: $palettePull) {
            PullImageSheet()
        }
        .sheet(isPresented: $paletteBuild) {
            BuildImageSheet()
        }
        .sheet(isPresented: $paletteCleanUp) {
            CleanUpSheet()
        }
        .onChange(of: store.lastError, initial: true) { _, error in
            guard let error else { return }
            dialogs.alert("Something went wrong", message: error)
            store.lastError = nil
        }
    }

    private func showToast(_ text: String, isError: Bool) {
        withAnimation(.snappy(duration: 0.2)) {
            toast = (text, isError)
        }
        Task {
            try? await Task.sleep(for: .seconds(3.2))
            await MainActor.run {
                if toast?.text == text {
                    withAnimation(.easeOut(duration: 0.2)) { toast = nil }
                }
            }
        }
    }
}

private struct PaletteImageSelection: Identifiable {
    let id = UUID()
    let ref: String
}

struct Sidebar: View {
    @Environment(EngineStore.self) private var store
    @Environment(AppRouter.self) private var router
    @Environment(FairyController.self) private var fairy
    @Environment(UpdateChecker.self) private var updates
    @Binding var selection: Section
    @State private var searchHover = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Room for the window's traffic lights above the sidebar.
            Color.clear.frame(height: 38).windowDraggable()

            HStack(spacing: 10) {
                GravitonMark(size: 24)
                VStack(alignment: .leading, spacing: 0) {
                    Text("Graviton").font(.system(size: 13.5, weight: .semibold)).foregroundStyle(G.textPrimary)
                    Text("Containers on your Macs").font(.system(size: 10.5)).foregroundStyle(G.textMuted)
                }
                Spacer(minLength: 4)
                Button {
                    withAnimation(.snappy(duration: 0.2)) {
                        router.isSidebarCollapsed = true
                    }
                } label: {
                    Image(systemName: "sidebar.left")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(G.textMuted)
                        .frame(width: 24, height: 24)
                }
                .buttonStyle(.plain)
                .help("Hide Sidebar (⌘\\)")
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 14)
            .windowDraggable()

            // Stratus-style Host Switcher pinned near top of rail.
            HostMenu()
                .padding(.horizontal, 12)
                .padding(.bottom, 10)

            // ⌘K Command Palette quick trigger
            Button {
                withAnimation(.easeOut(duration: 0.14)) {
                    router.isCommandPaletteOpen = true
                }
            } label: {
                HStack(spacing: 7) {
                    Image(systemName: "magnifyingglass")
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(G.textMuted)
                    Text("Quick jump…")
                        .font(.system(size: 12))
                        .foregroundStyle(G.textTertiary)
                    Spacer()
                    Text("⌘K")
                        .font(G.mono(10))
                        .foregroundStyle(G.textMuted)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 2)
                        .background(G.surfacePill, in: RoundedRectangle(cornerRadius: 4, style: .continuous))
                }
                .padding(.horizontal, 10)
                .frame(height: 30)
                .background(searchHover ? G.surfaceHover : G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .onHover { searchHover = $0 }
            .padding(.horizontal, 12)
            .padding(.bottom, 16)
            .help("Open Command Palette (⌘K)")

            GCaption("Workspace").padding(.horizontal, 18).padding(.bottom, 7)
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
            if let release = updates.visibleUpdate {
                Button {
                    selection = .settings
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: "arrow.down.circle.fill")
                            .font(.system(size: 12.5, weight: .semibold))
                            .foregroundStyle(G.indigo)
                        Text("Update v\(release.version.description)")
                            .font(.system(size: 12, weight: .medium))
                            .foregroundStyle(G.textPrimary)
                            .lineLimit(1)
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 8)
                    .background(G.indigo.opacity(0.14), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                }
                .buttonStyle(.plain)
                .padding(.horizontal, 12)
                .padding(.bottom, 8)
                .help("Open Settings to review Graviton v\(release.version.description)")
            }
            FairySidebarButton().padding(.horizontal, 12).padding(.bottom, 8)
            EngineCard().padding(.horizontal, 12).padding(.bottom, 12)
        }
        .frame(maxHeight: .infinity, alignment: .top)
        .background(G.sidebar)
    }

    private func count(for item: Section) -> Int? {
        switch item {
        case .containers: store.containers.count
        case .images: store.images.count
        case .volumes: store.volumes.count
        case .networks: store.networks.count
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
                    .foregroundStyle(selected ? item.accent : (hovering ? G.body : G.textMuted))
                    .frame(width: 18)
                Text(item.rawValue)
                    .font(.system(size: 13, weight: selected ? .semibold : .medium))
                    .foregroundStyle(selected ? G.textPrimary : (hovering ? G.textPrimary : G.textSecondary))
                Spacer()
                if selected {
                    Circle()
                        .fill(item.accent)
                        .frame(width: 5, height: 5)
                } else if let count, count > 0 {
                    Text("\(count)")
                        .font(G.mono(10.5))
                        .foregroundStyle(G.textMuted)
                }
            }
            .padding(.horizontal, 10)
            .frame(height: 32)
            .background {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(selected ? G.surfaceActive : (hovering ? G.surfaceHover : .clear))
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
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                switch store.connection {
                case .live:
                    StatusDot(running: true, size: 7)
                    Text(store.isLocal ? "Engine running" : "Connected")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(G.textPrimary)
                case .demo:
                    Circle().fill(G.amber).frame(width: 7, height: 7)
                    Text("Demo data")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(G.amber)
                case .connecting:
                    GSpinner(size: 10)
                    Text("Connecting…")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(G.textSecondary)
                case .unreachable:
                    Circle().fill(G.rose).frame(width: 7, height: 7)
                    Text(store.isLocal ? "gravitond offline" : "\(store.liveName ?? "Host") offline")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(G.rose)
                }
                Spacer()
                if let e = store.engine {
                    Text("\(e.cpus)c")
                        .font(G.mono(10.5))
                        .foregroundStyle(G.textMuted)
                }
            }
            if let e = store.engine, e.memoryBytes > 0 {
                let hostColor: Color = e.hostMemoryFraction >= 0.93 ? G.rose
                    : e.hostMemoryFraction >= 0.85 ? G.amber
                    : G.slate
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        Text(e.hasHostMemory ? "Memory" : "VM memory")
                        Spacer()
                        Text("\(Format.bytes(e.totalMemoryUsedBytes)) / \(Format.bytes(e.memoryBytes))").font(G.mono(10.5))
                    }
                    .font(.system(size: 11))
                    .foregroundStyle(G.textMuted)
                    StackedMeterBar([
                        .init(e.vmMemoryFraction, color: G.indigo),
                        .init(e.otherHostMemoryFraction, color: hostColor),
                    ], height: 4)
                    if e.hasHostMemory {
                        HStack(spacing: 8) {
                            HStack(spacing: 4) {
                                Circle().fill(G.indigo).frame(width: 5.5, height: 5.5)
                                Text("VMs \(Format.bytes(e.vmMemoryUsedBytes))")
                                    .foregroundStyle(e.vmMemoryUsedBytes > 0 ? G.textSecondary : G.faint)
                            }
                            Spacer()
                            HStack(spacing: 4) {
                                Circle().fill(hostColor).frame(width: 5.5, height: 5.5)
                                Text("macOS \(Format.bytes(e.otherHostMemoryUsedBytes))")
                                    .foregroundStyle(G.faint)
                            }
                        }
                        .font(G.mono(10))
                    }
                }
                Text("\(e.kernel) · \(e.arch)")
                    .font(.system(size: 10.5))
                    .foregroundStyle(G.textTertiary)
                    .lineLimit(1)
            }
            if store.connection == .demo {
                Text("Start `gravitond` to connect.")
                    .font(.system(size: 11))
                    .foregroundStyle(G.textTertiary)
            }
        }
        .padding(13)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 11, style: .continuous))
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
                FairyEyeMark(size: 16, color: fairy.isOpen || fairy.pendingApproval != nil ? G.violet : G.textMuted)
                Text("Ask \(fairy.fairyName)")
                    .font(.system(size: 12.5, weight: fairy.isOpen ? .semibold : .medium))
                    .foregroundStyle(fairy.isOpen ? G.textPrimary : G.textSecondary)
                Spacer()
                if fairy.pendingApproval != nil {
                    Circle().fill(G.amber).frame(width: 6.5, height: 6.5)
                } else {
                    Text("⌥⌘I")
                        .font(G.mono(10))
                        .foregroundStyle(G.textMuted)
                }
            }
            .padding(.horizontal, 11)
            .frame(height: 34)
            .background(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .fill(fairy.isOpen ? G.violet.opacity(0.16) : (hovering ? G.surfaceHover : G.surfaceSubtle))
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help("Open or close the Fairy agent panel (⌥⌘I)")
    }
}
