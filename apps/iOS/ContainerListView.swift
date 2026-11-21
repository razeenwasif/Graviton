import GravitonKit
import SwiftUI

struct ContainerListView: View {
    @Environment(EngineStore.self) private var store
    var openHosts: () -> Void = {}
    @State private var filter: ContainerFilter = .all
    @State private var search = ""
    @State private var running = false
    @State private var path: [String] = []
    @State private var collapsed: Set<String> = []

    private var rows: [Container] { filter.apply(store.containers, search: search) }
    private var grouped: (stacks: [ContainerStack], standalone: [Container]) { ContainerStack.group(rows) }

    var body: some View {
        NavigationStack(path: $path) {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    if store.connection == .unreachable {
                        GStatusLine(.failure, "Can't reach \(store.liveName ?? "the Mac"). Showing what it last sent. \(store.liveError ?? "")")
                    }

                    HStack(spacing: 10) {
                        SummaryTile(title: "Running", symbol: "play.circle") {
                            MetricValue("\(store.runningCount)", unit: " / \(store.containers.count)", size: 22)
                        }
                        SummaryTile(title: "CPU", symbol: "cpu") {
                            MetricValue(String(format: "%.0f", store.totalCPU), unit: "%", size: 22)
                        }
                        SummaryTile(title: "Memory", symbol: "memorychip") {
                            let parts = Format.bytes(store.totalMemory).split(separator: " ")
                            MetricValue(String(parts.first ?? ""), unit: " " + String(parts.last ?? ""), size: 22)
                        }
                    }

                    MobileSearchField(prompt: "Search containers", text: $search)

                    GSegmented(selection: $filter, options: ContainerFilter.allCases.map { f in
                        let n = switch f {
                        case .all: store.containers.count
                        case .running: store.runningCount
                        case .stopped: store.containers.count - store.runningCount
                        }
                        return (f, "\(f.rawValue) \(n)")
                    }, stretch: true)

                    if store.containers.isEmpty {
                        GEmptyState("No containers yet", symbol: "shippingbox",
                                    detail: "Run one on \(store.liveName ?? "the Mac"): it boots in its own small VM in a second or two.",
                                    actionTitle: store.canManage ? "Run a container" : nil) { running = true }
                            .frame(minHeight: 320)
                    } else if rows.isEmpty {
                        GEmptyState("Nothing matches", symbol: "magnifyingglass",
                                    detail: search.isEmpty ? "No \(filter.rawValue.lowercased()) containers." : "No container's name or image contains “\(search)”.")
                            .frame(minHeight: 260)
                    } else if grouped.stacks.isEmpty {
                        containerCard(rows)
                    } else {
                        ForEach(grouped.stacks) { stack in
                            stackCard(stack)
                        }
                        if !grouped.standalone.isEmpty {
                            VStack(alignment: .leading, spacing: 8) {
                                GCaption("Standalone").padding(.horizontal, 4)
                                containerCard(grouped.standalone)
                            }
                        }
                    }
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                VStack(alignment: .leading, spacing: 0) {
                    MobileHeader("Containers", large: true) {
                        Button { running = true } label: { Label("Run container", systemImage: "plus") }
                            .buttonStyle(CircleIconStyle(prominent: true))
                            .disabled(!store.canManage)
                    }
                    HostChip(openHosts: openHosts)
                        .padding(.horizontal, 20)
                        .padding(.bottom, 12)
                }
                .background(G.background)
            }
            .refreshable { await store.refresh() }
            .mobileScreen(pushed: false)
            .navigationDestination(for: String.self) { ContainerDetailView(containerID: $0) }
            .sheet(isPresented: $running) {
                MobileRunSheet { c in path = [c.id] }
            }
        }
    }

    private func containerCard(_ items: [Container]) -> some View {
        VStack(spacing: 0) {
            ForEach(Array(items.enumerated()), id: \.element.id) { index, c in
                if index > 0 { RowDivider(inset: 36) }
                NavigationLink(value: c.id) { ContainerRow(container: c) }
                    .buttonStyle(PressRowStyle())
                    .contextMenu { LifecycleMenu(container: c) }
            }
        }
        .mobileCard()
    }

    private func stackCard(_ stack: ContainerStack) -> some View {
        let isCollapsed = collapsed.contains(stack.name)
        return VStack(spacing: 0) {
            HStack(spacing: 10) {
                Button {
                    withAnimation(.snappy(duration: 0.2)) {
                        if isCollapsed { collapsed.remove(stack.name) } else { collapsed.insert(stack.name) }
                    }
                } label: {
                    HStack(spacing: 10) {
                        Image(systemName: isCollapsed ? "chevron.right" : "chevron.down")
                            .font(.system(size: 11, weight: .semibold))
                            .foregroundStyle(G.muted)
                            .frame(width: 12)
                        Image(systemName: "square.stack.3d.up.fill")
                            .font(.system(size: 14))
                            .foregroundStyle(stack.anyRunning ? G.accent : G.muted)
                            .frame(width: 28, height: 28)
                            .background((stack.anyRunning ? G.accent : G.muted).opacity(0.14), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                        VStack(alignment: .leading, spacing: 1) {
                            Text(stack.name).font(.system(size: 15, weight: .semibold)).foregroundStyle(G.text).lineLimit(1)
                            Text("\(stack.runningCount)/\(stack.containers.count) running")
                                .font(.system(size: 11.5)).foregroundStyle(stack.anyRunning ? G.running : G.muted)
                        }
                        Spacer(minLength: 0)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                Button {
                    Task {
                        if stack.anyRunning {
                            await store.stopStack(stack.name)
                        } else {
                            await store.startStack(stack.name)
                        }
                    }
                } label: {
                    Image(systemName: stack.anyRunning ? "stop.fill" : "play.fill")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(stack.anyRunning ? G.text : G.accent)
                        .frame(width: 30, height: 30)
                        .background(G.control, in: Circle())
                        .overlay(Circle().strokeBorder(G.borderStrong))
                }
                .buttonStyle(.plain)
                .accessibilityLabel(stack.anyRunning ? "Stop stack \(stack.name)" : "Start stack \(stack.name)")

                Menu {
                    if !stack.allRunning {
                        Button("Start all", systemImage: "play.fill") { Task { await store.startStack(stack.name) } }
                    }
                    if stack.anyRunning {
                        Button("Stop all", systemImage: "stop.fill", role: .destructive) { Task { await store.stopStack(stack.name) } }
                    }
                    Button("Restart stack", systemImage: "arrow.clockwise") { Task { await store.restartStack(stack.name) } }
                } label: {
                    Image(systemName: "ellipsis")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(G.muted)
                        .frame(width: 28, height: 30)
                        .contentShape(Rectangle())
                }
                .accessibilityLabel("More actions for stack \(stack.name)")
            }
            .padding(.horizontal, 14)
            .frame(minHeight: 54)
            .background(G.raised.opacity(0.45))

            if !isCollapsed {
                RowDivider(inset: 0)
                ForEach(Array(stack.containers.enumerated()), id: \.element.id) { index, c in
                    if index > 0 { RowDivider(inset: 36) }
                    NavigationLink(value: c.id) { ContainerRow(container: c) }
                        .buttonStyle(PressRowStyle())
                        .contextMenu { LifecycleMenu(container: c) }
                }
            }
        }
        .mobileCard()
    }
}

struct HostChip: View {
    @Environment(EngineStore.self) private var store
    @Environment(HostStore.self) private var hosts
    var openHosts: () -> Void = {}

    private var label: String {
        switch store.connection {
        case .live, .unreachable: store.liveName ?? "Mac"
        case .connecting: store.liveName ?? "Connecting…"
        case .demo: "Demo data"
        }
    }

    private var dot: Color {
        switch store.connection {
        case .live: G.running
        case .unreachable: G.danger
        case .demo, .connecting: G.warning
        }
    }

    var body: some View {
        Menu {
            Button {
                hosts.select(nil)
                store.use(nil, named: nil)
            } label: {
                Label("Demo data", systemImage: hosts.selectedID == nil ? "checkmark" : "sparkles")
            }
            ForEach(hosts.hosts) { host in
                Button {
                    hosts.select(host.id)
                    store.use(hosts.engineForSelection(), named: host.name)
                } label: {
                    Label(host.name, systemImage: hosts.selectedID == host.id ? "checkmark" : "desktopcomputer")
                }
            }
            Divider()
            Button("Pair a Mac…", systemImage: "plus", action: openHosts)
        } label: {
            HStack(spacing: 8) {
                Circle().fill(dot).frame(width: 8, height: 8)
                    .background { if store.connection == .live { Circle().fill(G.running.opacity(0.18)).frame(width: 14, height: 14) } }
                Text(label).font(.system(size: 13, weight: .medium))
                Image(systemName: "chevron.down").font(.system(size: 10, weight: .bold)).foregroundStyle(G.muted)
            }
            .foregroundStyle(G.text)
            .padding(.horizontal, 12)
            .frame(height: 32)
            .background(G.control, in: Capsule())
            .overlay(Capsule().strokeBorder(G.borderStrong))
        }
    }
}

struct SummaryTile<Content: View>: View {
    var title: String
    var symbol: String
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 5) {
                Image(systemName: symbol).font(.system(size: 10.5, weight: .semibold)).foregroundStyle(G.accent)
                Text(title.uppercased()).font(.system(size: 10.5, weight: .semibold)).kerning(0.5).foregroundStyle(G.faint)
            }
            content
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .mobileCard(radius: 14)
    }
}

struct ContainerRow: View {
    var container: Container

    var body: some View {
        HStack(spacing: 12) {
            StatusDot(running: container.isRunning, size: 9)
            VStack(alignment: .leading, spacing: 1) {
                Text(container.name).font(.system(size: 15, weight: .medium)).foregroundStyle(G.text)
                Text(container.image).font(G.mono(11.5)).foregroundStyle(G.muted).lineLimit(1)
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 2) {
                Text(container.isRunning ? String(format: "%.0f%%", container.cpuPercent ?? 0) : "Exited")
                    .font(G.mono(12.5, weight: .medium)).foregroundStyle(container.isRunning ? G.textSecondary : G.faint).monospacedDigit()
                if container.isRunning, let mem = container.memoryBytes {
                    Text(Format.bytes(mem)).font(G.mono(11)).foregroundStyle(G.faint)
                }
            }
            Image(systemName: "chevron.right").font(.system(size: 12, weight: .semibold)).foregroundStyle(Color(hex: 0x5F6672))
        }
        .padding(.horizontal, 14)
        .frame(minHeight: 64)
        .contentShape(Rectangle())
    }
}

struct LifecycleMenu: View {
    @Environment(EngineStore.self) private var store
    var container: Container

    var body: some View {
        if container.isRunning {
            Button("Stop", systemImage: "stop.fill", role: .destructive) { Task { await store.stop(container) } }
            Button("Restart", systemImage: "arrow.clockwise") { Task { await store.restart(container) } }
        } else {
            Button("Start", systemImage: "play.fill") { Task { await store.start(container) } }
        }
    }
}
