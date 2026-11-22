import AppKit
import GravitonKit
import SwiftUI

enum DetailTab: String, CaseIterable, Identifiable {
    case overview = "Overview", logs = "Logs", terminal = "Terminal", files = "Files", stats = "Stats", inspect = "Inspect"
    var id: String { rawValue }
}

struct ContainerDetailScreen: View {
    @Environment(EngineStore.self) private var store
    var containerID: String
    var onBack: () -> Void
    @State private var tab: DetailTab

    init(containerID: String, initialTab: DetailTab = .logs, onBack: @escaping () -> Void) {
        self.containerID = containerID
        self.onBack = onBack
        _tab = State(initialValue: initialTab)
    }

    var body: some View {
        if let c = store.container(containerID) {
            VStack(alignment: .leading, spacing: 0) {
                VStack(alignment: .leading, spacing: 0) {
                    ScreenHeader(c.name, subtitle: c.image, back: ("Containers", onBack)) {
                        StatusPill(c, showUptime: true)
                        Rectangle().fill(G.border).frame(width: 1, height: 20).padding(.horizontal, 4)
                        Button {
                            tab = .terminal
                        } label: {
                            Label("Terminal", systemImage: "terminal")
                        }
                        .buttonStyle(.gSecondary)
                        .disabled(!c.isRunning)
                        if c.isRunning {
                            Button {
                                Task { await store.pause(c) }
                            } label: {
                                Label("Pause", systemImage: "pause.fill")
                            }
                            .buttonStyle(.gSecondary)
                            Button {
                                Task { await store.restart(c) }
                            } label: {
                                Label("Restart", systemImage: "arrow.clockwise")
                            }
                            .buttonStyle(.gSecondary)
                            Button {
                                Task { await store.stop(c) }
                            } label: {
                                Label("Stop", systemImage: "stop.fill")
                            }
                            .buttonStyle(.gDanger)
                        } else if c.isPaused {
                            Button {
                                Task { await store.unpause(c) }
                            } label: {
                                Label("Resume", systemImage: "play.fill")
                            }
                            .buttonStyle(.gPrimary)
                            Button {
                                Task { await store.restart(c) }
                            } label: {
                                Label("Restart", systemImage: "arrow.clockwise")
                            }
                            .buttonStyle(.gSecondary)
                            Button {
                                Task { await store.stop(c) }
                            } label: {
                                Label("Stop", systemImage: "stop.fill")
                            }
                            .buttonStyle(.gDanger)
                        } else {
                            Button {
                                Task { await store.start(c) }
                            } label: {
                                Label("Start", systemImage: "play.fill")
                            }
                            .buttonStyle(.gPrimary)
                        }
                    }
                    TabStrip(selection: $tab)
                        .padding(.horizontal, 28)
                }
                .overlay(alignment: .bottom) { Rectangle().fill(G.border).frame(height: 1) }

                Group {
                    switch tab {
                    case .logs: LogsPanel(containerID: c.id)
                    case .overview: OverviewPanel(container: c)
                    case .terminal: TerminalPanel(container: c)
                    case .files: FilesPanel(container: c)
                    case .stats: StatsPanel(container: c)
                    case .inspect: InspectPanel(container: c)
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .background(G.background)
        } else {
            VStack(spacing: 0) {
                ScreenHeader("Not found", back: ("Containers", onBack))
                GEmptyState("Container not found", symbol: "shippingbox", detail: "It may have been removed.",
                            actionTitle: "Back to Containers", action: onBack)
            }
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
                GTextField("Filter logs (regex supported)", text: $query)
                    .gravitonField(mono: true, icon: "line.3.horizontal.decrease")

                GSegmented(selection: $level, options: [(nil, "All"), (.info, "Info"), (.warn, "Warn"), (.error, "Error")])

                Toggle("Follow", isOn: $follow).toggleStyle(.gSwitch)
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

/// A shell in the container. It keeps running while you switch tabs.
struct TerminalPanel: View {
    @Environment(EngineStore.self) private var store
    @Environment(ShellSessions.self) private var shells
    var container: Container
    @State private var shell: ShellSession?
    @State private var copied = false

    var body: some View {
        Group {
            if let shell {
                VStack(alignment: .leading, spacing: 10) {
                    header(shell)
                    TerminalHost(session: shell)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 10)
                        .background(G.console)
                        .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(G.border))
                }
                .padding(.horizontal, 28)
                .padding(.vertical, 18)
            } else if !container.isRunning {
                GEmptyState("Not running", symbol: "terminal", detail: "Start \(container.name) to open a shell in it.",
                            actionTitle: "Start \(container.name)") { Task { await store.start(container) } }
            } else {
                GSpinner(size: 22).frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .task(id: "\(container.id)|\(container.isRunning)") {
            if container.isRunning || shells.existing(container.id) != nil {
                shell = shells.shell(for: container.id, store: store)
            }
        }
    }

    private func header(_ shell: ShellSession) -> some View {
        HStack(spacing: 10) {
            Circle().fill(dot(shell.phase)).frame(width: 7, height: 7)
            Text(status(shell)).font(.system(size: 12.5)).foregroundStyle(G.muted).lineLimit(1)
            Spacer()
            Button(copied ? "Copied" : "Copy grav command", systemImage: copied ? "checkmark" : "doc.on.doc") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString("grav exec -it \(container.name) sh", forType: .string)
                copied = true
            }
            .help("The same shell from Terminal: grav exec -it \(container.name) sh")
            .fixedSize()
            if !shell.isLive {
                Button("New shell", systemImage: "plus") { shell.start(using: store) }
                    .buttonStyle(.gPrimary)
                    .disabled(!container.isRunning)
                    .fixedSize()
            }
        }
        .buttonStyle(.g(.secondary, size: .small))
    }

    private func status(_ shell: ShellSession) -> String {
        switch shell.phase {
        case .connecting: "Connecting…"
        case .running: shell.title ?? "Shell in \(container.name)"
        case let .exited(code): "Exited (\(code))"
        case let .failed(why): why
        }
    }

    private func dot(_ phase: ShellSession.Phase) -> Color {
        switch phase {
        case .running: G.running
        case .connecting: G.warning
        case .exited: G.stopped
        case .failed: G.danger
        }
    }
}
