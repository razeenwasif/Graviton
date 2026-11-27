import AppKit
import GravitonKit
import SwiftUI

/// The Inspect tab: borderless Stratus specification cards with JSON copy.
struct InspectPanel: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    @State private var details: ContainerDetails?
    @State private var problem: String?
    @State private var showValues = false
    @State private var copiedJSON = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if let problem {
                    GEmptyState("Can't inspect \(container.name)", symbol: "exclamationmark.triangle", detail: problem)
                        .frame(minHeight: 300)
                } else if let d = details {
                    HStack(spacing: 8) {
                        Spacer()
                        Button(copiedJSON ? "Copied JSON" : "Copy as JSON", systemImage: copiedJSON ? "checkmark" : "doc.on.doc") {
                            copyJSON(d)
                            copiedJSON = true
                            Task {
                                try? await Task.sleep(for: .seconds(1.5))
                                copiedJSON = false
                            }
                        }
                        .fixedSize()
                        if store.liveName == "This Mac" {
                            Button("Show VM Folder", systemImage: "folder") {
                                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: d.vmDir)])
                            }
                            .fixedSize()
                        }
                    }
                    .buttonStyle(.g(.secondary, size: .small))
                    .fixedSize(horizontal: false, vertical: true)

                    if let project = d.project {
                        InspectSection("Stack") {
                            InspectRow("Project", project)
                            if let service = d.service {
                                InspectRow("Service", service)
                            }
                        }
                    }
                    InspectSection("Process") {
                        InspectRow("Command", d.command.map(quote).joined(separator: " "))
                        InspectRow("Working directory", d.workingDir.isEmpty ? "/" : d.workingDir)
                        InspectRow("User", d.user ?? "root (image default)")
                        InspectRow("Hostname", d.hostname)
                    }
                    InspectSection("Image") {
                        InspectRow("Reference", d.imageReference)
                        InspectRow("Digest", d.imageDigest)
                        if let arch = d.architecture {
                            InspectRow("Architecture", d.rosetta == true ? "\(arch) · Rosetta" : arch)
                        }
                    }
                    InspectSection("Virtual machine") {
                        if let parentId = d.container.forkedFrom {
                            let parentName = store.container(parentId)?.name
                            InspectRow("Forked from", parentName.map { "\($0) (\(String(parentId.prefix(12))))" } ?? parentId)
                        }
                        InspectRow("vCPUs", "\(d.cpus)")
                        InspectRow("Memory", Format.bytes(d.memoryMib << 20))
                        InspectRow("Kernel", d.kernel)
                        InspectRow("Folder", d.vmDir)
                    }
                    InspectSection("Network") {
                        InspectRow("Networks", d.attachedNetworks.joined(separator: ", "))
                        InspectRow("IP address", d.container.ipAddress ?? "—")
                        InspectRow("Ports", d.container.ports.isEmpty ? "—" : d.container.ports.map(\.fullLabel).joined(separator: "\n"))
                    }
                    InspectSection("Mounts") {
                        InspectRow("Volumes", d.container.mounts.isEmpty ? "—" : d.container.mounts.map { "\($0.source) → \($0.target)\($0.readOnly ? " (read-only)" : "")" }.joined(separator: "\n"))
                        if let tmpfs = d.tmpfs, !tmpfs.isEmpty {
                            InspectRow("Tmpfs", tmpfs.map { $0.options.isEmpty ? $0.target : "\($0.target) (\($0.options))" }.joined(separator: "\n"))
                        }
                    }
                    if d.readOnly == true || d.usesInit || !(d.capAdd ?? []).isEmpty || !(d.capDrop ?? []).isEmpty
                        || d.shmSizeBytes != nil || !(d.ulimits ?? []).isEmpty {
                        InspectSection("Security & Runtime") {
                            if d.readOnly == true {
                                InspectRow("Root filesystem", "Read-only")
                            }
                            if d.usesInit {
                                InspectRow("Init process", "Enabled (docker-init)")
                            }
                            if let add = d.capAdd, !add.isEmpty {
                                InspectRow("Added capabilities", add.joined(separator: ", "))
                            }
                            if let drop = d.capDrop, !drop.isEmpty {
                                InspectRow("Dropped capabilities", drop.joined(separator: ", "))
                            }
                            if let shm = d.shmSizeBytes {
                                InspectRow("Shared memory (/dev/shm)", Format.bytes(shm))
                            }
                            if let ulimits = d.ulimits, !ulimits.isEmpty {
                                InspectRow("Ulimits", ulimits.map(\.displayLabel).joined(separator: ", "))
                            }
                        }
                    }
                    environment(d.env)
                    if let labels = d.labels, !labels.isEmpty {
                        InspectSection("Labels") {
                            ForEach(labels.sorted { $0.key < $1.key }, id: \.key) { key, value in
                                InspectRow(key, value)
                            }
                        }
                    }
                    if d.healthcheck != nil || d.health != nil {
                        InspectSection("Healthcheck") {
                            if let h = d.health {
                                GridRow(alignment: .center) {
                                    Text("Status").foregroundStyle(G.textMuted).frame(width: 150, alignment: .leading)
                                    HStack(spacing: 10) {
                                        HealthBadge(h.status)
                                        if h.failingStreak > 0 {
                                            Text("\(h.failingStreak) consecutive failure\(h.failingStreak == 1 ? "" : "s")")
                                                .font(G.mono(11)).foregroundStyle(G.textMuted)
                                        }
                                    }
                                }
                            }
                            if let hc = d.healthcheck {
                                InspectRow("Command", hc.commandDisplay)
                                InspectRow("Schedule", "every \(hc.intervalSecs)s · timeout \(hc.timeoutSecs)s · retries \(hc.retries)")
                                if hc.startPeriodSecs > 0 {
                                    InspectRow("Start period", "\(hc.startPeriodSecs)s")
                                }
                            }
                            if let logs = d.health?.log, !logs.isEmpty {
                                ForEach(Array(logs.reversed().prefix(3).enumerated()), id: \.offset) { _, entry in
                                    let out = entry.output.trimmingCharacters(in: .whitespacesAndNewlines)
                                    let ts = Date(timeIntervalSince1970: Double(entry.end)).formatted(date: .omitted, time: .standard)
                                    InspectRow("Exit \(entry.exitCode) · \(ts)", out.isEmpty ? "—" : out)
                                }
                            }
                        }
                    }

                    InspectSection("Lifecycle") {
                        InspectRow("ID", d.container.id)
                        InspectRow("Created", Date(timeIntervalSince1970: Double(d.container.createdAt)).formatted(date: .abbreviated, time: .standard))
                        InspectRow("Started", d.container.startedAt.map { Date(timeIntervalSince1970: Double($0)).formatted(date: .abbreviated, time: .standard) } ?? "—")
                        InspectRow("Status", d.container.statusText())
                        InspectRow("Restart policy", (d.restart ?? .no).rawValue)
                    }
                } else {
                    GSpinner(size: 20).frame(maxWidth: .infinity, minHeight: 200)
                }
            }
            .frame(maxWidth: 820, alignment: .leading)
            .padding(.horizontal, 28)
            .padding(.vertical, 18)
        }
        .task(id: container.id) {
            do {
                details = try await store.inspect(container.id)
                problem = nil
            } catch {
                problem = error.localizedDescription
            }
        }
        .onChange(of: container.status) {
            Task { details = try? await store.inspect(container.id) }
        }
    }

    private func environment(_ env: [String]) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                GCaption("Environment")
                Spacer()
                Toggle("Show values", isOn: $showValues).toggleStyle(.gSwitch)
            }
            Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 8) {
                ForEach(env, id: \.self) { pair in
                    let (key, value) = split(pair)
                    GridRow {
                        Text(key).font(G.mono(12)).foregroundStyle(G.textMuted)
                        Text(showValues ? value : String(repeating: "•", count: min(max(value.count, 4), 16)))
                            .font(G.mono(12))
                            .foregroundStyle(showValues ? G.body : G.faint)
                            .textSelection(.enabled)
                    }
                }
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(G.surfaceSubtle)
    }

    private func split(_ pair: String) -> (String, String) {
        guard let eq = pair.firstIndex(of: "=") else { return (pair, "") }
        return (String(pair[..<eq]), String(pair[pair.index(after: eq)...]))
    }

    private func quote(_ word: String) -> String {
        word.contains(where: { $0.isWhitespace || "\"'$".contains($0) }) ? "'" + word.replacingOccurrences(of: "'", with: "'\\''") + "'" : word
    }

    private func copyJSON(_ d: ContainerDetails) {
        let encoder = Wire.encoder
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        guard let data = try? encoder.encode(d), let text = String(data: data, encoding: .utf8) else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}

private struct InspectSection<Content: View>: View {
    var title: String
    @ViewBuilder var content: Content

    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            GCaption(title)
            Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 10) { content }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(G.surfaceSubtle)
    }
}

private struct InspectRow: View {
    var label: String
    var value: String

    init(_ label: String, _ value: String) {
        self.label = label
        self.value = value
    }

    var body: some View {
        GridRow(alignment: .firstTextBaseline) {
            Text(label).foregroundStyle(G.textMuted).frame(width: 150, alignment: .leading)
            Text(value).font(G.mono(12)).foregroundStyle(G.body).textSelection(.enabled)
        }
    }
}
