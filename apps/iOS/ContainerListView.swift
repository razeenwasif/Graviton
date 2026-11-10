import GravitonKit
import SwiftUI

struct ContainerListView: View {
    @Environment(EngineStore.self) private var store
    var openHosts: () -> Void = {}
    @State private var filter: ContainerFilter = .all
    @State private var search = ""

    private var rows: [Container] { filter.apply(store.containers, search: search) }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    HostChip(openHosts: openHosts)
                    if store.connection == .unreachable {
                        Label("Can't reach \(store.liveName ?? "the Mac"). Showing what it last sent. \(store.liveError ?? "")", systemImage: "wifi.exclamationmark")
                            .font(.system(size: 13))
                            .foregroundStyle(G.danger)
                    }

                    HStack(spacing: 10) {
                        SummaryTile(title: "Running") {
                            MetricValue("\(store.runningCount)", unit: " / \(store.containers.count)", size: 22)
                        }
                        SummaryTile(title: "CPU") {
                            MetricValue(String(format: "%.0f", store.totalCPU), unit: "%", size: 22)
                        }
                        SummaryTile(title: "Memory") {
                            let parts = Format.bytes(store.totalMemory).split(separator: " ")
                            MetricValue(String(parts.first ?? ""), unit: " " + String(parts.last ?? ""), size: 22)
                        }
                    }

                    Picker("Filter", selection: $filter) {
                        ForEach(ContainerFilter.allCases) { Text($0.rawValue).tag($0) }
                    }
                    .pickerStyle(.segmented)

                    VStack(spacing: 0) {
                        ForEach(Array(rows.enumerated()), id: \.element.id) { index, c in
                            if index > 0 { Divider().overlay(G.border) }
                            NavigationLink(value: c.id) { ContainerRow(container: c) }
                                .buttonStyle(.plain)
                                .contextMenu { LifecycleMenu(container: c) }
                        }
                        if rows.isEmpty {
                            Text("No containers").foregroundStyle(G.muted).frame(maxWidth: .infinity, minHeight: 60)
                        }
                    }
                    .background(G.card, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)
            }
            .background(G.background)
            .navigationTitle("Containers")
            .searchable(text: $search, prompt: "Search containers")
            .refreshable { await store.refresh() }
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Button {} label: { Image(systemName: "plus") }
                        .disabled(true)
                        .accessibilityLabel("Run container")
                }
            }
            .navigationDestination(for: String.self) { ContainerDetailView(containerID: $0) }
        }
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
            .frame(height: 34)
            .background(Color(hex: 0x15161A), in: Capsule())
            .overlay(Capsule().strokeBorder(Color.white.opacity(0.08)))
        }
    }
}

struct SummaryTile<Content: View>: View {
    var title: String
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title).font(.system(size: 12)).foregroundStyle(G.muted)
            content
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(G.card, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
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
            Text(container.isRunning ? String(format: "%.0f%%", container.cpuPercent ?? 0) : "Exited")
                .font(G.mono(12)).foregroundStyle(G.muted).monospacedDigit()
            Image(systemName: "chevron.right").font(.system(size: 12, weight: .semibold)).foregroundStyle(Color(hex: 0x5F6672))
        }
        .padding(.horizontal, 14)
        .frame(minHeight: 60)
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
