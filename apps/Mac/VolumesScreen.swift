import AppKit
import GravitonKit
import SwiftUI

struct VolumesScreen: View {
    @Environment(EngineStore.self) private var store
    @Environment(AppRouter.self) private var router
    @Environment(Dialogs.self) private var dialogs
    @State private var tab: Tab = .named
    @State private var search = ""
    @State private var selection: VolumeInfo.ID?
    @State private var showCreate = false
    @State private var refreshing = false
    @State private var width: CGFloat = 1200

    enum Tab: String, CaseIterable, Identifiable {
        case named = "Named", unused = "Unused", bind = "Bind Mounts"
        var id: String { rawValue }
    }

    private var totalBytes: UInt64 { store.volumes.map(\.sizeBytes).reduce(0, +) }
    private var unusedVolumes: [VolumeInfo] { store.volumes.filter { !$0.isInUse } }
    private var attachedVolumesCount: Int { store.volumes.count - unusedVolumes.count }

    private var shownVolumes: [VolumeInfo] {
        let base = tab == .unused ? unusedVolumes : store.volumes
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !q.isEmpty else { return base }
        return base.filter { v in
            v.name.lowercased().contains(q)
                || v.path.lowercased().contains(q)
                || v.usedBy.contains { $0.containerName.lowercased().contains(q) || $0.target.lowercased().contains(q) }
        }
    }

    private var bindMounts: [BindMountRow] {
        let all = store.containers.flatMap { c in
            c.mounts.filter { $0.source.hasPrefix("/") || $0.source.hasPrefix("~") }.map { m in
                BindMountRow(container: c, mount: m)
            }
        }
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !q.isEmpty else { return all }
        return all.filter {
            $0.mount.source.lowercased().contains(q)
                || $0.mount.target.lowercased().contains(q)
                || $0.container.name.lowercased().contains(q)
        }
    }

    private var selectedVolume: VolumeInfo? {
        guard let selection else { return nil }
        return store.volumes.first { $0.id == selection }
    }

    private var subtitle: String {
        let count = store.volumes.count
        let noun = count == 1 ? "named volume" : "named volumes"
        var parts = ["\(count) \(noun)", "\(Format.bytes(totalBytes)) on disk"]
        if !bindMounts.isEmpty && search.isEmpty {
            parts.append("\(bindMounts.count) bind \(bindMounts.count == 1 ? "mount" : "mounts")")
        }
        return parts.joined(separator: " · ")
    }

    var body: some View {
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                ScreenHeader("Volumes", subtitle: subtitle) {
                    HeaderSearch(prompt: "Search volumes", text: $search)
                    Button {
                        Task { await reload() }
                    } label: {
                        Image(systemName: "arrow.clockwise")
                    }
                    .buttonStyle(.gIconBordered)
                    .disabled(refreshing)
                    .help("Recalculate volume sizes")
                    Button {
                        showCreate = true
                    } label: {
                        Label("New volume", systemImage: "plus")
                    }
                    .buttonStyle(.gPrimary)
                    .keyboardShortcut("n")
                    .help("Create a named volume (⌘N)")
                }

                VStack(alignment: .leading, spacing: 18) {
                    kpiStrip

                    HStack(spacing: 8) {
                        GFilterPillButton("Named", count: store.volumes.count, selected: tab == .named, accent: G.amber) {
                            withAnimation(.snappy(duration: 0.18)) { tab = .named }
                        }
                        GFilterPillButton("Unused", count: unusedVolumes.count, selected: tab == .unused, accent: G.amber) {
                            withAnimation(.snappy(duration: 0.18)) { tab = .unused }
                        }
                        GFilterPillButton("Bind Mounts", count: allBindCount, selected: tab == .bind, accent: G.indigo) {
                            withAnimation(.snappy(duration: 0.18)) { tab = .bind }
                        }
                        Spacer()
                    }

                    if tab == .bind {
                        bindContent
                    } else {
                        namedContent
                    }
                }
                .padding(.horizontal, 28)
                .padding(.top, 4)
                .padding(.bottom, 22)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(G.bg)

            if tab != .bind, width >= 980, let vol = selectedVolume {
                VolumeInspector(volume: vol, onRemove: { confirmRemove(vol) }, onOpenContainer: openContainer)
                    .frame(width: 344)
                    .transition(.move(edge: .trailing).combined(with: .opacity))
            }
        }
        .animation(.snappy(duration: 0.2), value: selection)
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width = $0 }
        .task { await reload() }
        .onChange(of: store.volumes.map(\.id)) {
            if let selection, !store.volumes.contains(where: { $0.id == selection }) {
                self.selection = store.volumes.first?.id
            } else if selection == nil {
                selection = store.volumes.first?.id
            }
        }
        .sheet(isPresented: $showCreate) {
            CreateVolumeSheet { created in
                tab = .named
                selection = created.id
            }
        }
    }

    private var kpiStrip: some View {
        HStack(spacing: 36) {
            GMetricStatColumn(
                label: "Volumes",
                value: "\(store.volumes.count)",
                subtitle: "\(attachedVolumesCount) attached · \(unusedVolumes.count) unused",
                valueColor: G.amber
            )
            GMetricStatColumn(
                label: "Total Size",
                value: Format.bytes(totalBytes),
                subtitle: "persistent storage",
                valueColor: G.indigo
            )
            GMetricStatColumn(
                label: "Bind Mounts",
                value: "\(allBindCount)",
                subtitle: "host directories",
                valueColor: G.sky
            )
            Spacer()
        }
        .padding(.vertical, 14)
        .padding(.horizontal, 18)
        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }

    private var allBindCount: Int {
        store.containers.reduce(0) { sum, c in
            sum + c.mounts.filter { $0.source.hasPrefix("/") || $0.source.hasPrefix("~") }.count
        }
    }

    @ViewBuilder private var namedContent: some View {
        if store.volumes.isEmpty {
            GEmptyState("No named volumes yet", symbol: "internaldrive",
                        detail: "Named volumes keep data across container restarts and removals (-v name:/path). Create one here or pass one in grav run.",
                        actionTitle: "New volume") { showCreate = true }
                .gravitonCard(G.surfaceSubtle)
        } else if shownVolumes.isEmpty {
            GEmptyState("Nothing matches", symbol: "magnifyingglass",
                        detail: search.isEmpty ? "Every named volume is currently attached to a container." : "No volume matches “\(search)”.")
                .gravitonCard(G.surfaceSubtle)
        } else {
            VolumeTable(rows: shownVolumes, selection: $selection, onRemove: confirmRemove, onOpenContainer: openContainer)
        }
    }

    @ViewBuilder private var bindContent: some View {
        if allBindCount == 0 {
            GEmptyState("No bind mounts", symbol: "folder.badge.gearshape",
                        detail: "Share a Mac folder into a container with -v ~/code:/app or from the Run sheet.")
                .gravitonCard(G.surfaceSubtle)
        } else if bindMounts.isEmpty {
            GEmptyState("Nothing matches", symbol: "magnifyingglass", detail: "No bind mount matches “\(search)”.")
                .gravitonCard(G.surfaceSubtle)
        } else {
            BindMountTable(rows: bindMounts, onOpenContainer: openContainer)
        }
    }

    private func reload() async {
        refreshing = true
        defer { refreshing = false }
        await store.loadVolumes()
        if selection == nil { selection = store.volumes.first?.id }
    }

    private func openContainer(_ id: String) {
        router.pending = .container(id, .inspect)
    }

    private func confirmRemove(_ volume: VolumeInfo) {
        dialogs.confirm("Remove volume \(volume.name)?",
                        message: "All \(volume.fileCount == 1 ? "1 file" : "\(volume.fileCount) files") (\(Format.bytes(volume.sizeBytes))) inside \(volume.name) will be deleted permanently.",
                        confirm: "Remove") {
            Task {
                do {
                    _ = try await store.removeVolume(volume)
                } catch {
                    dialogs.alert("Couldn't remove \(volume.name)", message: error.localizedDescription)
                }
            }
        }
    }
}

// MARK: - Named volumes table

private struct VolumeTable: View {
    @Environment(EngineStore.self) private var store
    var rows: [VolumeInfo]
    @Binding var selection: VolumeInfo.ID?
    var onRemove: (VolumeInfo) -> Void
    var onOpenContainer: (String) -> Void
    @State private var sort: Column = .name
    @State private var ascending = true
    @FocusState private var focused: Bool

    enum Column: Hashable { case name, usedBy, files, size, created }

    private var sorted: [VolumeInfo] {
        let out = rows.sorted { a, b in
            switch sort {
            case .name: a.name.localizedStandardCompare(b.name) == .orderedAscending
            case .usedBy: a.usedBy.count < b.usedBy.count
            case .files: a.fileCount < b.fileCount
            case .size: a.sizeBytes < b.sizeBytes
            case .created: a.createdAt < b.createdAt
            }
        }
        return ascending ? out : out.reversed()
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                GColumnHeader("Volume", key: Column.name, sort: $sort, ascending: $ascending).frame(minWidth: 140, maxWidth: .infinity)
                GColumnHeader("Used By", key: Column.usedBy, sort: $sort, ascending: $ascending).frame(minWidth: 140, maxWidth: .infinity)
                GColumnHeader("Files", key: Column.files, sort: $sort, ascending: $ascending, alignment: .trailing).frame(width: 64)
                GColumnHeader("Size", key: Column.size, sort: $sort, ascending: $ascending, alignment: .trailing).frame(width: 82)
                GColumnHeader("Created", key: Column.created, sort: $sort, ascending: $ascending).frame(width: 118)
                Color.clear.frame(width: 28)
            }
            .padding(.horizontal, 12)
            .frame(height: 30)

            ScrollView {
                LazyVStack(spacing: 3) {
                    ForEach(sorted) { v in
                        row(v)
                    }
                }
                .padding(.vertical, 4)
            }
        }
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        .onKeyPress(.downArrow) { move(1) }
        .onKeyPress(.upArrow) { move(-1) }
    }

    private func row(_ v: VolumeInfo) -> some View {
        HStack(spacing: 12) {
            HStack(spacing: 10) {
                Image(systemName: "internaldrive")
                    .font(.system(size: 13))
                    .foregroundStyle(v.isInUse ? G.amber : G.textMuted)
                    .frame(width: 20)
                VStack(alignment: .leading, spacing: 2) {
                    Text(v.name).font(.system(size: 13.5, weight: .semibold)).foregroundStyle(G.textPrimary).lineLimit(1)
                    Text(v.path).font(G.mono(11)).foregroundStyle(G.textTertiary).lineLimit(1).truncationMode(.middle)
                }
            }
            .frame(minWidth: 140, maxWidth: .infinity, alignment: .leading)

            Group {
                if v.usedBy.isEmpty {
                    Text("Unused").font(.system(size: 12)).foregroundStyle(G.textTertiary)
                } else {
                    HStack(spacing: 6) {
                        ForEach(v.usedBy.prefix(2)) { ref in
                            Button { onOpenContainer(ref.containerId) } label: {
                                HStack(spacing: 5) {
                                    Circle().fill(ref.running ? G.mint : G.slate).frame(width: 5.5, height: 5.5)
                                    Text(ref.containerName).font(.system(size: 11.5, weight: .medium)).lineLimit(1)
                                }
                                .foregroundStyle(G.body)
                                .padding(.horizontal, 8)
                                .frame(height: 22)
                                .background(G.surfacePill, in: Capsule())
                            }
                            .buttonStyle(.plain)
                            .help("\(ref.containerName) → \(ref.target)\(ref.readOnly ? " (read-only)" : "")")
                        }
                        if v.usedBy.count > 2 {
                            Text("+\(v.usedBy.count - 2)").font(.system(size: 11)).foregroundStyle(G.textMuted)
                        }
                    }
                }
            }
            .frame(minWidth: 140, maxWidth: .infinity, alignment: .leading)

            Text("\(v.fileCount)").font(G.mono(12)).foregroundStyle(G.textSecondary).monospacedDigit()
                .frame(width: 64, alignment: .trailing)
            Text(Format.bytes(v.sizeBytes)).font(G.mono(12)).foregroundStyle(G.textSecondary).monospacedDigit()
                .frame(width: 82, alignment: .trailing)
            Text(v.createdAt > 0 ? v.createdDate.formatted(.dateTime.day().month(.abbreviated).hour().minute()) : "—")
                .font(.system(size: 12)).foregroundStyle(G.textMuted).lineLimit(1)
                .frame(width: 118, alignment: .leading)

            Menu {
                menuItems(v)
            } label: {
                Image(systemName: "ellipsis")
                    .font(.system(size: 12, weight: .bold))
                    .foregroundStyle(G.textMuted)
                    .frame(width: 26, height: 26)
                    .contentShape(Rectangle())
            }
            .gravitonMenu()
            .frame(width: 28)
            .accessibilityLabel("More actions for \(v.name)")
        }
        .padding(.horizontal, 12)
        .frame(height: 52)
        .gravitonRow(selected: selection == v.id)
        .simultaneousGesture(TapGesture().onEnded {
            selection = v.id
            focused = true
        })
        .contextMenu { menuItems(v) }
    }

    @ViewBuilder private func menuItems(_ v: VolumeInfo) -> some View {
        Button("Copy Name") { copyText(v.name) }
        Button("Copy Mount Flag (-v \(v.name):/data)") { copyText("-v \(v.name):/data") }
        Button("Copy Path") { copyText(v.path) }
        if store.isLocal && store.connection == .live {
            Button("Show in Finder") {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: v.path)])
            }
        }
        Divider()
        Button("Remove…", role: .destructive) { onRemove(v) }
            .disabled(v.isInUse)
    }

    private func move(_ by: Int) -> KeyPress.Result {
        let list = sorted
        guard !list.isEmpty else { return .ignored }
        let index = list.firstIndex { $0.id == selection } ?? (by > 0 ? -1 : list.count)
        selection = list[min(max(index + by, 0), list.count - 1)].id
        return .handled
    }
}

// MARK: - Volume inspector

private struct VolumeInspector: View {
    @Environment(EngineStore.self) private var store
    var volume: VolumeInfo
    var onRemove: () -> Void
    var onOpenContainer: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(alignment: .top, spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(volume.name).font(.system(size: 16, weight: .semibold)).foregroundStyle(G.textPrimary).lineLimit(1)
                    Text("Named volume").font(.system(size: 12)).foregroundStyle(G.textMuted)
                }
                Spacer()
                GTagPill(volume.isInUse ? "In use" : "Unused", color: volume.isInUse ? G.mint : G.textMuted)
            }

            HStack(spacing: 10) {
                statTile("SIZE", Format.bytes(volume.sizeBytes))
                statTile("FILES", "\(volume.fileCount)")
            }

            VStack(alignment: .leading, spacing: 6) {
                GCaption("Mount Flag")
                HStack(spacing: 8) {
                    Text("-v \(volume.name):/data")
                        .font(G.mono(12))
                        .foregroundStyle(G.body)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .textSelection(.enabled)
                    Spacer(minLength: 4)
                    Button { copyText("-v \(volume.name):/data") } label: { Image(systemName: "doc.on.doc") }
                        .buttonStyle(GIconButtonStyle(size: 24))
                        .help("Copy mount flag")
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .background(G.bg, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            }

            GKeyValueCard("Storage") {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Host Path").font(.system(size: 11)).foregroundStyle(G.textMuted)
                    Text(volume.path)
                        .font(G.mono(11.5))
                        .foregroundStyle(G.textSecondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            VStack(alignment: .leading, spacing: 8) {
                GCaption("Attached Containers")
                if volume.usedBy.isEmpty {
                    Text("No containers mount this volume. It can be safely removed.")
                        .font(.system(size: 12.5))
                        .foregroundStyle(G.textTertiary)
                        .fixedSize(horizontal: false, vertical: true)
                } else {
                    VStack(spacing: 6) {
                        ForEach(volume.usedBy) { ref in
                            Button { onOpenContainer(ref.containerId) } label: {
                                HStack(spacing: 8) {
                                    StatusDot(running: ref.running, size: 7)
                                    VStack(alignment: .leading, spacing: 1) {
                                        Text(ref.containerName).font(.system(size: 12.5, weight: .medium)).foregroundStyle(G.textPrimary)
                                        Text(ref.target + (ref.readOnly ? " (read-only)" : ""))
                                            .font(G.mono(11)).foregroundStyle(G.textMuted).lineLimit(1).truncationMode(.middle)
                                    }
                                    Spacer()
                                    Image(systemName: "chevron.right").font(.system(size: 10, weight: .semibold)).foregroundStyle(G.textTertiary)
                                }
                                .padding(.horizontal, 11)
                                .padding(.vertical, 8)
                                .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                            }
                            .buttonStyle(.plain)
                        }
                    }
                }
            }

            Spacer(minLength: 0)

            HStack(spacing: 8) {
                if store.isLocal && store.connection == .live {
                    Button("Finder", systemImage: "folder") {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: volume.path)])
                    }
                    .buttonStyle(.gSecondary)
                }
                Spacer()
                Button("Remove", systemImage: "trash", action: onRemove)
                    .buttonStyle(.g(.danger))
                    .disabled(volume.isInUse)
                    .help(volume.isInUse ? "Remove the containers using this volume first" : "Delete \(volume.name) from disk")
            }
        }
        .padding(20)
        .padding(.top, 8)
        .frame(maxHeight: .infinity, alignment: .top)
        .background(G.inspector)
    }

    private func statTile(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label).font(.system(size: 10, weight: .semibold)).tracking(0.55).foregroundStyle(G.textMuted)
            Text(value).font(G.statValue(18)).foregroundStyle(G.textPrimary)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }
}

// MARK: - Bind mounts table

private struct BindMountRow: Identifiable {
    let container: Container
    let mount: Mount
    var id: String { "\(container.id)|\(mount.source)|\(mount.target)" }
}

private struct BindMountTable: View {
    @Environment(EngineStore.self) private var store
    var rows: [BindMountRow]
    var onOpenContainer: (String) -> Void
    @State private var sort: Column = .source
    @State private var ascending = true

    enum Column: Hashable { case source, container, target, access }

    private var sorted: [BindMountRow] {
        let out = rows.sorted { a, b in
            switch sort {
            case .source: a.mount.source.localizedStandardCompare(b.mount.source) == .orderedAscending
            case .container: a.container.name.localizedStandardCompare(b.container.name) == .orderedAscending
            case .target: a.mount.target.localizedStandardCompare(b.mount.target) == .orderedAscending
            case .access: (!a.mount.readOnly && b.mount.readOnly)
            }
        }
        return ascending ? out : out.reversed()
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                GColumnHeader("Host Folder", key: Column.source, sort: $sort, ascending: $ascending).frame(minWidth: 160, maxWidth: .infinity)
                GColumnHeader("Container", key: Column.container, sort: $sort, ascending: $ascending).frame(width: 150)
                GColumnHeader("Mount Point", key: Column.target, sort: $sort, ascending: $ascending).frame(minWidth: 120, maxWidth: .infinity)
                GColumnHeader("Access", key: Column.access, sort: $sort, ascending: $ascending).frame(width: 90)
                Color.clear.frame(width: 28)
            }
            .padding(.horizontal, 12)
            .frame(height: 30)

            ScrollView {
                LazyVStack(spacing: 3) {
                    ForEach(sorted) { item in
                        HStack(spacing: 12) {
                            HStack(spacing: 8) {
                                Image(systemName: "folder").font(.system(size: 12)).foregroundStyle(G.indigo).frame(width: 16)
                                Text(item.mount.source).font(G.mono(12)).foregroundStyle(G.textPrimary).lineLimit(1).truncationMode(.middle)
                            }
                            .frame(minWidth: 160, maxWidth: .infinity, alignment: .leading)

                            Button { onOpenContainer(item.container.id) } label: {
                                HStack(spacing: 6) {
                                    StatusDot(running: item.container.isRunning, size: 7)
                                    Text(item.container.name).font(.system(size: 12.5, weight: .medium)).foregroundStyle(G.textPrimary).lineLimit(1)
                                }
                            }
                            .buttonStyle(.plain)
                            .frame(width: 150, alignment: .leading)

                            Text(item.mount.target).font(G.mono(12)).foregroundStyle(G.textSecondary).lineLimit(1).truncationMode(.middle)
                                .frame(minWidth: 120, maxWidth: .infinity, alignment: .leading)

                            GTagPill(item.mount.readOnly ? "Read-only" : "Read-write", color: item.mount.readOnly ? G.amber : G.textSecondary)
                                .frame(width: 90, alignment: .leading)

                            Menu {
                                Button("Open \(item.container.name)") { onOpenContainer(item.container.id) }
                                Button("Copy Host Path") { copyText(item.mount.source) }
                                if store.isLocal && store.connection == .live {
                                    Button("Show in Finder") {
                                        let expanded = (item.mount.source as NSString).expandingTildeInPath
                                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: expanded)])
                                    }
                                }
                            } label: {
                                Image(systemName: "ellipsis").font(.system(size: 12, weight: .bold)).foregroundStyle(G.textMuted)
                                    .frame(width: 26, height: 26).contentShape(Rectangle())
                            }
                            .gravitonMenu()
                            .frame(width: 28)
                        }
                        .padding(.horizontal, 12)
                        .frame(height: 46)
                        .gravitonRow(selected: false)
                        .onTapGesture(count: 2) { onOpenContainer(item.container.id) }
                    }
                }
                .padding(.vertical, 4)
            }
        }
    }
}

// MARK: - Create volume sheet

private struct CreateVolumeSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    var onCreated: (VolumeInfo) -> Void
    @State private var name = ""
    @State private var working = false
    @State private var problem: String?

    var body: some View {
        GSheet("New volume", subtitle: "Creates a persistent folder under Graviton's volumes directory. Mount it with -v name:/path.", symbol: "internaldrive", width: 460) {
            GFormRow("Name", hint: "Start with a letter or digit; letters, digits, _, . and - are allowed.") {
                GTextField("e.g. pgdata", text: $name)
                    .disabled(working)
                    .onSubmit(create)
                    .gravitonField(mono: true)
            }
            if working {
                GStatusLine(.working, "Creating \(name)…")
            } else if let problem {
                GStatusLine(.failure, problem)
            }
        } footer: {
            Spacer()
            Button("Cancel") { dismiss() }.buttonStyle(.gSecondary).keyboardShortcut(.cancelAction)
            Button("Create", action: create)
                .buttonStyle(.gPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(working || name.trimmingCharacters(in: .whitespaces).isEmpty)
        }
    }

    private func create() {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty, !working else { return }
        working = true
        problem = nil
        Task {
            do {
                let vol = try await store.createVolume(name: trimmed)
                onCreated(vol)
                dismiss()
            } catch {
                working = false
                problem = error.localizedDescription
            }
        }
    }
}

private func copyText(_ text: String) {
    NSPasteboard.general.clearContents()
    NSPasteboard.general.setString(text, forType: .string)
}
