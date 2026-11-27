import AppKit
import GravitonKit
import SwiftUI

struct ContainerRoute: Hashable {
    var id: String
    var tab: DetailTab = .logs
}

struct ContainersScreen: View {
    @Environment(EngineStore.self) private var store
    @Environment(AppRouter.self) private var router
    @State private var filter: ContainerFilter = .all
    @State private var search = ""
    @State private var selection: Container.ID?
    @State private var route: ContainerRoute?
    @State private var showRun = false
    @State private var commitContainer: Container?
    @State private var forkContainer: Container?
    @State private var width: CGFloat = 1200

    private var rows: [Container] { filter.apply(store.containers, search: search) }
    private var exposedPortsCount: Int {
        store.containers.filter(\.isRunning).reduce(0) { $0 + $1.ports.count }
    }

    var body: some View {
        ZStack {
            if let route {
                ContainerDetailScreen(containerID: route.id, initialTab: route.tab) { self.route = nil }
                    .id(route)
                    .transition(.move(edge: .trailing).combined(with: .opacity))
            } else {
                list.transition(.move(edge: .leading).combined(with: .opacity))
            }
        }
        .animation(.snappy(duration: 0.24), value: route)
        .sheet(isPresented: $showRun) {
            RunContainerSheet { container in
                filter = .all
                selection = container.id
            }
        }
        .sheet(item: $commitContainer) { container in
            CommitContainerSheet(container: container)
        }
        .sheet(item: $forkContainer) { container in
            ForkContainerSheet(container: container)
        }
        .onChange(of: router.pending, initial: true) { _, request in
            switch request {
            case .runContainer:
                route = nil
                showRun = true
            case let .container(id, tab):
                filter = .all
                selection = id
                route = ContainerRoute(id: id, tab: tab)
            case .containers, .images, .volumes, .networks, .settings, .pairMac, nil:
                return
            }
            router.pending = nil
        }
        .onAppear {
            if selection == nil { selection = store.containers.first(where: \.isRunning)?.id ?? store.containers.first?.id }
            router.selectedContainerID = selection
        }
        .onChange(of: selection) { _, newValue in
            router.selectedContainerID = newValue
        }
        .onChange(of: store.containers.map(\.id)) {
            if selection == nil { selection = store.containers.first(where: \.isRunning)?.id ?? store.containers.first?.id }
        }
    }

    private var headerSubtitle: String {
        var parts = ["\(store.runningCount) running"]
        if store.pausedCount > 0 { parts.append("\(store.pausedCount) paused") }
        parts.append("\(store.containers.count - store.activeCount) stopped")
        let base = parts.joined(separator: " · ")
        guard !store.stacks.isEmpty else { return base }
        return "\(base) · \(store.stacks.count) \(store.stacks.count == 1 ? "stack" : "stacks")"
    }

    private var list: some View {
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                ScreenHeader("Containers", subtitle: headerSubtitle) {
                    HeaderSearch(prompt: "Search containers", text: $search)
                    Button {
                        showRun = true
                    } label: {
                        Label("Run container", systemImage: "plus")
                    }
                    .buttonStyle(.gPrimary)
                    .keyboardShortcut("n")
                    .disabled(!store.canManage)
                    .help(store.canManage ? "Run a new container (⌘N)" : "Start gravitond to run containers")
                }

                VStack(alignment: .leading, spacing: 18) {
                    kpiStrip

                    HStack(spacing: 8) {
                        ForEach(ContainerFilter.allCases, id: \.self) { f in
                            GFilterPillButton(f.rawValue, count: count(for: f), selected: filter == f, accent: G.indigo) {
                                withAnimation(.snappy(duration: 0.18)) { filter = f }
                            }
                        }
                        Spacer()
                        if !search.isEmpty {
                            Text("\(rows.count) shown")
                                .font(G.mono(11.5))
                                .foregroundStyle(G.textMuted)
                        }
                    }

                    if store.containers.isEmpty {
                        GEmptyState("No containers yet", symbol: "shippingbox",
                                    detail: "Run one from an image: it boots in its own small VM in a second or two.",
                                    actionTitle: store.canManage ? "Run a container" : nil) { showRun = true }
                            .gravitonCard(G.surfaceSubtle)
                    } else if rows.isEmpty {
                        GEmptyState("Nothing matches", symbol: "magnifyingglass",
                                    detail: search.isEmpty ? "No \(filter.rawValue.lowercased()) containers." : "No container's name or image contains “\(search)”.")
                            .gravitonCard(G.surfaceSubtle)
                    } else {
                        ContainerTable(
                            rows: rows,
                            selection: $selection,
                            onFork: { forkContainer = $0 },
                            onCommit: { commitContainer = $0 },
                            openTab: { id, tab in route = ContainerRoute(id: id, tab: tab) }
                        )
                    }
                }
                .padding(.horizontal, 28)
                .padding(.top, 4)
                .padding(.bottom, 22)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(G.bg)

            if width >= 980, let selected = store.container(selection) {
                Inspector(container: selected) { tab in
                    route = ContainerRoute(id: selected.id, tab: tab)
                }
                .frame(width: 344)
                .transition(.move(edge: .trailing).combined(with: .opacity))
            }
        }
        .animation(.snappy(duration: 0.2), value: selection)
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width = $0 }
    }

    private var kpiStrip: some View {
        HStack(spacing: 36) {
            GMetricStatColumn(
                label: "Running",
                value: "\(store.runningCount)",
                subtitle: "of \(store.containers.count) containers",
                valueColor: store.runningCount > 0 ? G.mint : G.textPrimary
            )
            GMetricStatColumn(
                label: "CPU",
                value: Format.percent(store.totalCPU),
                subtitle: "aggregate load",
                valueColor: G.indigo
            )
            GMetricStatColumn(
                label: "Memory",
                value: Format.bytes(store.totalMemory),
                subtitle: store.engine.map { "of \(Format.bytes($0.memoryBytes))" } ?? "VM working set",
                valueColor: G.sky
            )
            GMetricStatColumn(
                label: store.stacks.isEmpty ? "Published Ports" : "Stacks",
                value: store.stacks.isEmpty ? "\(exposedPortsCount)" : "\(store.stacks.count)",
                subtitle: store.stacks.isEmpty ? "forwarded TCP/UDP" : "\(exposedPortsCount) exposed ports",
                valueColor: G.violet
            )
            Spacer()
        }
        .padding(.vertical, 14)
        .padding(.horizontal, 18)
        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }

    private func count(for filter: ContainerFilter) -> Int {
        switch filter {
        case .all: store.containers.count
        case .running: store.activeCount
        case .stopped: store.containers.count - store.activeCount
        }
    }
}

/// Borderless Stratus container table with Compose stacks, clickable ports, and hover quick-actions.
struct ContainerTable: View {
    @Environment(EngineStore.self) private var store
    var rows: [Container]
    @Binding var selection: Container.ID?
    var onFork: (Container) -> Void = { _ in }
    var onCommit: (Container) -> Void = { _ in }
    var openTab: (Container.ID, DetailTab) -> Void
    @State private var sort: Column = .name
    @State private var ascending = true
    @State private var collapsed: Set<String> = []
    @FocusState private var focused: Bool

    enum Column: Hashable { case name, ports, cpu, memory, uptime }

    private var sorted: [Container] {
        let out = rows.sorted { a, b in
            switch sort {
            case .name: a.name.localizedStandardCompare(b.name) == .orderedAscending
            case .ports: (a.ports.first?.hostPort ?? .max) < (b.ports.first?.hostPort ?? .max)
            case .cpu: (a.cpuPercent ?? -1) < (b.cpuPercent ?? -1)
            case .memory: (a.memoryBytes ?? 0) < (b.memoryBytes ?? 0)
            case .uptime: (a.startedAt ?? 0) > (b.startedAt ?? 0)
            }
        }
        return ascending ? out : out.reversed()
    }

    private var grouped: (stacks: [ContainerStack], standalone: [Container]) {
        ContainerStack.group(sorted)
    }

    private var visibleContainers: [Container] {
        let g = grouped
        guard !g.stacks.isEmpty else { return sorted }
        var out: [Container] = []
        for stack in g.stacks where !collapsed.contains(stack.name) {
            out.append(contentsOf: stack.containers)
        }
        out.append(contentsOf: g.standalone)
        return out
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                GColumnHeader("Container", key: Column.name, sort: $sort, ascending: $ascending).frame(minWidth: 140, maxWidth: .infinity)
                GColumnHeader("Ports", key: Column.ports, sort: $sort, ascending: $ascending).frame(width: 112)
                GColumnHeader("CPU", key: Column.cpu, sort: $sort, ascending: $ascending, alignment: .trailing).frame(width: 64)
                GColumnHeader("Memory", key: Column.memory, sort: $sort, ascending: $ascending, alignment: .trailing).frame(width: 78)
                GColumnHeader("Status", key: Column.uptime, sort: $sort, ascending: $ascending).frame(width: 96)
                Color.clear.frame(width: 108)
            }
            .padding(.horizontal, 12)
            .frame(height: 30)

            ScrollView {
                let g = grouped
                LazyVStack(spacing: 3) {
                    if g.stacks.isEmpty {
                        ForEach(sorted) { c in
                            ContainerRowView(
                                container: c,
                                indented: false,
                                selected: selection == c.id,
                                onSelect: { selection = c.id; focused = true },
                                onOpen: { tab in openTab(c.id, tab) },
                                onFork: { onFork(c) },
                                onCommit: { onCommit(c) }
                            )
                        }
                    } else {
                        ForEach(g.stacks) { stack in
                            StackHeaderRow(
                                stack: stack,
                                collapsed: collapsed.contains(stack.name),
                                onToggle: {
                                    withAnimation(.snappy(duration: 0.2)) {
                                        if collapsed.contains(stack.name) {
                                            collapsed.remove(stack.name)
                                        } else {
                                            collapsed.insert(stack.name)
                                        }
                                    }
                                }
                            )
                            if !collapsed.contains(stack.name) {
                                ForEach(stack.containers) { c in
                                    ContainerRowView(
                                        container: c,
                                        indented: true,
                                        selected: selection == c.id,
                                        onSelect: { selection = c.id; focused = true },
                                        onOpen: { tab in openTab(c.id, tab) },
                                        onFork: { onFork(c) },
                                        onCommit: { onCommit(c) }
                                    )
                                }
                            }
                        }
                        if !g.standalone.isEmpty {
                            HStack(spacing: 6) {
                                Text("STANDALONE")
                                    .font(.system(size: 10.5, weight: .semibold))
                                    .kerning(0.65)
                                    .foregroundStyle(G.textMuted)
                                Text("\(g.standalone.count)")
                                    .font(G.mono(10.5))
                                    .foregroundStyle(G.textTertiary)
                                Spacer()
                            }
                            .padding(.horizontal, 12)
                            .padding(.top, 12)
                            .padding(.bottom, 4)

                            ForEach(g.standalone) { c in
                                ContainerRowView(
                                    container: c,
                                    indented: false,
                                    selected: selection == c.id,
                                    onSelect: { selection = c.id; focused = true },
                                    onOpen: { tab in openTab(c.id, tab) },
                                    onFork: { onFork(c) },
                                    onCommit: { onCommit(c) }
                                )
                            }
                        }
                    }
                }
                .padding(.vertical, 4)
            }
        }
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        .onKeyPress(.downArrow) { move(1) }
        .onKeyPress(.upArrow) { move(-1) }
        .onKeyPress(.return) {
            guard let selection else { return .ignored }
            openTab(selection, .logs)
            return .handled
        }
    }

    private func move(_ by: Int) -> KeyPress.Result {
        let list = visibleContainers
        guard !list.isEmpty else { return .ignored }
        let index = list.firstIndex { $0.id == selection } ?? (by > 0 ? -1 : list.count)
        selection = list[min(max(index + by, 0), list.count - 1)].id
        return .handled
    }
}

private struct StackHeaderRow: View {
    @Environment(EngineStore.self) private var store
    var stack: ContainerStack
    var collapsed: Bool
    var onToggle: () -> Void
    @State private var hovering = false

    private var portCount: Int { stack.containers.reduce(0) { $0 + $1.ports.count } }

    var body: some View {
        HStack(spacing: 12) {
            HStack(spacing: 9) {
                Image(systemName: collapsed ? "chevron.right" : "chevron.down")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(G.textMuted)
                    .frame(width: 12)
                Image(systemName: "square.stack.3d.up.fill")
                    .font(.system(size: 12.5))
                    .foregroundStyle(stack.anyRunning ? G.indigo : G.textMuted)
                Text(stack.name)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(G.textPrimary)
                    .lineLimit(1)
                GTagPill("\(stack.runningCount)/\(stack.containers.count) running", color: stack.anyRunning ? G.mint : G.textMuted)
            }
            .frame(minWidth: 140, maxWidth: .infinity, alignment: .leading)

            Text(portCount > 0 ? "\(portCount) \(portCount == 1 ? "port" : "ports")" : "—")
                .font(G.mono(11.5))
                .foregroundStyle(G.textMuted)
                .lineLimit(1)
                .frame(width: 112, alignment: .leading)

            Text(stack.anyRunning ? Format.percent(stack.totalCPU) : "—")
                .font(G.mono(12))
                .foregroundStyle(G.textSecondary)
                .frame(width: 64, alignment: .trailing)

            Text(stack.totalMemory > 0 ? Format.bytes(stack.totalMemory) : "—")
                .font(G.mono(12))
                .foregroundStyle(G.textSecondary)
                .frame(width: 78, alignment: .trailing)

            Spacer().frame(width: 96)

            HStack(spacing: 4) {
                if hovering {
                    GActionPillButton(
                        stack.anyRunning ? "Stop" : "Start",
                        icon: stack.anyRunning ? "stop.fill" : "play.fill",
                        color: stack.anyRunning ? G.rose : G.mint
                    ) {
                        Task {
                            if stack.anyRunning {
                                await store.stopStack(stack.name)
                            } else {
                                await store.startStack(stack.name)
                            }
                        }
                    }
                }
                StackMenu(stack: stack)
            }
            .frame(width: 108, alignment: .trailing)
        }
        .padding(.horizontal, 12)
        .frame(height: 42)
        .background(hovering ? G.surfaceHover : G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .onTapGesture(perform: onToggle)
        .contextMenu { StackLifecycleButtons(stack: stack) }
    }
}

private struct ContainerRowView: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    var indented: Bool
    var selected: Bool
    var onSelect: () -> Void
    var onOpen: (DetailTab) -> Void
    var onFork: () -> Void
    var onCommit: () -> Void
    @State private var hovering = false

    private var webPorts: [PortMapping] { container.ports.filter { $0.protocol == .tcp } }

    var body: some View {
        HStack(spacing: 12) {
            HStack(spacing: 11) {
                StatusDot(status: container.status, size: 7.5, pulse: container.isRunning)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(container.name)
                            .font(.system(size: 13.5, weight: .semibold))
                            .foregroundStyle(G.textPrimary)
                            .lineLimit(1)
                        if indented, let svc = container.service, !svc.isEmpty, svc != container.name {
                            GTagPill(svc, color: G.indigo)
                        }
                        if let health = container.health, container.isRunning {
                            HealthBadge(health)
                        }
                    }
                    Text(container.image)
                        .font(G.mono(11.5))
                        .foregroundStyle(G.textMuted)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
            .padding(.leading, indented ? 18 : 0)
            .frame(minWidth: 140, maxWidth: .infinity, alignment: .leading)

            Group {
                if container.ports.isEmpty {
                    Text("—").font(G.mono(11.5)).foregroundStyle(G.textTertiary)
                } else if container.isRunning, let port = webPorts.first {
                    Button {
                        openPort(port)
                    } label: {
                        GTagPill(":\(port.hostPort)", color: G.sky, icon: "arrow.up.right")
                    }
                    .buttonStyle(.plain)
                    .help("Open http://localhost:\(port.hostPort) in browser")
                } else {
                    Text(container.ports.map(\.compactLabel).joined(separator: ", "))
                        .font(G.mono(11.5))
                        .foregroundStyle(G.textSecondary)
                        .lineLimit(1)
                }
            }
            .frame(width: 112, alignment: .leading)

            Text(Format.percent(container.cpuPercent))
                .font(G.mono(12))
                .foregroundStyle(container.isRunning ? G.textPrimary : G.textTertiary)
                .frame(width: 64, alignment: .trailing)

            Text(container.memoryBytes.map(Format.bytes) ?? "—")
                .font(G.mono(12))
                .foregroundStyle(container.isRunning ? G.textPrimary : G.textTertiary)
                .frame(width: 78, alignment: .trailing)

            Text(container.statusText())
                .font(.system(size: 12))
                .foregroundStyle(container.isPaused ? G.amber : (container.isRunning ? G.mint : G.textMuted))
                .lineLimit(1)
                .frame(width: 96, alignment: .leading)

            HStack(spacing: 3) {
                if hovering || selected {
                    if container.isRunning {
                        iconPill("terminal", help: "Open Terminal", tint: G.sky) { onOpen(.terminal) }
                        iconPill("doc.text", help: "View Logs", tint: G.indigo) { onOpen(.logs) }
                        iconPill("stop.fill", help: "Stop Container", tint: G.rose) { Task { await store.stop(container) } }
                    } else if container.isPaused {
                        iconPill("play.fill", help: "Resume", tint: G.mint) { Task { await store.unpause(container) } }
                        iconPill("stop.fill", help: "Stop", tint: G.rose) { Task { await store.stop(container) } }
                    } else {
                        iconPill("play.fill", help: "Start Container", tint: G.mint) { Task { await store.start(container) } }
                        iconPill("doc.text", help: "View Logs", tint: G.textSecondary) { onOpen(.logs) }
                    }
                }
                RowMenu(container: container, open: { onOpen(.logs) }, onFork: onFork, onCommit: onCommit)
            }
            .frame(width: 108, alignment: .trailing)
        }
        .padding(.horizontal, 12)
        .frame(height: 54)
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(selected ? G.surfaceActive : (hovering ? G.surfaceHover : .clear))
        )
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .onTapGesture(count: 2) { onOpen(.logs) }
        .simultaneousGesture(TapGesture().onEnded { onSelect() })
        .contextMenu {
            LifecycleButtons(container: container)
            Divider()
            Button("Fork…") { onFork() }
                .disabled(!store.canManage)
            Button("Commit to Image…") { onCommit() }
                .disabled(!store.canManage)
            Button("Show Logs") { onOpen(.logs) }
            Button("Inspect Filesystem Diff") { onOpen(.diff) }.disabled(!container.isRunning)
            Button("Open Terminal") { onOpen(.terminal) }.disabled(!container.isRunning)
        }
    }

    private func iconPill(_ symbol: String, help: String, tint: Color, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(tint)
                .frame(width: 24, height: 24)
                .background(tint.opacity(0.12), in: RoundedRectangle(cornerRadius: 6, style: .continuous))
        }
        .buttonStyle(.plain)
        .help(help)
    }

    private func openPort(_ port: PortMapping) {
        let host = switch port.hostIp {
        case nil, "127.0.0.1", "0.0.0.0", "::", "::1": "localhost"
        case let ip?: ip.contains(":") ? "[\(ip)]" : ip
        }
        if let url = URL(string: "http://\(host):\(port.hostPort)") { NSWorkspace.shared.open(url) }
    }
}

struct StackMenu: View {
    var stack: ContainerStack

    var body: some View {
        Menu {
            StackLifecycleButtons(stack: stack)
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 12, weight: .bold))
                .foregroundStyle(G.textMuted)
                .frame(width: 24, height: 24)
                .contentShape(Rectangle())
        }
        .gravitonMenu()
        .accessibilityLabel("More actions for stack \(stack.name)")
    }
}

struct StackLifecycleButtons: View {
    @Environment(EngineStore.self) private var store
    var stack: ContainerStack

    var body: some View {
        if !stack.allRunning {
            Button("Start All") { Task { await store.startStack(stack.name) } }
        }
        if stack.anyRunning {
            Button("Stop All") { Task { await store.stopStack(stack.name) } }
        }
        Button("Restart Stack") { Task { await store.restartStack(stack.name) } }
    }
}

struct RowMenu: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    var open: () -> Void
    var onFork: () -> Void
    var onCommit: () -> Void

    var body: some View {
        Menu {
            LifecycleButtons(container: container)
            Divider()
            Button("Fork…", action: onFork)
                .disabled(!store.canManage)
            Button("Commit to Image…", action: onCommit)
                .disabled(!store.canManage)
            Button("Show Logs", action: open)
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 12, weight: .bold))
                .foregroundStyle(G.textMuted)
                .frame(width: 24, height: 24)
                .contentShape(Rectangle())
        }
        .gravitonMenu()
        .accessibilityLabel("More actions for \(container.name)")
    }
}

struct LifecycleButtons: View {
    @Environment(EngineStore.self) private var store
    var container: Container

    var body: some View {
        if container.isRunning {
            Button("Pause") { Task { await store.pause(container) } }
            Button("Stop") { Task { await store.stop(container) } }
            Button("Restart") { Task { await store.restart(container) } }
        } else if container.isPaused {
            Button("Resume") { Task { await store.unpause(container) } }
            Button("Stop") { Task { await store.stop(container) } }
            Button("Restart") { Task { await store.restart(container) } }
        } else {
            Button("Start") { Task { await store.start(container) } }
            Button("Remove", role: .destructive) { Task { await store.remove(container) } }
        }
    }
}

struct Inspector: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    var openTab: (DetailTab) -> Void

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                HStack(alignment: .top, spacing: 10) {
                    VStack(alignment: .leading, spacing: 3) {
                        Text(container.name)
                            .font(.system(size: 16, weight: .semibold))
                            .foregroundStyle(G.textPrimary)
                        Text(container.image)
                            .font(G.mono(11.5))
                            .foregroundStyle(G.textMuted)
                            .lineLimit(2)
                    }
                    Spacer()
                    StatusPill(container)
                }

                HStack(spacing: 8) {
                    if container.isRunning {
                        Button("Stop") { Task { await store.stop(container) } }
                            .buttonStyle(.g(.danger, size: .small, fullWidth: true))
                        Button("Restart") { Task { await store.restart(container) } }
                            .buttonStyle(.g(.secondary, size: .small, fullWidth: true))
                        Button("Shell") { openTab(.terminal) }
                            .buttonStyle(.g(.primary, size: .small, fullWidth: true))
                    } else if container.isPaused {
                        Button("Resume") { Task { await store.unpause(container) } }
                            .buttonStyle(.g(.primary, size: .small, fullWidth: true))
                        Button("Stop") { Task { await store.stop(container) } }
                            .buttonStyle(.g(.danger, size: .small, fullWidth: true))
                    } else {
                        Button("Start") { Task { await store.start(container) } }
                            .buttonStyle(.g(.primary, size: .small, fullWidth: true))
                        Button("Remove") { Task { await store.remove(container) } }
                            .buttonStyle(.g(.danger, size: .small, fullWidth: true))
                    }
                    Button("Logs") { openTab(.logs) }
                        .buttonStyle(.g(.secondary, size: .small, fullWidth: true))
                }

                HStack(spacing: 10) {
                    MetricTile(
                        title: "CPU",
                        value: container.cpuPercent.map { String(format: "%.1f", $0) } ?? "—",
                        unit: container.cpuPercent == nil ? "" : "%",
                        history: store.cpuHistory[container.id] ?? [],
                        color: G.indigo
                    )
                    MetricTile(
                        title: "Memory",
                        value: container.memoryBytes.map { String(Format.bytes($0).split(separator: " ")[0]) } ?? "—",
                        unit: container.memoryBytes.map { " " + (Format.bytes($0).split(separator: " ").last.map(String.init) ?? "") } ?? "",
                        history: store.memoryHistory[container.id] ?? [],
                        color: G.sky
                    )
                }

                GKeyValueCard("Runtime") {
                    GKeyValueRow("ID", String(container.id.prefix(12)))
                    if let project = container.project {
                        GKeyValueRow("Stack", container.service.map { "\(project) / \($0)" } ?? project)
                    }
                    GKeyValueRow("IP Address", container.ipAddress ?? "—")
                    GKeyValueRow("Ports", container.ports.isEmpty ? "—" : container.ports.map(\.label).joined(separator: ", "))
                    GKeyValueRow("Mounts", container.mounts.isEmpty ? "—" : "\(container.mounts.count) attached")
                    GKeyValueRow("Created", Date(timeIntervalSince1970: Double(container.createdAt)).formatted(date: .abbreviated, time: .shortened), mono: false)
                }

                if let lines = store.logs[container.id], !lines.isEmpty {
                    VStack(alignment: .leading, spacing: 8) {
                        HStack {
                            GCaption("Recent Logs")
                            Spacer()
                            Button("Open full log") { openTab(.logs) }
                                .buttonStyle(.plain)
                                .font(.system(size: 11, weight: .medium))
                                .foregroundStyle(G.indigo)
                        }
                        VStack(alignment: .leading, spacing: 5) {
                            ForEach(lines.suffix(4)) { line in
                                HStack(spacing: 6) {
                                    Text(Format.clock(line.date).prefix(8)).foregroundStyle(G.textTertiary)
                                    if line.level >= .warn {
                                        Text(line.level.label).foregroundStyle(G.color(for: line.level))
                                    }
                                    Text(line.message).foregroundStyle(G.body).lineLimit(1)
                                }
                            }
                        }
                        .font(G.mono(11))
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(12)
                        .background(G.bg, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                    }
                }
            }
            .padding(20)
            .padding(.top, 8)
        }
        .frame(maxHeight: .infinity, alignment: .top)
        .background(G.inspector)
        .task(id: "\(container.id)|\(container.isActive)|\(store.connection)") {
            await store.followLogs(for: container.id, tail: 4)
        }
    }
}

struct MetricTile: View {
    var title: String
    var value: String
    var unit: String
    var history: [Double]
    var color: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title.uppercased())
                .font(.system(size: 10, weight: .semibold))
                .tracking(0.55)
                .foregroundStyle(G.textMuted)
            MetricValue(value, unit: unit, size: 19)
            Sparkline(history, color: color, filled: true).frame(height: 30)
        }
        .padding(13)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }
}

struct DetailRow: View {
    var label: String
    var value: String
    var mono: Bool

    init(_ label: String, _ value: String, mono: Bool = true) {
        self.label = label
        self.value = value
        self.mono = mono
    }

    var body: some View {
        GridRow(alignment: .firstTextBaseline) {
            Text(label).foregroundStyle(G.textMuted).gridColumnAlignment(.leading)
            Text(value).font(mono ? G.mono(12) : .system(size: 12.5)).foregroundStyle(G.body).textSelection(.enabled)
        }
    }
}
