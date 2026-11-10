import GravitonKit
import SwiftUI

enum DetailTab: String, CaseIterable, Identifiable {
    case overview = "Overview", logs = "Logs", terminal = "Terminal", files = "Files", stats = "Stats", inspect = "Inspect"
    var id: String { rawValue }
}

struct ContainerDetailScreen: View {
    @Environment(EngineStore.self) private var store
    var containerID: String
    @State private var tab: DetailTab = .logs

    var body: some View {
        if let c = store.container(containerID) {
            VStack(alignment: .leading, spacing: 0) {
                VStack(alignment: .leading, spacing: 14) {
                    HStack(spacing: 12) {
                        Text(c.name).font(.system(size: 20, weight: .semibold)).kerning(-0.4)
                        StatusPill(c, showUptime: true)
                        Spacer()
                        HStack(spacing: 8) {
                            Button("Open terminal") {}
                                .disabled(true)
                                .help("Interactive terminals arrive with milestone 5")
                            if c.isRunning {
                                Button("Restart") { Task { await store.restart(c) } }
                                Button("Stop") { Task { await store.stop(c) } }
                                    .buttonStyle(QuietButtonStyle(tint: G.danger))
                            } else {
                                Button("Start") { Task { await store.start(c) } }
                            }
                        }
                        .buttonStyle(QuietButtonStyle())
                        .fixedSize()
                    }
                    TabStrip(selection: $tab)
                }
                .padding(.horizontal, 28)
                .padding(.top, 18)
                .overlay(alignment: .bottom) { Divider().overlay(G.border) }

                Group {
                    switch tab {
                    case .logs: LogsPanel(containerID: c.id)
                    case .overview: OverviewPanel(container: c)
                    default:
                        ContentUnavailableView("\(tab.rawValue) is on the roadmap", systemImage: "hammer",
                                               description: Text("This tab lands once the guest agent is running containers."))
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .background(G.background)
            .navigationTitle(c.name)
            .navigationSubtitle(c.image)
        } else {
            ContentUnavailableView("Container not found", systemImage: "shippingbox",
                                   description: Text("It may have been removed."))
        }
    }
}

struct TabStrip: View {
    @Binding var selection: DetailTab
    @Namespace private var underline

    var body: some View {
        HStack(spacing: 22) {
            ForEach(DetailTab.allCases) { tab in
                Button {
                    selection = tab
                } label: {
                    Text(tab.rawValue)
                        .font(.system(size: 13, weight: selection == tab ? .medium : .regular))
                        .foregroundStyle(selection == tab ? G.text : G.muted)
                        .padding(.top, 8)
                        .padding(.bottom, 12)
                        .overlay(alignment: .bottom) {
                            if selection == tab {
                                Rectangle().fill(G.accent).frame(height: 2).matchedGeometryEffect(id: "tab", in: underline)
                            }
                        }
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityAddTraits(selection == tab ? .isSelected : [])
            }
        }
        .animation(.snappy(duration: 0.2), value: selection)
    }
}

struct LogsPanel: View {
    @Environment(EngineStore.self) private var store
    var containerID: String
    @State private var query = ""
    @State private var level: LogLevel?
    @State private var follow = true

    private var lines: [LogLine] {
        let all = store.logs[containerID] ?? []
        let q = query.trimmingCharacters(in: .whitespaces)
        let regex = q.isEmpty ? nil : try? Regex(q).ignoresCase()
        return all.filter { line in
            (level == nil || line.level == level) &&
                (q.isEmpty || (regex.map { line.message.contains($0) } ?? line.message.localizedCaseInsensitiveContains(q)))
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                HStack(spacing: 8) {
                    Image(systemName: "magnifyingglass").foregroundStyle(G.faint)
                    TextField("Filter logs (regex supported)", text: $query)
                        .textFieldStyle(.plain)
                }
                .padding(.horizontal, 12)
                .frame(height: 32)
                .gravitonCard(Color(hex: 0x15161A), radius: 8)

                Picker("Level", selection: $level) {
                    Text("All").tag(LogLevel?.none)
                    Text("Info").tag(LogLevel?.some(.info))
                    Text("Warn").tag(LogLevel?.some(.warn))
                    Text("Error").tag(LogLevel?.some(.error))
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()

                Toggle("Follow", isOn: $follow).toggleStyle(.switch).controlSize(.small)
            }

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(lines) { LogRow($0) }
                        HStack(spacing: 8) {
                            Rectangle().fill(G.accent.opacity(0.8)).frame(width: 7, height: 14)
                            Text(follow ? "Streaming · \(store.logs[containerID]?.count ?? 0) lines" : "Paused")
                        }
                        .font(G.mono(12))
                        .foregroundStyle(G.faint)
                        .padding(.leading, 68)
                        .padding(.top, 8)
                        .id("end")
                    }
                    .padding(.vertical, 14)
                }
                .gravitonCard(G.console)
                .onChange(of: lines.last?.id) {
                    if follow { withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo("end", anchor: .bottom) } }
                }
                .onAppear { proxy.scrollTo("end", anchor: .bottom) }
            }
        }
        .padding(.horizontal, 28)
        .padding(.vertical, 18)
        .task(id: containerID) {
            while !Task.isCancelled {
                if follow || store.logs[containerID] == nil { await store.loadLogs(for: containerID) }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }
}

struct OverviewPanel: View {
    @Environment(EngineStore.self) private var store
    var container: Container

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                HStack(spacing: 12) {
                    MetricTile(title: "CPU", value: container.cpuPercent.map { String(format: "%.1f", $0) } ?? "—",
                               unit: container.cpuPercent == nil ? "" : "%", history: store.cpuHistory[container.id] ?? [], color: G.accent)
                    MetricTile(title: "Memory", value: container.memoryBytes.map(Format.bytes) ?? "—", unit: "",
                               history: store.memoryHistory[container.id] ?? [], color: G.running)
                }
                Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 12) {
                    DetailRow("ID", container.id)
                    DetailRow("Image", container.image)
                    DetailRow("Ports", container.ports.isEmpty ? "—" : container.ports.map(\.label).joined(separator: "\n"))
                    DetailRow("IP address", container.ipAddress ?? "—")
                    DetailRow("Mounts", container.mounts.isEmpty ? "—" : container.mounts.map { "\($0.source) → \($0.target)" }.joined(separator: "\n"))
                    DetailRow("Memory limit", container.memoryLimitBytes.map(Format.bytes) ?? "—")
                }
                .padding(20)
                .frame(maxWidth: .infinity, alignment: .leading)
                .gravitonCard()
            }
            .frame(maxWidth: 760, alignment: .leading)
            .padding(.horizontal, 28)
            .padding(.vertical, 20)
        }
    }
}
