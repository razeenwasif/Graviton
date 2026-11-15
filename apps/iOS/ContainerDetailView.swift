import GravitonKit
import SwiftUI

struct ContainerDetailView: View {
    @Environment(EngineStore.self) private var store
    var containerID: String

    var body: some View {
        if let c = store.container(containerID) {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    VStack(alignment: .leading, spacing: 6) {
                        Text(c.name).font(.system(size: 28, weight: .bold)).kerning(-0.8)
                        HStack(spacing: 10) {
                            StatusPill(c, showUptime: true)
                            Text(c.image).font(G.mono(12)).foregroundStyle(G.muted).lineLimit(1)
                        }
                    }

                    HStack(spacing: 10) {
                        if c.isRunning {
                            ActionTile(title: "Stop", symbol: "stop.fill", tint: G.danger) { Task { await store.stop(c) } }
                            ActionTile(title: "Restart", symbol: "arrow.clockwise") { Task { await store.restart(c) } }
                        } else {
                            ActionTile(title: "Start", symbol: "play.fill", tint: G.running) { Task { await store.start(c) } }
                        }
                        NavigationLink {
                            MobileLogsView(containerID: c.id)
                        } label: {
                            ActionTileLabel(title: "Logs", symbol: "text.alignleft", tint: G.text)
                        }
                        .buttonStyle(.plain)
                        NavigationLink {
                            MobileShellView(container: c)
                        } label: {
                            ActionTileLabel(title: "Shell", symbol: "terminal", tint: G.text)
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
                    .background(G.card, in: RoundedRectangle(cornerRadius: 16, style: .continuous))

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
                        .background(G.card, in: RoundedRectangle(cornerRadius: 16, style: .continuous))

                        VStack(alignment: .leading, spacing: 8) {
                            Text("Limit").font(.system(size: 13)).foregroundStyle(G.muted)
                            Text(c.memoryLimitBytes.map(Format.bytes) ?? "—").font(.system(size: 20, weight: .semibold))
                        }
                        .padding(14)
                        .frame(maxWidth: .infinity, minHeight: 96, alignment: .topLeading)
                        .background(G.card, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
                    }

                    VStack(spacing: 0) {
                        DetailLink(title: "Stats", symbol: "chart.xyaxis.line", detail: "CPU, memory, network, processes") { MobileStatsView(container: c) }
                        Divider().overlay(G.border)
                        DetailLink(title: "Files", symbol: "folder", detail: c.isRunning ? "Browse and preview" : "Start it to browse", enabled: c.isRunning) { MobileFilesView(container: c) }
                        Divider().overlay(G.border)
                        DetailLink(title: "Inspect", symbol: "info.circle", detail: "Command, image, VM, environment") { MobileInspectView(container: c) }
                    }
                    .background(G.card, in: RoundedRectangle(cornerRadius: 16, style: .continuous))

                    VStack(spacing: 0) {
                        InfoRow(label: "Port", value: c.ports.first?.label ?? "—")
                        Divider().overlay(G.border)
                        InfoRow(label: "IP address", value: c.ipAddress ?? "—")
                        Divider().overlay(G.border)
                        InfoRow(label: "Mount", value: c.mounts.first?.source ?? "—")
                        Divider().overlay(G.border)
                        InfoRow(label: "ID", value: c.id)
                    }
                    .background(G.card, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)
            }
            .background(G.background)
            .navigationBarTitleDisplayMode(.inline)
        } else {
            ContentUnavailableView("Container not found", systemImage: "shippingbox")
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
        VStack(spacing: 6) {
            Image(systemName: symbol).font(.system(size: 18, weight: .medium))
            Text(title).font(.system(size: 12, weight: .medium))
        }
        .foregroundStyle(tint)
        .frame(maxWidth: .infinity, minHeight: 68)
        .background(G.card, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
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
                Image(systemName: symbol).foregroundStyle(G.accent).frame(width: 24)
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
        .buttonStyle(.plain)
        .disabled(!enabled)
        .opacity(enabled ? 1 : 0.45)
    }
}
