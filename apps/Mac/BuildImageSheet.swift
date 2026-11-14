import AppKit
import GravitonKit
import SwiftUI

/// Builds an image from a folder with a Dockerfile, through gravitond
/// (`grav build`), showing the build log as it runs.
struct BuildImageSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @AppStorage("graviton.lastBuildFolder") private var folder = ""
    @State private var tag = ""
    @State private var dockerfile = ""
    @State private var buildArgs = ""
    @State private var noCache = false
    @State private var lines: [String] = []
    @State private var phase: Phase = .idle
    @State private var job: Task<Void, Never>?

    enum Phase: Equatable {
        case idle
        case building(since: Date)
        case built(ImageInfo, seconds: Int)
        case failed(String)
    }

    private var building: Bool { if case .building = phase { true } else { false } }
    private var folderURL: URL? { folder.isEmpty ? nil : URL(fileURLWithPath: folder) }
    private var hasDockerfile: Bool {
        guard let folderURL else { return false }
        let name = dockerfile.trimmingCharacters(in: .whitespaces)
        return FileManager.default.fileExists(atPath: folderURL.appending(path: name.isEmpty ? "Dockerfile" : name).path)
    }
    private var defaultName: String {
        let base = folderURL?.lastPathComponent.lowercased() ?? "image"
        let cleaned = String(base.map { $0.isLetter || $0.isNumber || "-_.".contains($0) ? $0 : "-" }).trimmingCharacters(in: CharacterSet(charactersIn: "-._"))
        return (cleaned.isEmpty ? "image" : cleaned) + ":latest"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Build image").font(.system(size: 16, weight: .semibold))
                Text(store.isLocal
                     ? "From a folder with a Dockerfile. Each build runs in its own VM; unchanged steps come from the cache."
                     : "From a folder on this Mac, built on \(store.liveName ?? "the other Mac"): the folder is sent there first, minus what .dockerignore leaves out.")
                    .font(.system(size: 12.5)).foregroundStyle(G.muted)
            }

            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 10) {
                GridRow {
                    label("Folder")
                    HStack(spacing: 8) {
                        Text(folder.isEmpty ? "Choose a folder…" : (folder as NSString).abbreviatingWithTildeInPath)
                            .font(G.mono(12.5))
                            .foregroundStyle(folder.isEmpty ? G.faint : G.text)
                            .lineLimit(1).truncationMode(.middle)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.horizontal, 10).frame(height: 30)
                            .background(G.background, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                        Button("Choose…", action: choose).disabled(building)
                    }
                }
                GridRow {
                    label("Name")
                    field(defaultName, text: $tag)
                }
                GridRow {
                    label("Dockerfile")
                    field("Dockerfile", text: $dockerfile)
                }
                GridRow {
                    label("Build args")
                    field("KEY=value, KEY2=value", text: $buildArgs)
                }
                GridRow {
                    Color.clear.gridCellUnsizedAxes([.horizontal, .vertical])
                    Toggle("Rebuild every step (no cache)", isOn: $noCache).toggleStyle(.checkbox).disabled(building)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 1) {
                        ForEach(Array(lines.enumerated()), id: \.offset) { i, line in
                            Text(line)
                                .font(G.mono(11.5))
                                .foregroundStyle(style(line))
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .id(i)
                        }
                        if lines.isEmpty {
                            Text(hasDockerfile || folder.isEmpty ? "The build log shows here." : "No Dockerfile in this folder.")
                                .font(G.mono(11.5)).foregroundStyle(hasDockerfile || folder.isEmpty ? G.faint : G.warning)
                        }
                    }
                    .padding(12)
                }
                .frame(height: 230)
                .background(G.console, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(G.border))
                .onChange(of: lines.count) { proxy.scrollTo(lines.count - 1, anchor: .bottom) }
            }

            status.font(.system(size: 12.5)).fixedSize(horizontal: false, vertical: true)

            HStack {
                if building {
                    Text("Closing this window stops watching; the build carries on.").font(.system(size: 11.5)).foregroundStyle(G.faint)
                }
                Spacer()
                Button("Close") {
                    job?.cancel()
                    dismiss()
                }
                .keyboardShortcut(.cancelAction)
                Button("Build", action: start)
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(building || !hasDockerfile || !store.canBuild)
            }
        }
        .padding(22)
        .frame(width: 640)
        .background(Color(hex: 0x15161A))
    }

    @ViewBuilder private var status: some View {
        switch phase {
        case .idle:
            if !store.canBuild {
                Label("Builds need gravitond running (grav service install), here or on a paired Mac", systemImage: "exclamationmark.triangle")
                    .foregroundStyle(G.warning)
            }
        case let .building(since):
            HStack(spacing: 10) {
                ProgressView().controlSize(.small)
                TimelineView(.periodic(from: since, by: 1)) { context in
                    Text("Building… \(Int(context.date.timeIntervalSince(since))) s")
                }
            }
            .foregroundStyle(G.textSecondary)
        case let .built(image, seconds):
            Label("Built \(image.repository):\(image.tag) · \(Format.bytes(image.sizeBytes)) in \(seconds) s. It's in Images, ready to run.", systemImage: "checkmark.circle.fill")
                .foregroundStyle(G.running)
        case let .failed(message):
            Label(message, systemImage: "xmark.octagon.fill")
                .foregroundStyle(G.danger)
                .textSelection(.enabled)
        }
    }

    private func label(_ text: String) -> some View {
        Text(text).font(.system(size: 12, weight: .medium)).foregroundStyle(G.textSecondary).gridColumnAlignment(.trailing)
    }

    private func field(_ placeholder: String, text: Binding<String>) -> some View {
        TextField(placeholder, text: text)
            .textFieldStyle(.plain)
            .font(G.mono(12.5))
            .padding(.horizontal, 10)
            .frame(maxWidth: .infinity, minHeight: 30, maxHeight: 30)
            .background(G.background, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .disabled(building)
    }

    /// Step headers stand out; Graviton's own notes are dimmer than the program's output.
    private func style(_ line: String) -> Color {
        if line.hasPrefix("Step ") { return G.text }
        if line.hasPrefix(" --->") || line.hasPrefix("Building ") || line.hasPrefix("Built ") { return G.faint }
        return G.textSecondary
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Choose"
        panel.message = "Choose the folder to build (it should contain a Dockerfile)."
        if let folderURL { panel.directoryURL = folderURL }
        if panel.runModal() == .OK, let url = panel.url {
            folder = url.path
        }
    }

    private func start() {
        guard !building, hasDockerfile, store.canBuild else { return }
        let name = tag.trimmingCharacters(in: .whitespaces)
        let file = dockerfile.trimmingCharacters(in: .whitespaces)
        let spec = BuildSpec(context: folder, dockerfile: file.isEmpty ? nil : file, tag: name.isEmpty ? nil : name,
                             buildArgs: RunSpec.list(buildArgs), noCache: noCache)
        lines = []
        let since = Date.now
        phase = .building(since: since)
        job = Task {
            do {
                for try await event in store.build(spec) {
                    switch event {
                    case let .log(line): lines.append(line)
                    case let .built(image): phase = .built(image, seconds: Int(Date.now.timeIntervalSince(since).rounded()))
                    }
                }
            } catch is CancellationError {
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}
