import AppKit
import GravitonKit
import SwiftUI

enum DetailTab: String, CaseIterable, Identifiable {
    case overview = "Overview", logs = "Logs", terminal = "Terminal", files = "Files", diff = "Diff", stats = "Stats", inspect = "Inspect"
    var id: String { rawValue }
}

struct ContainerDetailScreen: View {
    @Environment(EngineStore.self) private var store
    var containerID: String
    var onBack: () -> Void
    @State private var tab: DetailTab
    @State private var showCommit = false
    @State private var showFork = false

    init(containerID: String, initialTab: DetailTab = .logs, onBack: @escaping () -> Void) {
        self.containerID = containerID
        self.onBack = onBack
        _tab = State(initialValue: initialTab)
    }

    var body: some View {
        if let c = store.container(containerID) {
            VStack(alignment: .leading, spacing: 0) {
                ScreenHeader(c.name, subtitle: c.image, back: ("Containers", onBack)) {
                    StatusPill(c, showUptime: true)
                    Button {
                        tab = .terminal
                    } label: {
                        Label("Terminal", systemImage: "terminal")
                    }
                    .buttonStyle(.gSecondary)
                    .disabled(!c.isRunning)

                    Button {
                        showFork = true
                    } label: {
                        Label("Fork", systemImage: "arrow.triangle.branch")
                    }
                    .buttonStyle(.gSecondary)
                    .disabled(!store.canManage)
                    .help(store.canManage ? "Clone this container’s live state into isolated forks" : "Start gravitond to fork containers")

                    Button {
                        showCommit = true
                    } label: {
                        Label("Commit", systemImage: "camera.aperture")
                    }
                    .buttonStyle(.gSecondary)
                    .disabled(!store.canManage)
                    .help(store.canManage ? "Snapshot this container into a new local image" : "Start gravitond to commit containers")

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
                    .padding(.bottom, 8)

                Group {
                    switch tab {
                    case .logs: LogsPanel(containerID: c.id)
                    case .overview: OverviewPanel(container: c)
                    case .terminal: TerminalPanel(container: c)
                    case .files: FilesPanel(container: c)
                    case .diff: DiffPanel(container: c)
                    case .stats: StatsPanel(container: c)
                    case .inspect: InspectPanel(container: c)
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .background(G.bg)
            .sheet(isPresented: $showCommit) {
                CommitContainerSheet(container: c)
            }
            .sheet(isPresented: $showFork) {
                ForkContainerSheet(container: c)
            }
        } else {
            VStack(spacing: 0) {
                ScreenHeader("Not found", back: ("Containers", onBack))
                GEmptyState("Container not found", symbol: "shippingbox", detail: "It may have been removed.",
                            actionTitle: "Back to Containers", action: onBack)
            }
            .background(G.bg)
        }
    }
}

struct TabStrip: View {
    @Binding var selection: DetailTab

    var body: some View {
        HStack(spacing: 6) {
            ForEach(DetailTab.allCases) { tab in
                GFilterPillButton(tab.rawValue, selected: selection == tab, accent: G.indigo) {
                    withAnimation(.snappy(duration: 0.18)) { selection = tab }
                }
            }
        }
    }
}

struct LogsPanel: View {
    @Environment(EngineStore.self) private var store
    var containerID: String
    @State private var query = ""
    @State private var level: LogLevel?
    @State private var follow = true
    @State private var showTimestamps = true
    @State private var window: LogTimeWindow = .all
    @State private var loadedWindow: LogTimeWindow?
    @State private var copiedAll = false

    private struct StreamKey: Equatable {
        var id: String
        var follow: Bool
        var window: LogTimeWindow
        var active: Bool
        var connection: EngineStore.Connection
    }

    private var containerActive: Bool {
        store.container(containerID)?.isActive ?? false
    }

    private var streamKey: StreamKey {
        StreamKey(
            id: containerID,
            follow: follow,
            window: window,
            active: containerActive,
            connection: store.connection
        )
    }

    private var lines: [LogLine] {
        let all = store.logs[containerID] ?? []
        let q = query.trimmingCharacters(in: .whitespaces)
        let regex = q.isEmpty ? nil : try? Regex(q).ignoresCase()
        return all.filter { line in
            (level == nil || line.level == level) &&
                (q.isEmpty || (regex.map { line.message.contains($0) } ?? line.message.localizedCaseInsensitiveContains(q)))
        }
    }

    private var footerLabel: String {
        let count = store.logs[containerID]?.count ?? 0
        if !follow {
            return "Paused · \(count) lines"
        }
        return containerActive ? "Live stream · \(count) lines" : "End of logs · \(count) lines"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                GBorderlessSearchField("Filter logs (regex supported)", text: $query)

                GSegmented(selection: $level, options: [(nil, "All"), (.info, "Info"), (.warn, "Warn"), (.error, "Error")])

                Toggle("Follow", isOn: $follow).toggleStyle(.gSwitch)
            }

            HStack(spacing: 10) {
                GSegmented(selection: $window, options: LogTimeWindow.allCases.map { ($0, $0.rawValue) })
                Spacer()
                GActionPillButton(copiedAll ? "Copied" : "Copy All", icon: copiedAll ? "checkmark" : "doc.on.doc") {
                    let dump = lines.map { "[\(Format.clock($0.date))] [\($0.level.label)] \($0.message)" }.joined(separator: "\n")
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(dump, forType: .string)
                    copiedAll = true
                    Task {
                        try? await Task.sleep(for: .seconds(1.5))
                        copiedAll = false
                    }
                }
                Toggle("Timestamps", isOn: $showTimestamps).toggleStyle(.gSwitch)
            }

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(lines) { LogRow($0, showTimestamp: showTimestamps) }
                        HStack(spacing: 8) {
                            Rectangle().fill(follow && containerActive ? G.mint : G.indigo.opacity(0.8)).frame(width: 7, height: 14)
                            Text(footerLabel)
                        }
                        .font(G.mono(12))
                        .foregroundStyle(G.textMuted)
                        .padding(.leading, 68)
                        .padding(.top, 10)
                        .id("end")
                    }
                    .padding(.vertical, 14)
                }
                .background(G.console, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                .onChange(of: lines.last?.id) {
                    if follow { withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo("end", anchor: .bottom) } }
                }
                .onAppear { proxy.scrollTo("end", anchor: .bottom) }
            }
        }
        .padding(.horizontal, 28)
        .padding(.vertical, 16)
        .task(id: streamKey) {
            if follow {
                loadedWindow = window
                await store.followLogs(for: containerID, tail: 500, sinceMs: window.sinceMs())
            } else if store.logs[containerID] == nil || loadedWindow != window {
                loadedWindow = window
                await store.loadLogs(for: containerID, tail: 500, sinceMs: window.sinceMs())
            }
        }
    }
}

/// Snapshot a container's filesystem and configuration into a new local image (`grav commit`).
struct CommitContainerSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    var container: Container
    @State private var reference = ""
    @State private var author = ""
    @State private var message = ""
    @State private var changesText = ""
    @State private var phase: Phase = .idle
    @FocusState private var focused: Bool

    enum Phase: Equatable {
        case idle
        case committing
        case done(ImageInfo)
        case failed(String)
    }

    private var committing: Bool { if case .committing = phase { true } else { false } }

    private var parsedChanges: [String] {
        changesText
            .split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
    }

    var body: some View {
        GSheet("Commit to image",
               subtitle: "Snapshot \(container.name)’s filesystem and config into a local OCI image.",
               symbol: "camera.aperture") {
            GFormRow("Reference", hint: "repo[:tag]. Leave blank to default to committed:\(String(container.id.prefix(12))).") {
                GTextField("e.g. \(container.name):snapshot", text: $reference)
                    .focused($focused)
                    .disabled(committing)
                    .onSubmit(commit)
                    .gravitonField(mono: true, height: 36)
            }
            HStack(alignment: .top, spacing: 12) {
                GFormRow("Author") {
                    GTextField("Optional name or email", text: $author)
                        .disabled(committing)
                        .gravitonField()
                }
                GFormRow("Message") {
                    GTextField("Optional commit message", text: $message)
                        .disabled(committing)
                        .gravitonField()
                }
            }
            GFormRow("Config changes", hint: "Optional Dockerfile instructions (one per line), e.g. ENV MODE=prod or CMD [\"sh\"].") {
                TextEditor(text: $changesText)
                    .scrollContentBackground(.hidden)
                    .disabled(committing)
                    .frame(height: 68)
                    .gravitonField(mono: true, height: nil)
            }

            switch phase {
            case .idle:
                if !store.canManage {
                    GStatusLine(.warning, "Start gravitond to commit containers.")
                }
            case .committing:
                GStatusLine(.working, "Snapshotting \(container.name)’s overlay filesystem…")
            case let .done(image):
                GStatusLine(.success, "Committed \(image.repository):\(image.tag) · \(Format.bytes(image.sizeBytes))")
            case let .failed(reason):
                GStatusLine(.failure, reason)
            }
        } footer: {
            Spacer()
            Button("Close") { dismiss() }
                .buttonStyle(.gSecondary)
                .keyboardShortcut(.cancelAction)
            Button("Commit", action: commit)
                .buttonStyle(.gPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(committing || !store.canManage)
        }
        .onAppear { focused = true }
    }

    private func commit() {
        guard !committing, store.canManage else { return }
        let trimmedRef = reference.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedAuthor = author.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedMsg = message.trimmingCharacters(in: .whitespacesAndNewlines)
        let spec = CommitSpec(
            container: container.id,
            reference: trimmedRef,
            author: trimmedAuthor.isEmpty ? nil : trimmedAuthor,
            message: trimmedMsg.isEmpty ? nil : trimmedMsg,
            changes: parsedChanges
        )
        phase = .committing
        Task {
            do {
                let image = try await store.commit(spec)
                phase = .done(image)
            } catch {
                phase = .failed(error.localizedDescription)
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
                HStack(spacing: 34) {
                    GMetricStatColumn(
                        label: "CPU",
                        value: Format.percent(container.cpuPercent),
                        subtitle: "current utilization",
                        valueColor: G.indigo
                    )
                    GMetricStatColumn(
                        label: "Memory",
                        value: container.memoryBytes.map(Format.bytes) ?? "—",
                        subtitle: container.memoryLimitBytes.map { "limit \(Format.bytes($0))" } ?? "VM working set",
                        valueColor: G.sky
                    )
                    GMetricStatColumn(
                        label: "Published Ports",
                        value: "\(container.ports.count)",
                        subtitle: "TCP / UDP bindings",
                        valueColor: G.mint
                    )
                    GMetricStatColumn(
                        label: "Mounts",
                        value: "\(container.mounts.count)",
                        subtitle: "volumes & binds",
                        valueColor: G.amber
                    )
                    Spacer()
                }
                .padding(.vertical, 14)
                .padding(.horizontal, 18)
                .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 12, style: .continuous))

                HStack(spacing: 12) {
                    MetricTile(
                        title: "CPU History",
                        value: container.cpuPercent.map { String(format: "%.1f", $0) } ?? "—",
                        unit: container.cpuPercent == nil ? "" : "%",
                        history: store.cpuHistory[container.id] ?? [],
                        color: G.indigo
                    )
                    MetricTile(
                        title: "Memory History",
                        value: container.memoryBytes.map(Format.bytes) ?? "—",
                        unit: "",
                        history: store.memoryHistory[container.id] ?? [],
                        color: G.sky
                    )
                }

                GKeyValueCard("Configuration & Networking") {
                    GKeyValueRow("Container ID", container.id)
                    GKeyValueRow("Image", container.image)
                    if let parentId = container.forkedFrom {
                        let parentName = store.container(parentId)?.name
                        GKeyValueRow("Forked From", parentName.map { "\($0) (\(String(parentId.prefix(12))))" } ?? String(parentId.prefix(12)))
                    }
                    if let project = container.project {
                        GKeyValueRow("Compose Stack", container.service.map { "\(project) / \($0)" } ?? project)
                    }
                    GKeyValueRow("IP Address", container.ipAddress ?? "—")
                    GKeyValueRow("Ports", container.ports.isEmpty ? "—" : container.ports.map(\.label).joined(separator: ", "))
                    GKeyValueRow("Mounts", container.mounts.isEmpty ? "—" : container.mounts.map { "\($0.source) → \($0.target)" }.joined(separator: ", "))
                    GKeyValueRow("Memory Limit", container.memoryLimitBytes.map(Format.bytes) ?? "—")
                }
            }
            .frame(maxWidth: 820, alignment: .leading)
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
                        .padding(.horizontal, 14)
                        .padding(.vertical, 12)
                        .background(G.console)
                        .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                }
                .padding(.horizontal, 28)
                .padding(.vertical, 16)
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
            Text(status(shell)).font(.system(size: 12.5)).foregroundStyle(G.textSecondary).lineLimit(1)
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
        case .running: G.mint
        case .connecting: G.amber
        case .exited: G.slate
        case .failed: G.rose
        }
    }
}
