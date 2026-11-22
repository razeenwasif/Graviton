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
    @State private var platform = ""
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
        GSheet("Build image",
               subtitle: store.isLocal
                   ? "From a folder with a Dockerfile. Each build runs in its own VM; unchanged steps come from the cache."
                   : "From a folder on this Mac, built on \(store.liveName ?? "the other Mac"): the folder is sent there first, minus what .dockerignore leaves out.",
               symbol: "hammer", width: 660) {
            GFormRow("Folder", error: !folder.isEmpty && !hasDockerfile ? "No \(dockerfile.isEmpty ? "Dockerfile" : dockerfile) in this folder." : nil) {
                HStack(spacing: 8) {
                    HStack(spacing: 8) {
                        Image(systemName: "folder").font(.system(size: 12)).foregroundStyle(folder.isEmpty ? G.faint : G.accent)
                        Text(folder.isEmpty ? "Choose a folder…" : (folder as NSString).abbreviatingWithTildeInPath)
                            .font(G.mono(12.5))
                            .foregroundStyle(folder.isEmpty ? G.faint : G.text)
                            .lineLimit(1).truncationMode(.middle)
                    }
                    .padding(.horizontal, 11)
                    .frame(maxWidth: .infinity, minHeight: 32, alignment: .leading)
                    .background(G.background, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(G.borderStrong))
                    .contentShape(Rectangle())
                    .onTapGesture { if !building { choose() } }
                    Button("Choose…", action: choose).buttonStyle(.gSecondary).disabled(building)
                }
            }
            HStack(alignment: .top, spacing: 12) {
                GFormRow("Name") { field(defaultName, text: $tag) }
                GFormRow("Dockerfile") { field("Dockerfile", text: $dockerfile) }.frame(width: 190)
            }
            GFormRow("Build args") { field("KEY=value, KEY2=value", text: $buildArgs) }
            if store.engine?.rosetta == true {
                GFormRow("Platform") {
                    GSegmented(selection: $platform, options: [
                        ("", "Native"),
                        ("linux/amd64", "amd64 (Rosetta)"),
                        ("linux/arm64,linux/amd64", "Multi-arch"),
                    ])
                    .disabled(building)
                }
            }
            Toggle("Rebuild every step (no cache)", isOn: $noCache).toggleStyle(.gCheckbox).disabled(building)

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
                            Text("The build log shows here.").font(G.mono(11.5)).foregroundStyle(G.faint)
                        }
                    }
                    .padding(12)
                }
                .frame(height: 210)
                .background(G.console, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(G.border))
                .onChange(of: lines.count) { proxy.scrollTo(lines.count - 1, anchor: .bottom) }
            }

            status
        } footer: {
            if building {
                Text("Closing this window stops watching; the build carries on.").font(.system(size: 11.5)).foregroundStyle(G.faint)
            }
            Spacer()
            Button("Close") {
                job?.cancel()
                dismiss()
            }
            .buttonStyle(.gSecondary)
            .keyboardShortcut(.cancelAction)
            Button("Build", action: start)
                .buttonStyle(.gPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(building || !hasDockerfile || !store.canBuild)
        }
    }

    @ViewBuilder private var status: some View {
        switch phase {
        case .idle:
            if !store.canBuild {
                GStatusLine(.warning, "Builds need gravitond running (grav service install), here or on a paired Mac")
            }
        case let .building(since):
            TimelineView(.periodic(from: since, by: 1)) { context in
                GStatusLine(.working, "Building… \(Int(context.date.timeIntervalSince(since))) s")
            }
        case let .built(image, seconds):
            GStatusLine(.success, "Built \(image.repository):\(image.tag) · \(Format.bytes(image.sizeBytes)) in \(seconds) s. It's in Images, ready to run.")
        case let .failed(message):
            GStatusLine(.failure, message)
        }
    }

    private func field(_ placeholder: String, text: Binding<String>) -> some View {
        GTextField(placeholder, text: text)
            .disabled(building)
            .gravitonField(mono: true)
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
                             buildArgs: RunSpec.list(buildArgs), noCache: noCache,
                             platform: platform.isEmpty ? nil : platform)
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
