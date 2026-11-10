import GravitonKit
import SwiftUI

struct ContainerRoute: Hashable {
    var id: String
}

struct ContainersScreen: View {
    @Environment(EngineStore.self) private var store
    @State private var filter: ContainerFilter = .all
    @State private var search = ""
    @State private var selection: Container.ID?
    @State private var path: [ContainerRoute] = []
    @State private var showRun = false

    private var rows: [Container] { filter.apply(store.containers, search: search) }

    var body: some View {
        NavigationStack(path: $path) {
            HStack(alignment: .top, spacing: 24) {
                VStack(alignment: .leading, spacing: 14) {
                    Picker("Filter", selection: $filter) {
                        ForEach(ContainerFilter.allCases) { Text($0.rawValue).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .fixedSize()

                    ContainerTable(rows: rows, selection: $selection) { id in
                        path.append(ContainerRoute(id: id))
                    }
                }
                .frame(maxWidth: .infinity)

                if let selected = store.container(selection) {
                    Inspector(container: selected) { path.append(ContainerRoute(id: selected.id)) }
                        .frame(width: 330)
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                }
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .background(G.background)
            .navigationTitle("Containers")
            .navigationSubtitle("\(store.runningCount) running · \(store.containers.count - store.runningCount) stopped")
            .searchable(text: $search, placement: .toolbar, prompt: "Search containers")
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Button {
                        showRun = true
                    } label: {
                        Label("Run container", systemImage: "plus")
                    }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut("n")
                    .help(store.canManage ? "Run a new container (⌘N)" : "Start gravitond to run containers")
                }
            }
            .sheet(isPresented: $showRun) {
                RunContainerSheet { container in
                    filter = .all
                    selection = container.id
                }
            }
            .navigationDestination(for: ContainerRoute.self) { route in
                ContainerDetailScreen(containerID: route.id)
            }
            .animation(.snappy(duration: 0.2), value: selection)
            .onAppear {
                if selection == nil { selection = store.containers.first(where: \.isRunning)?.id }
            }
            .onChange(of: store.containers.map(\.id)) {
                if selection == nil { selection = store.containers.first(where: \.isRunning)?.id }
            }
        }
    }
}

struct ContainerTable: View {
    @Environment(EngineStore.self) private var store
    var rows: [Container]
    @Binding var selection: Container.ID?
    var open: (Container.ID) -> Void

    var body: some View {
        Table(rows, selection: $selection) {
            TableColumn("Name") { c in
                HStack(spacing: 12) {
                    StatusDot(running: c.isRunning)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(c.name).fontWeight(.medium).foregroundStyle(G.text)
                        Text(c.image).font(G.mono(11.5)).foregroundStyle(G.muted)
                    }
                }
                .padding(.vertical, 6)
            }
            .width(min: 200, ideal: 260)

            TableColumn("Ports") { c in
                Text(c.ports.isEmpty ? "—" : c.ports.map { "\($0.hostPort)→\($0.containerPort)" }.joined(separator: ", "))
                    .font(G.mono(12)).foregroundStyle(G.textSecondary)
            }
            .width(min: 90, ideal: 120)

            TableColumn("CPU") { c in
                Text(Format.percent(c.cpuPercent)).font(G.mono(12)).foregroundStyle(G.textSecondary).monospacedDigit()
            }
            .width(min: 60, ideal: 70)

            TableColumn("Memory") { c in
                Text(c.memoryBytes.map(Format.bytes) ?? "—").font(G.mono(12)).foregroundStyle(G.textSecondary)
            }
            .width(min: 70, ideal: 84)

            TableColumn("Uptime") { c in
                Text(c.statusText()).foregroundStyle(G.muted)
            }
            .width(min: 80, ideal: 100)

            TableColumn("") { c in
                RowMenu(container: c, open: { open(c.id) })
            }
            .width(32)
        }
        .tableStyle(.inset)
        .alternatingRowBackgrounds(.disabled)
        .scrollContentBackground(.hidden)
        .contextMenu(forSelectionType: Container.ID.self) { ids in
            if let id = ids.first, let c = store.container(id) {
                LifecycleButtons(container: c)
                Divider()
                Button("Show Logs") { open(id) }
            }
        } primaryAction: { ids in
            if let id = ids.first { open(id) }
        }
        .gravitonCard()
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
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .foregroundStyle(G.muted)
        .accessibilityLabel("More actions for \(container.name)")
    }
}

struct LifecycleButtons: View {
    @Environment(EngineStore.self) private var store
    var container: Container

    var body: some View {
        if container.isRunning {
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
                    Button("Stop") { Task { await store.stop(container) } }
                    Button("Restart") { Task { await store.restart(container) } }
                } else {
                    Button("Start") { Task { await store.start(container) } }
                    Button("Remove") { Task { await store.remove(container) } }
                }
                Button("Logs", action: openLogs)
            }
            .buttonStyle(QuietButtonStyle())

            HStack(spacing: 10) {
                MetricTile(title: "CPU", value: container.cpuPercent.map { String(format: "%.1f", $0) } ?? "—",
                           unit: container.cpuPercent == nil ? "" : "%", history: store.cpuHistory[container.id] ?? [], color: G.accent)
                MetricTile(title: "Memory", value: container.memoryBytes.map { String(Format.bytes($0).split(separator: " ")[0]) } ?? "—",
                           unit: container.memoryBytes.map { " " + (Format.bytes($0).split(separator: " ").last.map(String.init) ?? "") } ?? "",
                           history: store.memoryHistory[container.id] ?? [], color: G.running)
            }

            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 10) {
                DetailRow("ID", container.id)
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

struct QuietButtonStyle: ButtonStyle {
    var tint: Color = G.text

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13, weight: .medium))
            .foregroundStyle(tint)
            .padding(.horizontal, 12)
            .frame(maxWidth: .infinity, minHeight: 32)
            .background(G.control.opacity(configuration.isPressed ? 0.6 : 1), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(Color.white.opacity(0.08)))
            .contentShape(Rectangle())
    }
}
