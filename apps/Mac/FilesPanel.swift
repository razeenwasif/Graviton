import AppKit
import GravitonKit
import SwiftUI
import UniformTypeIdentifiers

/// The Files tab: browse a running container's filesystem (volumes included),
/// preview files, and upload or download files and folders.
struct FilesPanel: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    @State private var path = "/"
    @State private var listing: DirectoryListing?
    @State private var selection: FileEntry.ID?
    @State private var preview: FileContents?
    @State private var problem: String?
    @State private var previewProblem: String?
    @State private var loading = false
    @State private var transferring: String?
    @State private var transferBanner: (tone: GStatusLine.Tone, text: String)?
    @State private var dropTargeted = false
    @State private var goTo = ""
    @State private var sort: Column = .name
    @State private var ascending = true
    @FocusState private var listFocused: Bool

    enum Column: Hashable { case name, size, modified, mode }

    private var selectedEntry: FileEntry? {
        guard let selection else { return nil }
        return entries.first { $0.id == selection }
    }

    private var entries: [FileEntry] {
        let all = listing?.entries ?? []
        func ordered(_ list: [FileEntry]) -> [FileEntry] {
            let out = list.sorted { a, b in
                switch sort {
                case .name: a.name.localizedStandardCompare(b.name) == .orderedAscending
                case .size: a.size < b.size
                case .modified: a.modified < b.modified
                case .mode: a.mode < b.mode
                }
            }
            return ascending ? out : out.reversed()
        }
        // Folders stay first whichever column sorts.
        return ordered(all.filter(\.isDirectory)) + ordered(all.filter { !$0.isDirectory })
    }

    var body: some View {
        if !container.isRunning {
            GEmptyState("Not running", symbol: "folder", detail: "Start \(container.name) to browse its files.",
                        actionTitle: "Start \(container.name)") { Task { await store.start(container) } }
        } else {
            VStack(alignment: .leading, spacing: 12) {
                toolbar
                if let transferring {
                    GStatusLine(.working, transferring)
                } else if let banner = transferBanner {
                    GStatusLine(banner.tone, banner.text)
                        .overlay(alignment: .trailing) {
                            Button { transferBanner = nil } label: { Image(systemName: "xmark") }
                                .buttonStyle(GIconButtonStyle(size: 22))
                                .padding(.trailing, 8)
                                .help("Dismiss")
                        }
                }
                HSplitView {
                    list.frame(minWidth: 380, maxHeight: .infinity)
                    previewPane.frame(minWidth: 280, idealWidth: 420, maxHeight: .infinity)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .gravitonCard()
                .overlay {
                    if dropTargeted {
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .strokeBorder(G.accent, lineWidth: 2)
                            .background(G.accent.opacity(0.06), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                            .overlay(alignment: .bottom) {
                                Label("Drop to upload into \(path)", systemImage: "square.and.arrow.up")
                                    .font(.system(size: 12.5, weight: .medium))
                                    .foregroundStyle(G.text)
                                    .padding(.horizontal, 14)
                                    .padding(.vertical, 8)
                                    .background(G.raised, in: Capsule())
                                    .overlay(Capsule().strokeBorder(G.accent.opacity(0.5)))
                                    .padding(.bottom, 16)
                            }
                            .allowsHitTesting(false)
                    }
                }
                .onDrop(of: [.fileURL], isTargeted: $dropTargeted) { handleDrop($0) }
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 18)
            .task(id: "\(container.id)|\(path)") { await load() }
        }
    }

    // MARK: Toolbar

    private var toolbar: some View {
        HStack(spacing: 10) {
            Button { open(parent(of: path)) } label: { Image(systemName: "chevron.up") }
                .disabled(path == "/")
                .help("Enclosing folder")
            HStack(spacing: 2) {
                ForEach(Array(crumbs.enumerated()), id: \.offset) { i, crumb in
                    if i > 0 { Image(systemName: "chevron.right").font(.system(size: 9, weight: .semibold)).foregroundStyle(G.faint) }
                    Button(crumb.name) { open(crumb.path) }
                        .buttonStyle(.plain)
                        .font(G.mono(12.5, weight: i == crumbs.count - 1 ? .medium : .regular))
                        .foregroundStyle(i == crumbs.count - 1 ? G.text : G.muted)
                        .padding(.horizontal, 4)
                }
            }
            .padding(.horizontal, 8)
            .frame(height: 32)
            .background(G.surface, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(G.border))
            Spacer()
            GTextField("Go to path", text: $goTo)
                .onSubmit {
                    let target = goTo.trimmingCharacters(in: .whitespaces)
                    guard !target.isEmpty else { return }
                    open(target.hasPrefix("/") ? target : join(path, target))
                    goTo = ""
                }
                .gravitonField(mono: true, icon: "arrow.turn.down.right")
                .frame(width: 220)
            Button { pickAndUpload() } label: { Image(systemName: "square.and.arrow.up") }
                .disabled(!store.canTransferFiles || transferring != nil)
                .help("Upload files or folders into \(path)")
            Button { downloadSelection() } label: { Image(systemName: "square.and.arrow.down") }
                .disabled(!store.canTransferFiles || transferring != nil)
                .help(selectedEntry.map { "Download \($0.name)" } ?? "Download \(path)")
            Button { Task { await load() } } label: { Image(systemName: "arrow.clockwise") }
                .help("Refresh")
        }
        .buttonStyle(.gIconBordered)
    }

    private var crumbs: [(name: String, path: String)] {
        var out = [(name: "/", path: "/")]
        var acc = ""
        for part in path.split(separator: "/") {
            acc += "/" + part
            out.append((String(part), acc))
        }
        return out
    }

    // MARK: List

    private var list: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                GColumnHeader("Name", key: Column.name, sort: $sort, ascending: $ascending).frame(maxWidth: .infinity)
                GColumnHeader("Size", key: Column.size, sort: $sort, ascending: $ascending, alignment: .trailing).frame(width: 64)
                GColumnHeader("Modified", key: Column.modified, sort: $sort, ascending: $ascending).frame(width: 130)
                GColumnHeader("Mode", key: Column.mode, sort: $sort, ascending: $ascending).frame(width: 84)
            }
            .padding(.horizontal, 16)
            .frame(height: 32)
            .overlay(alignment: .bottom) { Rectangle().fill(G.border).frame(height: 1) }

            ScrollView {
                LazyVStack(spacing: 1) {
                    ForEach(entries) { e in
                        fileRow(e)
                    }
                }
                .padding(6)
            }
            .overlay {
                if let problem {
                    GEmptyState("Can't open \(path)", symbol: "exclamationmark.triangle", detail: problem)
                } else if listing?.entries.isEmpty == true {
                    GEmptyState("Empty folder", symbol: "folder")
                } else if loading && listing == nil {
                    GSpinner(size: 18)
                }
            }
        }
        .focusable()
        .focused($listFocused)
        .focusEffectDisabled()
        .onKeyPress(.downArrow) { move(1) }
        .onKeyPress(.upArrow) { move(-1) }
        .onKeyPress(.return) {
            guard let name = selection, let e = entries.first(where: { $0.id == name }) else { return .ignored }
            activate(e)
            return .handled
        }
        .onChange(of: selection) { _, name in
            guard let name, let e = entries.first(where: { $0.id == name }) else { return }
            if !e.isDirectory { Task { await loadPreview(join(path, e.name)) } }
        }
    }

    private func fileRow(_ e: FileEntry) -> some View {
        HStack(spacing: 10) {
            HStack(spacing: 8) {
                Image(systemName: icon(e))
                    .font(.system(size: 12))
                    .foregroundStyle(e.isDirectory ? G.chartPrimary : G.muted)
                    .frame(width: 16)
                Text(e.name).font(.system(size: 12.5)).foregroundStyle(G.text).lineLimit(1)
                if let target = e.linkTarget {
                    Text("→ \(target)").font(.system(size: 12)).foregroundStyle(G.faint).lineLimit(1).truncationMode(.middle)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            Text(e.isDirectory ? "—" : Format.bytes(e.size)).font(.system(size: 12)).foregroundStyle(G.muted).monospacedDigit()
                .frame(width: 64, alignment: .trailing)
            Text(e.modifiedDate, format: .dateTime.day().month(.abbreviated).year().hour().minute())
                .font(.system(size: 12)).foregroundStyle(G.muted).lineLimit(1)
                .frame(width: 130, alignment: .leading)
            Text(e.permissions).font(G.mono(11.5)).foregroundStyle(G.faint)
                .frame(width: 84, alignment: .leading)
        }
        .padding(.horizontal, 10)
        .frame(height: 30)
        .gravitonRow(selected: selection == e.id)
        .onTapGesture(count: 2) { activate(e) }
        .simultaneousGesture(TapGesture().onEnded {
            selection = e.id
            listFocused = true
        })
        .contextMenu {
            Button("Download…") {
                pickAndDownload(remotePath: join(path, e.name), suggestedName: e.name)
            }
            .disabled(!store.canTransferFiles || transferring != nil)
            Divider()
            Button("Copy Path") { copy(join(path, e.name)) }
        }
    }

    private func move(_ by: Int) -> KeyPress.Result {
        let list = entries
        guard !list.isEmpty else { return .ignored }
        let index = list.firstIndex { $0.id == selection } ?? (by > 0 ? -1 : list.count)
        selection = list[min(max(index + by, 0), list.count - 1)].id
        return .handled
    }

    // MARK: Preview

    @ViewBuilder private var previewPane: some View {
        if let preview {
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 8) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(preview.path.split(separator: "/").last.map(String.init) ?? preview.path).font(.system(size: 13, weight: .medium))
                        Text(preview.truncated ? "First \(Format.bytes(UInt64(preview.data.count))) of \(Format.bytes(preview.size))" : Format.bytes(preview.size))
                            .font(.system(size: 11.5)).foregroundStyle(G.faint)
                    }
                    Spacer()
                    Button("Download", systemImage: "square.and.arrow.down") {
                        let name = preview.path.split(separator: "/").last.map(String.init) ?? "file"
                        pickAndDownload(remotePath: preview.path, suggestedName: name)
                    }
                    .buttonStyle(.g(.secondary, size: .small))
                    .disabled(!store.canTransferFiles || transferring != nil)
                    if let text = preview.text {
                        Button("Copy", systemImage: "doc.on.doc") { copy(text) }.buttonStyle(.g(.secondary, size: .small))
                    }
                }
                .padding(12)
                Rectangle().fill(G.border).frame(height: 1)
                ScrollView([.vertical, .horizontal]) {
                    Text(preview.text ?? hexDump(preview.data))
                        .font(G.mono(12))
                        .foregroundStyle(preview.text == nil ? G.muted : G.textSecondary)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                        .padding(12)
                }
                .background(G.console)
            }
        } else {
            GEmptyState(previewProblem == nil ? "No file selected" : "Can't preview", symbol: "doc.text",
                        detail: previewProblem ?? "Select a file to see what's in it.")
        }
    }

    // MARK: Actions

    private func activate(_ e: FileEntry) {
        switch e.kind {
        case .directory: open(join(path, e.name))
        case .symlink:
            // A link may point at a folder: try listing it, else preview it.
            let target = join(path, e.name)
            Task {
                if (try? await store.listFiles(in: container.id, path: target)) != nil { open(target) } else { await loadPreview(target) }
            }
        default: Task { await loadPreview(join(path, e.name)) }
        }
    }

    private func open(_ newPath: String) {
        selection = nil
        preview = nil
        previewProblem = nil
        path = newPath.isEmpty ? "/" : newPath
    }

    private func load() async {
        loading = true
        defer { loading = false }
        do {
            let result = try await store.listFiles(in: container.id, path: path)
            listing = result
            problem = nil
        } catch {
            listing = nil
            problem = error.localizedDescription
        }
    }

    private func loadPreview(_ file: String) async {
        do {
            preview = try await store.readFile(in: container.id, path: file)
            previewProblem = nil
        } catch {
            preview = nil
            previewProblem = error.localizedDescription
        }
    }

    private func icon(_ e: FileEntry) -> String {
        switch e.kind {
        case .directory: "folder.fill"
        case .symlink: "arrow.turn.up.right"
        case .other: "questionmark.square.dashed"
        case .file: e.mode & 0o111 != 0 ? "terminal" : "doc"
        }
    }

    private func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }

    // MARK: Upload & download

    private func pickAndUpload() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        panel.prompt = "Upload"
        panel.message = "Copy files or folders into \(container.name):\(path)"
        panel.begin { response in
            guard response == .OK, !panel.urls.isEmpty else { return }
            let urls = panel.urls
            Task { await upload(urls) }
        }
    }

    private func downloadSelection() {
        if let entry = selectedEntry {
            pickAndDownload(remotePath: join(path, entry.name), suggestedName: entry.name)
        } else {
            let suggested = path == "/" ? "\(container.name)-rootfs" : (path.split(separator: "/").last.map(String.init) ?? container.name)
            pickAndDownload(remotePath: path, suggestedName: suggested)
        }
    }

    private func pickAndDownload(remotePath: String, suggestedName: String) {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = suggestedName
        panel.canCreateDirectories = true
        panel.prompt = "Download"
        panel.message = "Save \(container.name):\(remotePath) to this Mac"
        panel.begin { response in
            guard response == .OK, let destination = panel.url else { return }
            Task { await download(remotePath: remotePath, to: destination) }
        }
    }

    private func handleDrop(_ providers: [NSItemProvider]) -> Bool {
        guard store.canTransferFiles, transferring == nil else { return false }
        let fileProviders = providers.filter { $0.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) }
        guard !fileProviders.isEmpty else { return false }

        let group = DispatchGroup()
        let collected = DropURLBox()
        for provider in fileProviders {
            group.enter()
            provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier, options: nil) { item, _ in
                defer { group.leave() }
                let resolved: URL? = switch item {
                case let data as Data: URL(dataRepresentation: data, relativeTo: nil)
                case let url as URL: url
                case let nsurl as NSURL: nsurl as URL
                default: nil
                }
                if let resolved, resolved.isFileURL {
                    collected.append(resolved)
                }
            }
        }
        group.notify(queue: .main) {
            let dropped = collected.all()
            guard !dropped.isEmpty else { return }
            Task { await upload(dropped) }
        }
        return true
    }

    private func upload(_ urls: [URL]) async {
        let targetDir = path
        transferBanner = nil
        transferring = urls.count == 1
            ? "Uploading \(urls[0].lastPathComponent) to \(targetDir)…"
            : "Uploading \(urls.count) items to \(targetDir)…"
        defer { transferring = nil }
        do {
            let count = try await store.uploadFiles(to: container.id, destinationDir: targetDir, urls: urls)
            await load()
            if let shown = preview?.path {
                await loadPreview(shown)
            }
            let label = count == 1 ? "1 item" : "\(count) items"
            transferBanner = (.success, "Uploaded \(label) to \(targetDir)")
        } catch {
            transferBanner = (.failure, error.localizedDescription)
        }
    }

    private func download(remotePath: String, to destination: URL) async {
        transferBanner = nil
        transferring = "Downloading \(remotePath)…"
        defer { transferring = nil }
        let tempTar = FileManager.default.temporaryDirectory.appending(path: "graviton-dl-\(UUID().uuidString).tar")
        defer { try? FileManager.default.removeItem(at: tempTar) }
        do {
            let (isDir, archiveName) = try await store.downloadArchive(from: container.id, path: remotePath, to: tempTar)
            try await Task.detached {
                try BuildUpload.extractArchive(tempTar, isDir: isDir, archiveName: archiveName, to: destination)
            }.value
            transferBanner = (.success, "Saved \(destination.lastPathComponent)")
        } catch {
            transferBanner = (.failure, error.localizedDescription)
        }
    }
}

private final class DropURLBox: @unchecked Sendable {
    private let lock = NSLock()
    private var urls: [URL] = []
    func append(_ url: URL) {
        lock.lock()
        urls.append(url)
        lock.unlock()
    }
    func all() -> [URL] {
        lock.lock()
        defer { lock.unlock() }
        return urls
    }
}

private func join(_ dir: String, _ name: String) -> String { dir == "/" ? "/" + name : dir + "/" + name }

private func parent(of path: String) -> String {
    let parts = path.split(separator: "/").dropLast()
    return "/" + parts.joined(separator: "/")
}

/// `xxd`-style dump of the first 512 bytes.
private func hexDump(_ bytes: [UInt8]) -> String {
    let shown = bytes.prefix(512)
    var lines: [String] = ["Binary file. First \(shown.count) bytes:", ""]
    for offset in stride(from: 0, to: shown.count, by: 16) {
        let row = Array(shown[(shown.startIndex + offset)..<min(shown.startIndex + offset + 16, shown.endIndex)])
        let hex = row.map { String(format: "%02x", $0) }.joined(separator: " ")
        let ascii = String(row.map { (0x20...0x7e).contains($0) ? Character(UnicodeScalar($0)) : "." })
        lines.append(String(format: "%08x  ", offset) + hex.padding(toLength: 48, withPad: " ", startingAt: 0) + "  " + ascii)
    }
    return lines.joined(separator: "\n")
}
