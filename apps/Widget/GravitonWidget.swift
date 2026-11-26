import GravitonKit
import SwiftUI
import WidgetKit

@main
struct GravitonWidgetsBundle: WidgetBundle {
    var body: some Widget {
        GravitonStatusWidget()
    }
}

struct GravitonEntry: TimelineEntry {
    let date: Date
    let snapshot: GravitonWidgetSnapshot
    let isPlaceholder: Bool
}

struct GravitonProvider: TimelineProvider {
    func placeholder(in context: Context) -> GravitonEntry {
        GravitonEntry(date: .now, snapshot: .placeholder, isPlaceholder: true)
    }

    func getSnapshot(in context: Context, completion: @escaping (GravitonEntry) -> Void) {
        let snap = GravitonWidgetStore.load() ?? .placeholder
        completion(GravitonEntry(date: .now, snapshot: snap, isPlaceholder: context.isPreview && GravitonWidgetStore.load() == nil))
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<GravitonEntry>) -> Void) {
        let snap = GravitonWidgetStore.load() ?? .placeholder
        let entry = GravitonEntry(date: .now, snapshot: snap, isPlaceholder: false)
        let next = Calendar.current.date(byAdding: .minute, value: 5, to: .now) ?? Date(timeIntervalSinceNow: 300)
        completion(Timeline(entries: [entry], policy: .after(next)))
    }
}

struct GravitonStatusWidget: Widget {
    let kind = "dev.graviton.status"

    var body: some WidgetConfiguration {
        StaticConfiguration(kind: kind, provider: GravitonProvider()) { entry in
            GravitonWidgetRoot(entry: entry)
        }
        .configurationDisplayName("Graviton Runtime")
        .description("Live container VMs, CPU load, and memory usage across your Macs.")
        #if os(iOS)
        .supportedFamilies([.systemSmall, .systemMedium, .systemLarge, .accessoryCircular, .accessoryRectangular])
        #else
        .supportedFamilies([.systemSmall, .systemMedium, .systemLarge])
        #endif
    }
}

// MARK: - Root & Background

private struct GravitonWidgetRoot: View {
    @Environment(\.widgetFamily) private var family
    let entry: GravitonEntry

    var body: some View {
        let snap = entry.snapshot
        Group {
            switch family {
            case .systemSmall:
                SmallRuntimeWidget(snap: snap)
            case .systemMedium:
                MediumRuntimeWidget(snap: snap)
            case .systemLarge:
                LargeRuntimeWidget(snap: snap)
            #if os(iOS)
            case .accessoryCircular:
                LockCircularWidget(snap: snap)
            case .accessoryRectangular:
                LockRectangularWidget(snap: snap)
            #endif
            default:
                SmallRuntimeWidget(snap: snap)
            }
        }
        .containerBackground(for: .widget) {
            #if os(iOS)
            if family == .accessoryCircular || family == .accessoryRectangular {
                Color.clear
            } else {
                obsidianBackground
            }
            #else
            obsidianBackground
            #endif
        }
    }

    private var obsidianBackground: some View {
        ZStack {
            LinearGradient(
                colors: [Color(hex: 0x06070C), Color(hex: 0x000000)],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
            RadialGradient(
                colors: [G.indigo.opacity(0.16), .clear],
                center: .topLeading,
                startRadius: 2,
                endRadius: 190
            )
            RadialGradient(
                colors: [G.sky.opacity(0.08), .clear],
                center: .bottomTrailing,
                startRadius: 4,
                endRadius: 170
            )
        }
    }
}

// MARK: - Small Widget

private struct SmallRuntimeWidget: View {
    let snap: GravitonWidgetSnapshot

    private var statusColor: Color {
        if snap.online { return G.mint }
        return snap.isDemo ? G.amber : G.rose
    }

    private var statusLabel: String {
        if snap.online { return "LIVE" }
        return snap.isDemo ? "DEMO" : "OFFLINE"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                GravitonMark(size: 24)
                Spacer()
                HStack(spacing: 4) {
                    Circle().fill(statusColor).frame(width: 5.5, height: 5.5)
                    Text(statusLabel)
                        .font(G.mono(9.5, weight: .bold))
                        .foregroundStyle(statusColor)
                }
                .padding(.horizontal, 7)
                .frame(height: 18)
                .background(statusColor.opacity(0.14), in: Capsule())
            }

            Spacer(minLength: 6)

            HStack(alignment: .firstTextBaseline, spacing: 4) {
                Text("\(snap.runningCount)")
                    .font(G.heroKPI(34))
                    .foregroundStyle(snap.runningCount > 0 ? G.textPrimary : G.textSecondary)
                Text("/ \(snap.totalContainers)")
                    .font(G.mono(13, weight: .medium))
                    .foregroundStyle(G.textMuted)
            }
            Text(snap.runningCount == 1 ? "container running" : "containers running")
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(G.textMuted)
                .lineLimit(1)

            Spacer(minLength: 8)

            StackedMeterBar([
                .init(snap.vmMemoryFraction, color: G.indigo),
                .init(snap.otherHostMemoryFraction, color: G.slate),
            ], height: 4)

            HStack {
                Text(snap.hostName)
                    .font(.system(size: 10.5, weight: .medium))
                    .foregroundStyle(G.textSecondary)
                    .lineLimit(1)
                Spacer()
                Text(Format.percent(snap.totalCPU))
                    .font(G.mono(10.5, weight: .semibold))
                    .foregroundStyle(G.indigo)
            }
            .padding(.top, 7)
        }
        .widgetURL(URL(string: "graviton://containers"))
    }
}

// MARK: - Medium Widget

private struct MediumRuntimeWidget: View {
    let snap: GravitonWidgetSnapshot

    private var statusColor: Color {
        if snap.online { return G.mint }
        return snap.isDemo ? G.amber : G.rose
    }

    var body: some View {
        HStack(spacing: 14) {
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 7) {
                    GravitonMark(size: 22)
                    VStack(alignment: .leading, spacing: 0) {
                        Text("Graviton")
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundStyle(G.textPrimary)
                        HStack(spacing: 4) {
                            Circle().fill(statusColor).frame(width: 5, height: 5)
                            Text(snap.hostName)
                                .font(.system(size: 10))
                                .foregroundStyle(G.textMuted)
                                .lineLimit(1)
                        }
                    }
                }

                Spacer(minLength: 6)

                HStack(alignment: .firstTextBaseline, spacing: 3) {
                    Text("\(snap.runningCount)")
                        .font(G.heroKPI(28))
                        .foregroundStyle(snap.runningCount > 0 ? G.mint : G.textPrimary)
                    Text("/ \(snap.totalContainers) VMs")
                        .font(G.mono(11))
                        .foregroundStyle(G.textMuted)
                }

                Spacer(minLength: 6)

                HStack(spacing: 10) {
                    statChip("CPU", Format.percent(snap.totalCPU), G.indigo)
                    statChip("MEM", Format.bytes(snap.vmMemoryUsedBytes), G.sky)
                }
            }
            .frame(width: 132, alignment: .leading)

            VStack(alignment: .leading, spacing: 5) {
                if snap.containers.isEmpty {
                    VStack(spacing: 6) {
                        Image(systemName: "shippingbox")
                            .font(.system(size: 16))
                            .foregroundStyle(G.textTertiary)
                        Text("No containers")
                            .font(.system(size: 11.5))
                            .foregroundStyle(G.textMuted)
                    }
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(Color.white.opacity(0.03), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                } else {
                    ForEach(snap.containers.prefix(4)) { item in
                        Link(destination: URL(string: "graviton://container/\(item.id)") ?? URL(string: "graviton://containers")!) {
                            containerRow(item)
                        }
                    }
                    Spacer(minLength: 0)
                }
            }
            .frame(maxWidth: .infinity, alignment: .topLeading)
        }
    }

    private func statChip(_ label: String, _ value: String, _ color: Color) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label)
                .font(.system(size: 8.5, weight: .semibold))
                .tracking(0.5)
                .foregroundStyle(G.textTertiary)
            Text(value)
                .font(G.mono(11, weight: .semibold))
                .foregroundStyle(color)
                .lineLimit(1)
        }
    }

    private func containerRow(_ c: WidgetContainerItem) -> some View {
        HStack(spacing: 7) {
            Circle()
                .fill(c.status == .running ? G.mint : (c.status == .paused ? G.amber : G.slate))
                .frame(width: 6, height: 6)
            Text(c.name)
                .font(.system(size: 11.5, weight: .semibold))
                .foregroundStyle(G.textPrimary)
                .lineLimit(1)
            Spacer(minLength: 4)
            if let port = c.portLabel, c.status == .running {
                Text(port)
                    .font(G.mono(10))
                    .foregroundStyle(G.sky)
            }
            if c.status == .running, let cpu = c.cpuPercent {
                Text(String(format: "%.0f%%", cpu))
                    .font(G.mono(10.5))
                    .foregroundStyle(G.textSecondary)
            } else {
                Text(c.status == .paused ? "paused" : "stopped")
                    .font(.system(size: 10))
                    .foregroundStyle(G.textTertiary)
            }
        }
        .padding(.horizontal, 8)
        .frame(height: 28)
        .background(Color.white.opacity(0.045), in: RoundedRectangle(cornerRadius: 7, style: .continuous))
    }
}

// MARK: - Large Widget

private struct LargeRuntimeWidget: View {
    let snap: GravitonWidgetSnapshot

    private var statusColor: Color {
        if snap.online { return G.mint }
        return snap.isDemo ? G.amber : G.rose
    }

    private var statusLabel: String {
        if snap.online { return "ENGINE LIVE" }
        return snap.isDemo ? "DEMO DATA" : "OFFLINE"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 9) {
                GravitonMark(size: 24)
                VStack(alignment: .leading, spacing: 1) {
                    Text("Graviton")
                        .font(.system(size: 13.5, weight: .semibold))
                        .foregroundStyle(G.textPrimary)
                    Text(snap.hostName)
                        .font(.system(size: 11))
                        .foregroundStyle(G.textMuted)
                }
                Spacer()
                HStack(spacing: 5) {
                    Circle().fill(statusColor).frame(width: 6, height: 6)
                    Text(statusLabel)
                        .font(G.mono(9.5, weight: .bold))
                        .foregroundStyle(statusColor)
                }
                .padding(.horizontal, 8)
                .frame(height: 20)
                .background(statusColor.opacity(0.13), in: Capsule())
            }

            HStack(spacing: 10) {
                kpiTile("RUNNING", "\(snap.runningCount)/\(snap.totalContainers)", G.mint)
                kpiTile("CPU LOAD", Format.percent(snap.totalCPU), G.indigo)
                kpiTile("VM MEMORY", Format.bytes(snap.vmMemoryUsedBytes), G.sky)
            }

            if snap.hostMemoryBytes > 0 {
                VStack(alignment: .leading, spacing: 5) {
                    HStack {
                        Text("MEMORY ALLOCATION")
                            .font(.system(size: 9, weight: .semibold))
                            .tracking(0.55)
                            .foregroundStyle(G.textTertiary)
                        Spacer()
                        Text("\(Format.bytes(snap.vmMemoryUsedBytes)) VMs · \(Format.bytes(snap.hostMemoryBytes)) host")
                            .font(G.mono(9.5))
                            .foregroundStyle(G.textMuted)
                    }
                    StackedMeterBar([
                        .init(snap.vmMemoryFraction, color: G.indigo),
                        .init(snap.otherHostMemoryFraction, color: G.slate),
                    ], height: 5)
                }
            }

            VStack(alignment: .leading, spacing: 5) {
                HStack {
                    Text("CONTAINERS")
                        .font(.system(size: 9.5, weight: .semibold))
                        .tracking(0.6)
                        .foregroundStyle(G.textTertiary)
                    Spacer()
                    Text("\(snap.imageCount) images · \(snap.volumeCount) volumes")
                        .font(G.mono(9.5))
                        .foregroundStyle(G.textMuted)
                }

                if snap.containers.isEmpty {
                    Text("No containers created yet")
                        .font(.system(size: 12))
                        .foregroundStyle(G.textMuted)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .background(Color.white.opacity(0.03), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                } else {
                    ForEach(snap.containers.prefix(5)) { c in
                        Link(destination: URL(string: "graviton://container/\(c.id)") ?? URL(string: "graviton://containers")!) {
                            largeContainerRow(c)
                        }
                    }
                }
            }

            Spacer(minLength: 0)
        }
    }

    private func kpiTile(_ title: String, _ value: String, _ color: Color) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title)
                .font(.system(size: 9, weight: .semibold))
                .tracking(0.55)
                .foregroundStyle(G.textTertiary)
            Text(value)
                .font(G.mono(15, weight: .bold))
                .foregroundStyle(color)
                .lineLimit(1)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.white.opacity(0.045), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
    }

    private func largeContainerRow(_ c: WidgetContainerItem) -> some View {
        HStack(spacing: 8) {
            Circle()
                .fill(c.status == .running ? G.mint : (c.status == .paused ? G.amber : G.slate))
                .frame(width: 6.5, height: 6.5)
            VStack(alignment: .leading, spacing: 1) {
                Text(c.name)
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(G.textPrimary)
                    .lineLimit(1)
                Text(c.image)
                    .font(G.mono(9.5))
                    .foregroundStyle(G.textMuted)
                    .lineLimit(1)
            }
            Spacer(minLength: 6)
            if let port = c.portLabel, c.status == .running {
                Text(port)
                    .font(G.mono(10))
                    .foregroundStyle(G.sky)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(G.sky.opacity(0.14), in: Capsule())
            }
            Text(c.cpuPercent.map { String(format: "%.1f%%", $0) } ?? "—")
                .font(G.mono(11))
                .foregroundStyle(c.status == .running ? G.textPrimary : G.textTertiary)
                .frame(width: 42, alignment: .trailing)
            Text(c.memoryBytes.map(Format.bytes) ?? "—")
                .font(G.mono(10.5))
                .foregroundStyle(G.textSecondary)
                .frame(width: 52, alignment: .trailing)
        }
        .padding(.horizontal, 10)
        .frame(height: 36)
        .background(Color.white.opacity(0.04), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
    }
}

#if os(iOS)
private struct LockCircularWidget: View {
    let snap: GravitonWidgetSnapshot

    var body: some View {
        let total = max(1, snap.totalContainers)
        let frac = Double(snap.runningCount) / Double(total)
        Gauge(value: frac) {
            Image(systemName: "shippingbox.fill")
        } currentValueLabel: {
            Text("\(snap.runningCount)").font(.system(size: 13, weight: .bold).monospacedDigit())
        }
        .gaugeStyle(.accessoryCircular)
        .widgetURL(URL(string: "graviton://containers"))
    }
}

private struct LockRectangularWidget: View {
    let snap: GravitonWidgetSnapshot

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 4) {
                Image(systemName: "shippingbox.fill").font(.system(size: 10))
                Text("GRAVITON · \(snap.hostName.uppercased())")
                    .font(.system(size: 10, weight: .bold))
                    .lineLimit(1)
            }
            Text("\(snap.runningCount)/\(snap.totalContainers) running · \(Format.percent(snap.totalCPU)) CPU")
                .font(.system(size: 12, weight: .semibold).monospacedDigit())
                .lineLimit(1)
            if let top = snap.containers.first(where: { $0.status == .running }) ?? snap.containers.first {
                Text("\(top.name) · \(top.image)")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .widgetURL(URL(string: "graviton://containers"))
    }
}
#endif
