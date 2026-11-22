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
    @State private var width: CGFloat = 1200

    private var rows: [Container] { filter.apply(store.containers, search: search) }

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
        .animation(.snappy(duration: 0.25), value: route)
        .sheet(isPresented: $showRun) {
            RunContainerSheet { container in
                filter = .all
                selection = container.id
            }
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
            case .containers, .images, .volumes, .networks, .pairMac, nil:
                return
            }
            router.pending = nil
        }
        .onAppear {
            if selection == nil { selection = store.containers.first(where: \.isRunning)?.id }
            router.selectedContainerID = selection
        }
        .onChange(of: selection) { _, newValue in
            router.selectedContainerID = newValue
        }
        .onChange(of: store.containers.map(\.id)) {
            if selection == nil { selection = store.containers.first(where: \.isRunning)?.id }
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

            HStack(alignment: .top, spacing: 20) {
                VStack(alignment: .leading, spacing: 14) {
                    GSegmented(selection: $filter, options: ContainerFilter.allCases.map { ($0, label(for: $0)) })

                    if store.containers.isEmpty {
                        GEmptyState("No containers yet", symbol: "shippingbox",
                                    detail: "Run one from an image: it boots in its own small VM in a second or two.",
                                    actionTitle: store.canManage ? "Run a container" : nil) { showRun = true }
                            .gravitonCard()
                    } else if rows.isEmpty {
                        GEmptyState("Nothing matches", symbol: "magnifyingglass",
                                    detail: search.isEmpty ? "No \(filter.rawValue.lowercased()) containers." : "No container's name or image contains “\(search)”.")
                            .gravitonCard()
                    } else {
                        ContainerTable(rows: rows, selection: $selection) { id in
                            route = ContainerRoute(id: id)
                        }
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)

                if width >= 940, let selected = store.container(selection) {
                    Inspector(container: selected) { route = ContainerRoute(id: selected.id) }
                        .frame(width: 330)
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                }
            }
            .padding(.horizontal, 28)
            .padding(.top, 6)
            .padding(.bottom, 22)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .animation(.snappy(duration: 0.2), value: selection)
        }
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width = $0 }
    }

    private func label(for filter: ContainerFilter) -> String {
        switch filter {
        case .all: "All  \(store.containers.count)"
        case .running: "Running  \(store.activeCount)"
        case .stopped: "Stopped  \(store.containers.count - store.activeCount)"
        }
    }
}

/// The container list: Graviton's own table, with hover, selection, a menu
/// per row, arrow keys to move and Return to open.
struct ContainerTable: View {
    @Environment(EngineStore.self) private var store
    var rows: [Container]
    @Binding var selection: Container.ID?
    var open: (Container.ID) -> Void
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
                GColumnHeader("Name", key: Column.name, sort: $sort, ascending: $ascending).frame(minWidth: 120, maxWidth: .infinity)
                GColumnHeader("Ports", key: Column.ports, sort: $sort, ascending: $ascending).frame(width: 104)
                GColumnHeader("CPU", key: Column.cpu, sort: $sort, ascending: $ascending, alignment: .trailing).frame(width: 64)
                GColumnHeader("Memory", key: Column.memory, sort: $sort, ascending: $ascending, alignment: .trailing).frame(width: 80)
                GColumnHeader("Uptime", key: Column.uptime, sort: $sort, ascending: $ascending).frame(width: 92)
                Color.clear.frame(width: 28)
            }
            .padding(.horizontal, 14)
            .frame(height: 34)
            .overlay(alignment: .bottom) { Rectangle().fill(G.border).frame(height: 1) }

            ScrollView {
                let g = grouped
                LazyVStack(spacing: 2) {
                    if g.stacks.isEmpty {
                        ForEach(sorted) { c in
                            row(c, indented: false)
                        }
                    } else {
                        ForEach(g.stacks) { stack in
                            stackHeader(stack)
                            if !collapsed.contains(stack.name) {
                                ForEach(stack.containers) { c in
                                    row(c, indented: true)
                                }
                            }
                        }
                        if !g.standalone.isEmpty {
                            HStack(spacing: 6) {
                                Text("STANDALONE")
                                    .font(.system(size: 10.5, weight: .semibold))
                                    .kerning(0.6)
                                    .foregroundStyle(G.faint)
                                Text("\(g.standalone.count)")
                                    .font(G.mono(10.5))
                                    .foregroundStyle(G.faint)
                                Spacer()
                            }
                            .padding(.horizontal, 10)
                            .padding(.top, 10)
                            .padding(.bottom, 4)

                            ForEach(g.standalone) { c in
                                row(c, indented: false)
                            }
                        }
                    }
                }
                .padding(6)
            }
        }
        .gravitonCard(G.surface)
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        .onKeyPress(.downArrow) { move(1) }
        .onKeyPress(.upArrow) { move(-1) }
        .onKeyPress(.return) {
            guard let selection else { return .ignored }
            open(selection)
            return .handled
        }
    }

    private func stackHeader(_ stack: ContainerStack) -> some View {
        let isCollapsed = collapsed.contains(stack.name)
        let portCount = stack.containers.reduce(0) { $0 + $1.ports.count }
        return HStack(spacing: 12) {
            HStack(spacing: 8) {
                Image(systemName: isCollapsed ? "chevron.right" : "chevron.down")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(G.muted)
                    .frame(width: 12)
                Image(systemName: "square.stack.3d.up.fill")
                    .font(.system(size: 12.5))
                    .foregroundStyle(stack.anyRunning ? G.accent : G.muted)
                Text(stack.name)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(G.text)
                    .lineLimit(1)
                Text("\(stack.runningCount)/\(stack.containers.count)")
                    .font(G.mono(11, weight: .medium))
                    .foregroundStyle(stack.anyRunning ? G.running : G.muted)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background((stack.anyRunning ? G.running : G.muted).opacity(0.12), in: Capsule())
            }
            .frame(minWidth: 120, maxWidth: .infinity, alignment: .leading)

            Text(portCount > 0 ? "\(portCount) \(portCount == 1 ? "port" : "ports")" : "—")
                .font(G.mono(11.5)).foregroundStyle(G.muted).lineLimit(1)
                .frame(width: 104, alignment: .leading)

            Text(stack.anyRunning ? Format.percent(stack.totalCPU) : "—")
                .font(G.mono(12)).foregroundStyle(G.textSecondary)
                .frame(width: 64, alignment: .trailing)

            Text(stack.totalMemory > 0 ? Format.bytes(stack.totalMemory) : "—")
                .font(G.mono(12)).foregroundStyle(G.textSecondary)
                .frame(width: 80, alignment: .trailing)

            HStack(spacing: 6) {
                Button {
                    Task {
                        if stack.anyRunning {
                            await store.stopStack(stack.name)
                        } else {
                            await store.startStack(stack.name)
                        }
                    }
                } label: {
                    Label(stack.anyRunning ? "Stop" : "Start", systemImage: stack.anyRunning ? "stop.fill" : "play.fill")
                        .font(.system(size: 11.5, weight: .medium))
                }
                .buttonStyle(.g(stack.anyRunning ? .secondary : .primary, size: .small))
                .help(stack.anyRunning ? "Stop all running containers in \(stack.name)" : "Start all containers in \(stack.name)")
            }
            .frame(width: 92, alignment: .leading)

            StackMenu(stack: stack).frame(width: 28)
        }
        .padding(.horizontal, 8)
        .frame(height: 40)
        .background(G.raised.opacity(0.55), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .contentShape(Rectangle())
        .onTapGesture {
            withAnimation(.snappy(duration: 0.2)) {
                if isCollapsed { collapsed.remove(stack.name) } else { collapsed.insert(stack.name) }
            }
        }
        .contextMenu {
            StackLifecycleButtons(stack: stack)
        }
    }

    private func row(_ c: Container, indented: Bool) -> some View {
        HStack(spacing: 12) {
            HStack(spacing: 12) {
                StatusDot(status: c.status)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(c.name).font(.system(size: 13, weight: .medium)).foregroundStyle(G.text).lineLimit(1)
                        if indented, let svc = c.service, !svc.isEmpty, svc != c.name {
                            Text(svc)
                                .font(G.mono(10.5))
                                .foregroundStyle(G.muted)
                                .padding(.horizontal, 5)
                                .padding(.vertical, 1)
                                .background(G.raised, in: RoundedRectangle(cornerRadius: 4, style: .continuous))
                                .lineLimit(1)
                        }
                    }
                    Text(c.image).font(G.mono(11.5)).foregroundStyle(G.muted).lineLimit(1).truncationMode(.middle)
                }
            }
            .padding(.leading, indented ? 18 : 0)
            .frame(minWidth: 120, maxWidth: .infinity, alignment: .leading)
            Text(c.ports.isEmpty ? "—" : c.ports.map(\.compactLabel).joined(separator: ", "))
                .font(G.mono(12)).foregroundStyle(G.textSecondary).lineLimit(1)
                .frame(width: 104, alignment: .leading)
            Text(Format.percent(c.cpuPercent)).font(G.mono(12)).foregroundStyle(G.textSecondary)
                .frame(width: 64, alignment: .trailing)
            Text(c.memoryBytes.map(Format.bytes) ?? "—").font(G.mono(12)).foregroundStyle(G.textSecondary)
                .frame(width: 80, alignment: .trailing)
            Text(c.statusText()).font(.system(size: 12.5)).foregroundStyle(c.isPaused ? G.warning : G.muted).lineLimit(1)
                .frame(width: 92, alignment: .leading)
            RowMenu(container: c, open: { open(c.id) }).frame(width: 28)
        }
        .padding(.horizontal, 8)
        .frame(height: 50)
        .gravitonRow(selected: selection == c.id)
        .onTapGesture(count: 2) { open(c.id) }
        .simultaneousGesture(TapGesture().onEnded {
            selection = c.id
            focused = true
        })
        .contextMenu {
            LifecycleButtons(container: c)
            Divider()
            Button("Show Logs") { open(c.id) }
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

struct StackMenu: View {
    var stack: ContainerStack

    var body: some View {
        Menu {
            StackLifecycleButtons(stack: stack)
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 12, weight: .bold))
                .foregroundStyle(G.muted)
                .frame(width: 26, height: 26)
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
    var container: Container
    var open: () -> Void

    var body: some View {
        Menu {
            LifecycleButtons(container: container)
            Divider()
            Button("Show Logs", action: open)
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 12, weight: .bold))
                .foregroundStyle(G.muted)
                .frame(width: 26, height: 26)
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
    var openLogs: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(container.name).font(.system(size: 16, weight: .semibold))
                    Text(container.image).font(G.mono(12)).foregroundStyle(G.muted)
                }
                Spacer()
                StatusPill(container)
            }

            HStack(spacing: 8) {
                if container.isRunning {
                    Button("Pause") { Task { await store.pause(container) } }.buttonStyle(.g(.secondary, fullWidth: true))
                    Button("Stop") { Task { await store.stop(container) } }.buttonStyle(.g(.danger, fullWidth: true))
                    Button("Restart") { Task { await store.restart(container) } }.buttonStyle(.g(.secondary, fullWidth: true))
                } else if container.isPaused {
                    Button("Resume") { Task { await store.unpause(container) } }.buttonStyle(.g(.primary, fullWidth: true))
                    Button("Stop") { Task { await store.stop(container) } }.buttonStyle(.g(.danger, fullWidth: true))
                    Button("Restart") { Task { await store.restart(container) } }.buttonStyle(.g(.secondary, fullWidth: true))
                } else {
                    Button("Start") { Task { await store.start(container) } }.buttonStyle(.g(.primary, fullWidth: true))
                    Button("Remove") { Task { await store.remove(container) } }.buttonStyle(.g(.danger, fullWidth: true))
                }
                Button("Open", action: openLogs).buttonStyle(.g(.secondary, fullWidth: true))
            }

            HStack(spacing: 10) {
                MetricTile(title: "CPU", value: container.cpuPercent.map { String(format: "%.1f", $0) } ?? "—",
                           unit: container.cpuPercent == nil ? "" : "%", history: store.cpuHistory[container.id] ?? [], color: G.accent)
                MetricTile(title: "Memory", value: container.memoryBytes.map { String(Format.bytes($0).split(separator: " ")[0]) } ?? "—",
                           unit: container.memoryBytes.map { " " + (Format.bytes($0).split(separator: " ").last.map(String.init) ?? "") } ?? "",
                           history: store.memoryHistory[container.id] ?? [], color: G.running)
            }

            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 10) {
                DetailRow("ID", container.id)
                if let project = container.project {
                    DetailRow("Stack", container.service.map { "\(project) / \($0)" } ?? project)
                }
                DetailRow("Ports", container.ports.isEmpty ? "—" : container.ports.map(\.label).joined(separator: "\n"))
                DetailRow("IP address", container.ipAddress ?? "—")
                DetailRow("Mounts", container.mounts.isEmpty ? "—" : container.mounts.map { "\($0.source) → \($0.target)" }.joined(separator: "\n"))
                DetailRow("Created", Date(timeIntervalSince1970: Double(container.createdAt)).formatted(date: .abbreviated, time: .shortened), mono: false)
            }
            .font(.system(size: 12.5))

            if let lines = store.logs[container.id], !lines.isEmpty {
                Button(action: openLogs) {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(lines.suffix(3)) { line in
                            HStack(spacing: 6) {
                                Text(Format.clock(line.date).prefix(8)).foregroundStyle(G.faint)
                                if line.level >= .warn { Text(line.level.label).foregroundStyle(G.color(for: line.level)) }
                                Text(line.message).foregroundStyle(G.textSecondary).lineLimit(1)
                            }
                        }
                    }
                    .font(G.mono(11.5))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(12)
                    .gravitonCard(G.background, radius: 10)
                }
                .buttonStyle(.plain)
                .help("Open logs")
            }
        }
        .padding(20)
        .gravitonCard(radius: 14)
        .task(id: container.id) {
            while !Task.isCancelled {
                await store.loadLogs(for: container.id, tail: 3)
                try? await Task.sleep(for: .seconds(2))
            }
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
            Text(title).font(.system(size: 12)).foregroundStyle(G.muted)
            MetricValue(value, unit: unit)
            Sparkline(history, color: color).frame(height: 32)
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(G.raised, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
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
            Text(label).foregroundStyle(G.muted).gridColumnAlignment(.leading)
            Text(value).font(mono ? G.mono(12) : .system(size: 12.5)).textSelection(.enabled)
        }
    }
}
