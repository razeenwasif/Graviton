import AppKit
import GravitonKit
import SwiftUI

/// The Inspect tab: how the container was set up, with a JSON copy.
struct InspectPanel: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    @State private var details: ContainerDetails?
    @State private var problem: String?
    @State private var showValues = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if let problem {
                    GEmptyState("Can't inspect \(container.name)", symbol: "exclamationmark.triangle", detail: problem)
                        .frame(minHeight: 300)
                } else if let d = details {
                    HStack {
                        Spacer()
                        Button("Copy as JSON", systemImage: "doc.on.doc") { copyJSON(d) }.fixedSize()
                        if store.liveName == "This Mac" {
                            Button("Show VM Folder", systemImage: "folder") {
                                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: d.vmDir)])
                            }
                            .fixedSize()
                        }
                    }
                    .buttonStyle(.g(.secondary, size: .small))
                    .fixedSize(horizontal: false, vertical: true)

                    InspectSection("Process") {
                        InspectRow("Command", d.command.map(quote).joined(separator: " "))
                        InspectRow("Working directory", d.workingDir.isEmpty ? "/" : d.workingDir)
                        InspectRow("User", d.user ?? "root (image default)")
                        InspectRow("Hostname", d.hostname)
                    }
                    InspectSection("Image") {
                        InspectRow("Reference", d.imageReference)
                        InspectRow("Digest", d.imageDigest)
                    }
                    InspectSection("Virtual machine") {
                        InspectRow("vCPUs", "\(d.cpus)")
                        InspectRow("Memory", Format.bytes(d.memoryMib << 20))
                        InspectRow("Kernel", d.kernel)
                        InspectRow("Folder", d.vmDir)
                    }
                    InspectSection("Network") {
                        InspectRow("IP address", d.container.ipAddress ?? "—")
                        InspectRow("Ports", d.container.ports.isEmpty ? "—" : d.container.ports.map { "127.0.0.1:\($0.label)" }.joined(separator: "\n"))
                    }
                    InspectSection("Mounts") {
                        InspectRow("Volumes", d.container.mounts.isEmpty ? "—" : d.container.mounts.map { "\($0.source) → \($0.target)\($0.readOnly ? " (read-only)" : "")" }.joined(separator: "\n"))
                    }
                    environment(d.env)
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
            .padding(.vertical, 20)
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
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                GCaption("Environment")
                Spacer()
                Toggle("Show values", isOn: $showValues).toggleStyle(.gSwitch)
            }
            Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 8) {
                ForEach(env, id: \.self) { pair in
                    let (key, value) = split(pair)
                    GridRow {
                        Text(key).font(G.mono(12)).foregroundStyle(G.muted)
                        Text(showValues ? value : String(repeating: "•", count: min(max(value.count, 4), 16)))
                            .font(G.mono(12))
                            .foregroundStyle(showValues ? G.text : G.faint)
                            .textSelection(.enabled)
                    }
                }
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard()
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
        .gravitonCard()
    }
}

/// A label and value; labels share one width so values line up across cards.
private struct InspectRow: View {
    var label: String
    var value: String

    init(_ label: String, _ value: String) {
        self.label = label
        self.value = value
    }

    var body: some View {
        GridRow(alignment: .firstTextBaseline) {
            Text(label).foregroundStyle(G.muted).frame(width: 150, alignment: .leading)
            Text(value).font(G.mono(12)).textSelection(.enabled)
        }
    }
}
