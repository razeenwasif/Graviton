import AppKit
import GravitonKit
import SwiftUI

/// The Files tab: browse a running container's filesystem (volumes included)
/// and preview files. Read-only.
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
    @State private var goTo = ""
    @State private var order = [KeyPathComparator(\FileEntry.name)]

    private var entries: [FileEntry] {
        let all = listing?.entries ?? []
        // Folders stay first whichever column sorts.
        return all.filter(\.isDirectory).sorted(using: order) + all.filter { !$0.isDirectory }.sorted(using: order)
    }

    var body: some View {
        if !container.isRunning {
            ContentUnavailableView("Not running", systemImage: "folder",
                                   description: Text("Start \(container.name) to browse its files."))
        } else {
            VStack(alignment: .leading, spacing: 12) {
                toolbar
                HSplitView {
                    list.frame(minWidth: 380, maxHeight: .infinity)
                    previewPane.frame(minWidth: 280, idealWidth: 420, maxHeight: .infinity)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .gravitonCard()
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
                .fixedSize()
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
            Spacer()
            TextField("Go to path", text: $goTo)
                .textFieldStyle(.roundedBorder)
                .font(G.mono(12))
                .frame(width: 200)
                .onSubmit {
                    let target = goTo.trimmingCharacters(in: .whitespaces)
                    guard !target.isEmpty else { return }
                    open(target.hasPrefix("/") ? target : join(path, target))
                    goTo = ""
                }
            Button { Task { await load() } } label: { Image(systemName: "arrow.clockwise") }
                .help("Refresh")
                .fixedSize()
        }
        .buttonStyle(QuietButtonStyle())
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
        Table(entries, selection: $selection, sortOrder: $order) {
            TableColumn("Name", value: \.name) { e in
                HStack(spacing: 7) {
                    Image(systemName: icon(e)).foregroundStyle(e.isDirectory ? Color(hex: 0x6E7EF2) : G.muted).frame(width: 16)
                    Text(e.name).lineLimit(1)
                    if let target = e.linkTarget {
                        Text("→ \(target)").foregroundStyle(G.faint).lineLimit(1).truncationMode(.middle)
                    }
                }
            }
            TableColumn("Size", value: \.size) { e in
                Text(e.isDirectory ? "—" : Format.bytes(e.size)).foregroundStyle(G.muted).monospacedDigit()
            }
            .width(72)
            TableColumn("Modified", value: \.modified) { e in
                Text(e.modifiedDate, format: .dateTime.day().month(.abbreviated).year().hour().minute()).foregroundStyle(G.muted)
            }
            .width(min: 110, ideal: 140)
            TableColumn("Permissions", value: \.mode) { e in
                Text(e.permissions).font(G.mono(11.5)).foregroundStyle(G.muted)
            }
            .width(90)
        }
        .contextMenu(forSelectionType: FileEntry.ID.self) { ids in
            if let name = ids.first {
                Button("Copy Path") { copy(join(path, name)) }
            }
        } primaryAction: { ids in
            if let name = ids.first, let e = entries.first(where: { $0.id == name }) { activate(e) }
        }
        .onChange(of: selection) { _, name in
            guard let name, let e = entries.first(where: { $0.id == name }) else { return }
            if !e.isDirectory { Task { await loadPreview(join(path, e.name)) } }
        }
        .overlay {
            if let problem {
                ContentUnavailableView("Can't open \(path)", systemImage: "exclamationmark.triangle", description: Text(problem))
            } else if listing?.entries.isEmpty == true {
                Text("Empty folder").foregroundStyle(G.faint)
            } else if loading && listing == nil {
                ProgressView().controlSize(.small)
            }
        }
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
                    if let text = preview.text {
                        Button("Copy") { copy(text) }.buttonStyle(QuietButtonStyle()).fixedSize()
                    }
                }
                .padding(12)
                Divider().overlay(G.border)
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
            ContentUnavailableView(previewProblem == nil ? "No file selected" : "Can't preview", systemImage: "doc.text",
                                   description: Text(previewProblem ?? "Select a file to see what's in it."))
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
