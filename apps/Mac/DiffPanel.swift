import GravitonKit
import SwiftUI

/// Inspects live overlayfs upperdir filesystem changes (`Added`, `Modified`, `Deleted`) inside a running container.
struct DiffPanel: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    @State private var entries: [DiffEntry] = []
    @State private var filterKind: DiffKind?
    @State private var search = ""
    @State private var loading = false
    @State private var problem: String?

    private var visibleEntries: [DiffEntry] {
        let q = search.trimmingCharacters(in: .whitespaces)
        return entries.filter { entry in
            (filterKind == nil || entry.kind == filterKind)
                && (q.isEmpty || entry.path.localizedCaseInsensitiveContains(q))
        }
    }

    var body: some View {
        if !container.isRunning {
            GEmptyState(
                "Not running",
                symbol: "doc.text.magnifyingglass",
                detail: "Start \(container.name) to inspect its overlay filesystem changes.",
                actionTitle: "Start \(container.name)"
            ) {
                Task { await store.start(container) }
            }
        } else {
            VStack(alignment: .leading, spacing: 12) {
                toolbar
                if let problem {
                    GStatusLine(.failure, problem)
                } else if loading && entries.isEmpty {
                    GSpinner(size: 22).frame(maxWidth: .infinity, maxHeight: .infinity)
                } else if visibleEntries.isEmpty {
                    GEmptyState(
                        entries.isEmpty ? "No filesystem changes" : "No matching paths",
                        symbol: "checkmark.circle",
                        detail: entries.isEmpty
                            ? "\(container.name) hasn’t written any files above its base image."
                            : "Try clearing the filter or search query."
                    )
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(G.console, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                } else {
                    ScrollView {
                        LazyVStack(spacing: 2) {
                            ForEach(visibleEntries) { entry in
                                HStack(spacing: 10) {
                                    Text(entry.kind.marker)
                                        .font(G.mono(11, weight: .bold))
                                        .foregroundStyle(tint(for: entry.kind))
                                        .frame(width: 20, height: 20)
                                        .background(tint(for: entry.kind).opacity(0.14), in: RoundedRectangle(cornerRadius: 5, style: .continuous))
                                    Text(entry.path)
                                        .font(G.mono(12.5))
                                        .foregroundStyle(G.textPrimary)
                                        .lineLimit(1)
                                        .truncationMode(.middle)
                                        .textSelection(.enabled)
                                    Spacer()
                                    if let size = entry.sizeBytes {
                                        Text(Format.bytes(size))
                                            .font(G.mono(11.5))
                                            .foregroundStyle(G.textMuted)
                                    }
                                    Text(entry.kind.label)
                                        .font(.system(size: 11, weight: .medium))
                                        .foregroundStyle(tint(for: entry.kind))
                                        .frame(width: 60, alignment: .trailing)
                                }
                                .padding(.horizontal, 12)
                                .frame(height: 30)
                                .background(G.surfaceSubtle.opacity(0.45), in: RoundedRectangle(cornerRadius: 7, style: .continuous))
                            }
                        }
                        .padding(10)
                    }
                    .background(G.console, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                }
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 16)
            .task(id: "\(container.id)|\(container.isRunning)") {
                await reload()
            }
        }
    }

    private var toolbar: some View {
        HStack(spacing: 8) {
            GFilterPillButton("All (\(entries.count))", selected: filterKind == nil, accent: G.indigo) { filterKind = nil }
            GFilterPillButton("Added", selected: filterKind == .added, accent: G.mint) { filterKind = .added }
            GFilterPillButton("Modified", selected: filterKind == .modified, accent: G.amber) { filterKind = .modified }
            GFilterPillButton("Deleted", selected: filterKind == .deleted, accent: G.rose) { filterKind = .deleted }

            GTextField("Filter paths", text: $search)
                .gravitonField(mono: false, height: 30, icon: "magnifyingglass")
                .frame(maxWidth: 260)

            Spacer()

            Button {
                Task { await reload() }
            } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
            }
            .buttonStyle(.g(.secondary, size: .small))
            .disabled(loading)
        }
    }

    private func tint(for kind: DiffKind) -> Color {
        switch kind {
        case .added: G.mint
        case .modified: G.amber
        case .deleted: G.rose
        }
    }

    private func reload() async {
        loading = true
        defer { loading = false }
        do {
            entries = try await store.diff(id: container.id)
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }
}
