import Charts
import GravitonKit
import SwiftUI

private enum Series {
    static let primary = G.indigo
    static let secondary = G.sky
    static let sent = G.amber
}

/// The Stats tab: Stratus-grade live CPU, memory and network spline charts and process table.
struct StatsPanel: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    @State private var stats: ContainerStats?
    @State private var problem: String?

    private var samples: [StatSample] { stats?.samples ?? [] }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                if let problem {
                    GStatusLine(.failure, problem)
                }
                if !container.isRunning && samples.isEmpty {
                    GEmptyState("Not running", symbol: "chart.xyaxis.line",
                                detail: "Start \(container.name) to see its CPU, memory and network use.",
                                actionTitle: "Start \(container.name)") { Task { await store.start(container) } }
                        .frame(minHeight: 340)
                } else {
                    kpiStrip
                    HStack(alignment: .top, spacing: 16) {
                        ChartCard(title: "CPU", detail: cpuDetail) { CPUChart(samples: samples, cpus: stats?.cpus ?? 1) }
                        ChartCard(title: "Memory", detail: memoryDetail) { MemoryChart(samples: samples) }
                    }
                    ChartCard(title: "Network", detail: "bytes per second") { NetworkChart(samples: samples) }
                    ProcessTable(processes: stats?.processes ?? [], running: container.isRunning)
                }
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 18)
        }
        .task(id: container.id) {
            stats = nil
            while !Task.isCancelled {
                do {
                    stats = try await store.stats(for: container.id)
                    problem = nil
                } catch {
                    problem = error.localizedDescription
                }
                try? await Task.sleep(for: .seconds(2))
            }
        }
    }

    private var cpuDetail: String { "100% is one busy vCPU, of \(stats?.cpus ?? 0)" }
    private var memoryDetail: String { samples.last.map { "limit \(Format.bytes($0.memoryLimitBytes))" } ?? "" }

    private var kpiStrip: some View {
        let last = samples.last
        let rates = NetworkRate.series(samples).last
        return HStack(spacing: 34) {
            GMetricStatColumn(
                label: "CPU",
                value: last.map { String(format: "%.1f%%", $0.cpuPercent) } ?? "—",
                subtitle: "\(stats?.cpus ?? 1) vCPU",
                valueColor: G.indigo
            )
            GMetricStatColumn(
                label: "Memory",
                value: last.map { Format.bytes($0.memoryBytes) } ?? "—",
                subtitle: last.map { "limit \(Format.bytes($0.memoryLimitBytes))" },
                valueColor: G.sky
            )
            GMetricStatColumn(
                label: "Rx Rate",
                value: rates.map { Format.bytes(UInt64($0.received)) + "/s" } ?? "—",
                subtitle: "inbound",
                valueColor: G.mint
            )
            GMetricStatColumn(
                label: "Tx Rate",
                value: rates.map { Format.bytes(UInt64($0.sent)) + "/s" } ?? "—",
                subtitle: "outbound",
                valueColor: G.amber
            )
            GMetricStatColumn(
                label: "Processes",
                value: stats.map { "\($0.processes.count)" } ?? "—",
                subtitle: "active tasks",
                valueColor: G.textPrimary
            )
            Spacer()
        }
        .padding(.vertical, 14)
        .padding(.horizontal, 18)
        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

private struct ChartCard<Content: View>: View {
    var title: String
    var detail: String
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(G.sectionTitle()).foregroundStyle(G.textPrimary)
                Text(detail).font(.system(size: 11.5)).foregroundStyle(G.textMuted)
                Spacer()
                Text("Last 5 min").font(.system(size: 11)).foregroundStyle(G.textTertiary)
            }
            content.frame(height: 175)
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(G.surfaceSubtle)
    }
}

// MARK: - Charts

private extension View {
    func statsAxes(yLabel: @escaping (Double) -> String) -> some View {
        chartXAxis {
            AxisMarks(values: .stride(by: .minute)) { _ in
                AxisValueLabel(format: .dateTime.hour().minute()).foregroundStyle(G.textTertiary)
            }
        }
        .chartYAxis {
            AxisMarks(position: .leading, values: .automatic(desiredCount: 4)) { value in
                AxisGridLine(stroke: StrokeStyle(lineWidth: 0.7, dash: [3, 3]))
                    .foregroundStyle(Color.white.opacity(0.05))
                AxisValueLabel { if let v = value.as(Double.self) { Text(yLabel(v)) } }.foregroundStyle(G.textTertiary)
            }
        }
    }
}

private struct HoverReadout: View {
    var date: Date
    var rows: [(String, Color?, String)]

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(date, format: .dateTime.hour().minute().second()).font(.system(size: 10.5)).foregroundStyle(G.textMuted)
            ForEach(rows, id: \.0) { row in
                HStack(spacing: 6) {
                    if let color = row.1 { RoundedRectangle(cornerRadius: 1).fill(color).frame(width: 8, height: 2) }
                    Text(row.0).foregroundStyle(G.textMuted)
                    Text(row.2).foregroundStyle(G.textPrimary).monospacedDigit()
                }
                .font(.system(size: 11.5))
            }
        }
        .padding(9)
        .background(G.palette, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .shadow(color: .black.opacity(0.6), radius: 12, y: 4)
    }
}

private func nearest<T>(_ items: [T], to date: Date?, by key: (T) -> Date) -> T? {
    guard let date else { return nil }
    return items.min { abs(key($0).timeIntervalSince(date)) < abs(key($1).timeIntervalSince(date)) }
}

private struct CPUChart: View {
    var samples: [StatSample]
    var cpus: UInt32
    @State private var hover: Date?

    var body: some View {
        let picked = nearest(samples, to: hover, by: \.date)
        Chart {
            ForEach(samples, id: \.timestampMs) { s in
                AreaMark(x: .value("Time", s.date), y: .value("CPU", s.cpuPercent))
                    .interpolationMethod(.catmullRom)
                    .foregroundStyle(LinearGradient(colors: [Series.primary.opacity(0.24), Series.primary.opacity(0)], startPoint: .top, endPoint: .bottom))
                LineMark(x: .value("Time", s.date), y: .value("CPU", s.cpuPercent))
                    .interpolationMethod(.catmullRom)
                    .foregroundStyle(Series.primary)
                    .lineStyle(StrokeStyle(lineWidth: 2))
            }
            if let picked {
                RuleMark(x: .value("Time", picked.date)).foregroundStyle(Color.white.opacity(0.2))
                PointMark(x: .value("Time", picked.date), y: .value("CPU", picked.cpuPercent))
                    .foregroundStyle(Series.primary).symbolSize(64)
                    .annotation(position: .top, overflowResolution: .init(x: .fit(to: .chart), y: .fit(to: .chart))) {
                        HoverReadout(date: picked.date, rows: [("CPU", nil, String(format: "%.1f%%", picked.cpuPercent))])
                    }
            }
        }
        .chartYScale(domain: 0...max(100, (samples.map(\.cpuPercent).max() ?? 0) * 1.15))
        .statsAxes { "\(Int($0))%" }
        .chartXSelection(value: $hover)
        .accessibilityLabel("CPU use over the last five minutes")
    }
}

private struct MemoryChart: View {
    var samples: [StatSample]
    @State private var hover: Date?

    var body: some View {
        let picked = nearest(samples, to: hover, by: \.date)
        let limit = Double(samples.last?.memoryLimitBytes ?? 0)
        Chart {
            ForEach(samples, id: \.timestampMs) { s in
                AreaMark(x: .value("Time", s.date), y: .value("Memory", Double(s.memoryBytes)))
                    .interpolationMethod(.catmullRom)
                    .foregroundStyle(LinearGradient(colors: [Series.secondary.opacity(0.24), Series.secondary.opacity(0)], startPoint: .top, endPoint: .bottom))
                LineMark(x: .value("Time", s.date), y: .value("Memory", Double(s.memoryBytes)))
                    .interpolationMethod(.catmullRom)
                    .foregroundStyle(Series.secondary)
                    .lineStyle(StrokeStyle(lineWidth: 2))
            }
            if limit > 0 {
                RuleMark(y: .value("Limit", limit))
                    .foregroundStyle(G.textTertiary.opacity(0.65))
                    .lineStyle(StrokeStyle(lineWidth: 1, dash: [3, 3]))
                    .annotation(position: .top, alignment: .trailing) {
                        Text("Limit").font(.system(size: 10.5)).foregroundStyle(G.textMuted)
                    }
            }
            if let picked {
                RuleMark(x: .value("Time", picked.date)).foregroundStyle(Color.white.opacity(0.2))
                PointMark(x: .value("Time", picked.date), y: .value("Memory", Double(picked.memoryBytes)))
                    .foregroundStyle(Series.secondary).symbolSize(64)
                    .annotation(position: .top, overflowResolution: .init(x: .fit(to: .chart), y: .fit(to: .chart))) {
                        HoverReadout(date: picked.date, rows: [("Memory", nil, Format.bytes(picked.memoryBytes))])
                    }
            }
        }
        .chartYScale(domain: 0...max(limit, 1) * 1.08)
        .statsAxes { Format.bytes(UInt64(max(0, $0))) }
        .chartXSelection(value: $hover)
        .accessibilityLabel("Memory use over the last five minutes")
    }
}

private struct NetworkChart: View {
    var samples: [StatSample]
    @State private var hover: Date?

    var body: some View {
        let rates = NetworkRate.series(samples)
        let picked = nearest(rates, to: hover, by: \.date)
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 14) {
                LegendKey(color: G.mint, label: "Received")
                LegendKey(color: Series.sent, label: "Sent")
            }
            Chart {
                ForEach(rates) { r in
                    LineMark(x: .value("Time", r.date), y: .value("Bytes/s", r.received), series: .value("Direction", "Received"))
                        .interpolationMethod(.catmullRom)
                        .foregroundStyle(G.mint)
                        .lineStyle(StrokeStyle(lineWidth: 2))
                    LineMark(x: .value("Time", r.date), y: .value("Bytes/s", r.sent), series: .value("Direction", "Sent"))
                        .interpolationMethod(.catmullRom)
                        .foregroundStyle(Series.sent)
                        .lineStyle(StrokeStyle(lineWidth: 2))
                }
                if let picked {
                    RuleMark(x: .value("Time", picked.date))
                        .foregroundStyle(Color.white.opacity(0.2))
                        .annotation(position: .top, overflowResolution: .init(x: .fit(to: .chart), y: .fit(to: .chart))) {
                            HoverReadout(date: picked.date, rows: [
                                ("Received", G.mint, Format.bytes(UInt64(picked.received)) + "/s"),
                                ("Sent", Series.sent, Format.bytes(UInt64(picked.sent)) + "/s"),
                            ])
                        }
                }
            }
            .statsAxes { Format.bytes(UInt64(max(0, $0))) }
            .chartXSelection(value: $hover)
            .accessibilityLabel("Network traffic received and sent over the last five minutes")
        }
    }
}

private struct LegendKey: View {
    var color: Color
    var label: String

    var body: some View {
        HStack(spacing: 6) {
            RoundedRectangle(cornerRadius: 1).fill(color).frame(width: 12, height: 2)
            Text(label).font(.system(size: 11.5)).foregroundStyle(G.textMuted)
        }
    }
}

// MARK: - Processes

private struct ProcessTable: View {
    var processes: [ContainerProcess]
    var running: Bool
    @State private var sort: Column = .memory
    @State private var ascending = false

    enum Column: Hashable { case pid, user, state, memory, cpu, command }

    private var sorted: [ContainerProcess] {
        let out = processes.sorted { a, b in
            switch sort {
            case .pid: a.pid < b.pid
            case .user: a.uid < b.uid
            case .state: a.state < b.state
            case .memory: a.rssBytes < b.rssBytes
            case .cpu: a.cpuMs < b.cpuMs
            case .command: a.command < b.command
            }
        }
        return ascending ? out : out.reversed()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                GCaption("Processes")
                Spacer()
                if !processes.isEmpty { GBadge("\(processes.count)", color: G.indigo) }
            }
            VStack(spacing: 0) {
                HStack(spacing: 10) {
                    GColumnHeader("PID", key: Column.pid, sort: $sort, ascending: $ascending).frame(width: 56)
                    GColumnHeader("User", key: Column.user, sort: $sort, ascending: $ascending).frame(width: 56)
                    GColumnHeader("State", key: Column.state, sort: $sort, ascending: $ascending).frame(width: 84)
                    GColumnHeader("Memory", key: Column.memory, sort: $sort, ascending: $ascending, alignment: .trailing).frame(width: 74)
                    GColumnHeader("CPU time", key: Column.cpu, sort: $sort, ascending: $ascending, alignment: .trailing).frame(width: 74)
                    GColumnHeader("Command", key: Column.command, sort: $sort, ascending: $ascending).frame(maxWidth: .infinity)
                }
                .padding(.horizontal, 10)
                .frame(height: 28)

                if processes.isEmpty {
                    Text(running ? "Waiting for the first sample…" : "Not running")
                        .foregroundStyle(G.textTertiary).font(.system(size: 12))
                        .frame(maxWidth: .infinity, minHeight: 90)
                } else {
                    VStack(spacing: 2) {
                        ForEach(sorted, id: \.pid) { p in
                            HStack(spacing: 10) {
                                Text("\(p.pid)").frame(width: 56, alignment: .leading)
                                Text(p.uid == 0 ? "root" : "\(p.uid)").frame(width: 56, alignment: .leading)
                                HStack(spacing: 6) {
                                    Circle().fill(p.state == "R" ? G.mint : (p.state == "Z" ? G.rose : G.slate)).frame(width: 6, height: 6)
                                    Text(stateName(p.state))
                                }
                                .frame(width: 84, alignment: .leading)
                                Text(Format.bytes(p.rssBytes)).frame(width: 74, alignment: .trailing)
                                Text(cpuTime(p.cpuMs)).frame(width: 74, alignment: .trailing)
                                Text(p.command).font(G.mono(12)).foregroundStyle(G.textPrimary).lineLimit(1).truncationMode(.tail)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .help(p.command)
                            }
                            .font(.system(size: 12))
                            .monospacedDigit()
                            .foregroundStyle(G.textSecondary)
                            .padding(.horizontal, 10)
                            .frame(height: 30)
                            .gravitonRow(selected: false)
                        }
                    }
                    .padding(.vertical, 4)
                }
            }
            .padding(6)
            .background(G.bg, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        }
        .padding(16)
        .gravitonCard(G.surfaceSubtle)
    }

    private func stateName(_ s: String) -> String {
        switch s {
        case "R": "Running"
        case "S": "Sleeping"
        case "D": "Waiting"
        case "Z": "Zombie"
        case "T", "t": "Stopped"
        case "I": "Idle"
        default: s
        }
    }

    private func cpuTime(_ ms: UInt64) -> String {
        let s = Double(ms) / 1000
        return s < 60 ? String(format: "%.1fs", s) : String(format: "%dm %02ds", Int(s) / 60, Int(s) % 60)
    }
}
