import GravitonKit
import SwiftUI

struct ContainerDetailView: View {
    @Environment(EngineStore.self) private var store
    var containerID: String
    @State private var showCommit = false

    var body: some View {
        if let c = store.container(containerID) {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    VStack(alignment: .leading, spacing: 8) {
                        Text(c.name).font(.system(size: 28, weight: .bold)).kerning(-0.8).foregroundStyle(G.text)
                        HStack(spacing: 10) {
                            StatusPill(c, showUptime: true)
                            Text(c.image).font(G.mono(12)).foregroundStyle(G.muted).lineLimit(1)
                        }
                    }

                    HStack(spacing: 10) {
                        if c.isRunning {
                            ActionTile(title: "Pause", symbol: "pause.fill", tint: G.warning) { Task { await store.pause(c) } }
                            ActionTile(title: "Restart", symbol: "arrow.clockwise", tint: G.accent) { Task { await store.restart(c) } }
                            ActionTile(title: "Stop", symbol: "stop.fill", tint: G.danger) { Task { await store.stop(c) } }
                        } else if c.isPaused {
                            ActionTile(title: "Resume", symbol: "play.fill", tint: G.running) { Task { await store.unpause(c) } }
                            ActionTile(title: "Restart", symbol: "arrow.clockwise", tint: G.accent) { Task { await store.restart(c) } }
                            ActionTile(title: "Stop", symbol: "stop.fill", tint: G.danger) { Task { await store.stop(c) } }
                        } else {
                            ActionTile(title: "Start", symbol: "play.fill", tint: G.running) { Task { await store.start(c) } }
                        }
                        NavigationLink {
                            MobileLogsView(containerID: c.id)
                        } label: {
                            ActionTileLabel(title: "Logs", symbol: "text.alignleft", tint: G.accent)
                        }
                        .buttonStyle(.plain)
                        NavigationLink {
                            MobileShellView(container: c)
                        } label: {
                            ActionTileLabel(title: "Shell", symbol: "terminal", tint: G.accent)
                        }
                        .buttonStyle(.plain)
                        .disabled(!c.isRunning)
                        .opacity(c.isRunning ? 1 : 0.45)
                    }

                    VStack(alignment: .leading, spacing: 10) {
                        HStack(alignment: .firstTextBaseline) {
                            Text("CPU").font(.system(size: 13)).foregroundStyle(G.muted)
                            Spacer()
                            MetricValue(c.cpuPercent.map { String(format: "%.1f", $0) } ?? "—", unit: c.cpuPercent == nil ? "" : "%", size: 22)
                        }
                        Sparkline(store.cpuHistory[c.id] ?? [], filled: true, lineWidth: 2).frame(height: 90)
                    }
                    .padding(16)
                    .mobileCard()

                    HStack(spacing: 10) {
                        VStack(alignment: .leading, spacing: 8) {
                            Text("Memory").font(.system(size: 13)).foregroundStyle(G.muted)
                            Text(c.memoryBytes.map(Format.bytes) ?? "—").font(.system(size: 20, weight: .semibold))
                            if let used = c.memoryBytes, let limit = c.memoryLimitBytes, limit > 0 {
                                MeterBar(Double(used) / Double(limit), color: G.running, height: 5)
                            }
                        }
                        .padding(14)
                        .frame(maxWidth: .infinity, minHeight: 96, alignment: .topLeading)
                        .mobileCard()

                        VStack(alignment: .leading, spacing: 8) {
                            Text("Limit").font(.system(size: 13)).foregroundStyle(G.muted)
                            Text(c.memoryLimitBytes.map(Format.bytes) ?? "—").font(.system(size: 20, weight: .semibold))
                        }
                        .padding(14)
                        .frame(maxWidth: .infinity, minHeight: 96, alignment: .topLeading)
                        .mobileCard()
                    }

                    VStack(spacing: 0) {
                        DetailLink(title: "Stats", symbol: "chart.xyaxis.line", detail: "CPU, memory, network, processes") { MobileStatsView(container: c) }
                        RowDivider()
                        DetailLink(title: "Files", symbol: "folder", detail: c.isRunning ? "Browse and preview" : "Start it to browse", enabled: c.isRunning) { MobileFilesView(container: c) }
                        RowDivider()
                        DetailLink(title: "Inspect", symbol: "info.circle", detail: "Command, image, VM, environment") { MobileInspectView(container: c) }
                    }
                    .mobileCard()

                    VStack(spacing: 0) {
                        if let project = c.project {
                            InfoRow(label: "Stack", value: c.service.map { "\(project) / \($0)" } ?? project)
                            RowDivider()
                        }
                        InfoRow(label: "Port", value: c.ports.first?.label ?? "—")
                        RowDivider()
                        InfoRow(label: "IP address", value: c.ipAddress ?? "—")
                        RowDivider()
                        InfoRow(label: "Mount", value: c.mounts.first?.source ?? "—")
                        RowDivider()
                        if let parentId = c.forkedFrom {
                            let parentName = store.container(parentId)?.name
                            InfoRow(label: "Forked from", value: parentName ?? String(parentId.prefix(12)))
                            RowDivider()
                        }
                        InfoRow(label: "ID", value: c.id)
                    }
                    .mobileCard()
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                MobileHeader("", actions: {
                    Menu {
                        Button("Fork Container", systemImage: "arrow.triangle.branch") {
                            Task { try? await store.fork(ForkSpec(source: c.id)) }
                        }
                        .disabled(!store.canManage)
                        Button("Commit to Image…", systemImage: "camera.aperture") { showCommit = true }
                            .disabled(!store.canManage)
                        Divider()
                        Button("Copy ID", systemImage: "doc.on.doc") { UIPasteboard.general.string = c.id }
                        Button("Copy Image", systemImage: "square.stack.3d.up") { UIPasteboard.general.string = c.image }
                    } label: {
                        Image(systemName: "ellipsis")
                    }
                    .buttonStyle(CircleIconStyle())
                    .accessibilityLabel("More")
                })
                .background(G.background)
            }
            .refreshable { await store.refresh() }
            .mobileScreen()
            .sheet(isPresented: $showCommit) {
                MobileCommitSheet(container: c)
            }
        } else {
            GEmptyState("Container not found", symbol: "shippingbox", detail: "It may have been removed.")
                .safeAreaInset(edge: .top, spacing: 0) { MobileHeader("") }
                .mobileScreen()
        }
    }
}

struct ActionTile: View {
    var title: String
    var symbol: String
    var tint: Color = G.text
    var enabled = true
    var action: () -> Void

    var body: some View {
        Button(action: action) { ActionTileLabel(title: title, symbol: symbol, tint: tint) }
            .buttonStyle(.plain)
            .disabled(!enabled)
            .opacity(enabled ? 1 : 0.45)
    }
}

struct ActionTileLabel: View {
    var title: String
    var symbol: String
    var tint: Color

    var body: some View {
        VStack(spacing: 7) {
            Image(systemName: symbol)
                .font(.system(size: 16, weight: .semibold))
                .frame(width: 36, height: 36)
                .background(tint.opacity(0.13), in: Circle())
            Text(title).font(.system(size: 12, weight: .medium)).foregroundStyle(G.textSecondary)
        }
        .foregroundStyle(tint)
        .frame(maxWidth: .infinity, minHeight: 80)
        .mobileCard(radius: 14)
        .contentShape(Rectangle())
    }
}

struct InfoRow: View {
    var label: String
    var value: String

    var body: some View {
        HStack {
            Text(label).foregroundStyle(G.muted)
            Spacer()
            Text(value).font(G.mono(13)).lineLimit(1).truncationMode(.middle)
        }
        .font(.system(size: 14))
        .padding(.horizontal, 16)
        .frame(minHeight: 48)
    }
}

/// A row that opens one of the container's screens.
struct DetailLink<Destination: View>: View {
    var title: String
    var symbol: String
    var detail: String
    var enabled = true
    @ViewBuilder var destination: () -> Destination

    var body: some View {
        NavigationLink(destination: destination) {
            HStack(spacing: 12) {
                Image(systemName: symbol)
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(G.accent)
                    .frame(width: 32, height: 32)
                    .background(G.accent.opacity(0.12), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).font(.system(size: 15, weight: .medium)).foregroundStyle(G.text)
                    Text(detail).font(.system(size: 12)).foregroundStyle(G.muted)
                }
                Spacer()
                Image(systemName: "chevron.right").font(.system(size: 12, weight: .semibold)).foregroundStyle(Color(hex: 0x5F6672))
            }
            .padding(.horizontal, 16)
            .frame(minHeight: 56)
            .contentShape(Rectangle())
        }
        .buttonStyle(PressRowStyle())
        .disabled(!enabled)
        .opacity(enabled ? 1 : 0.45)
    }
}
