import Charts
import GravitonKit
import SwiftUI

/// Chart colours, checked with the dataviz palette validator against the
/// card surface (dark band, CVD ΔE 26.6): blue for the single-series charts
/// and received traffic, amber for sent.
private enum Series {
    static let primary = G.chartPrimary
    static let secondary = G.chartSecondary
}

/// The Stats tab: live CPU, memory and network charts and the process list.
struct StatsPanel: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    @State private var stats: ContainerStats?
    @State private var problem: String?

    private var samples: [StatSample] { stats?.samples ?? [] }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if let problem {
                    Label(problem, systemImage: "exclamationmark.triangle").foregroundStyle(G.danger).font(.system(size: 13))
                }
                if !container.isRunning && samples.isEmpty {
                    ContentUnavailableView("Not running", systemImage: "chart.xyaxis.line",
                                           description: Text("Start \(container.name) to see its CPU, memory and network use."))
                        .frame(maxWidth: .infinity, minHeight: 300)
                } else {
                    tiles
                    HStack(alignment: .top, spacing: 16) {
                        ChartCard(title: "CPU", detail: cpuDetail) { CPUChart(samples: samples, cpus: stats?.cpus ?? 1) }
                        ChartCard(title: "Memory", detail: memoryDetail) { MemoryChart(samples: samples) }
                    }
                    ChartCard(title: "Network", detail: "bytes per second") { NetworkChart(samples: samples) }
                    ProcessTable(processes: stats?.processes ?? [], running: container.isRunning)
                }
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 20)
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

    private var tiles: some View {
        let last = samples.last
        let rates = NetworkRate.series(samples).last
        return HStack(spacing: 12) {
            StatTile(title: "CPU", value: last.map { String(format: "%.1f", $0.cpuPercent) } ?? "—", unit: last == nil ? "" : "%")
            StatTile(title: "Memory", value: last.map { Format.bytes($0.memoryBytes) } ?? "—", unit: "") {
                if let last, last.memoryLimitBytes > 0 {
                    MeterBar(Double(last.memoryBytes) / Double(last.memoryLimitBytes), color: Series.primary, height: 4)
                }
            }
            StatTile(title: "Received", value: rates.map { Format.bytes(UInt64($0.received)) } ?? "—", unit: rates == nil ? "" : "/s")
            StatTile(title: "Sent", value: rates.map { Format.bytes(UInt64($0.sent)) } ?? "—", unit: rates == nil ? "" : "/s")
            StatTile(title: "Processes", value: stats.map { "\($0.processes.count)" } ?? "—", unit: "")
        }
    }
}

private struct StatTile<Extra: View>: View {
    var title: String
    var value: String
    var unit: String
    @ViewBuilder var extra: Extra

    init(title: String, value: String, unit: String, @ViewBuilder extra: () -> Extra = { EmptyView() }) {
        self.title = title
        self.value = value
        self.unit = unit
        self.extra = extra()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.system(size: 12)).foregroundStyle(G.muted)
            MetricValue(value, unit: unit, size: 20)
            extra
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 78, alignment: .topLeading)
        .gravitonCard()
    }
}

private struct ChartCard<Content: View>: View {
    var title: String
    var detail: String
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(.system(size: 13, weight: .medium)).foregroundStyle(G.text)
                Text(detail).font(.system(size: 11.5)).foregroundStyle(G.faint)
                Spacer()
                Text("Last 5 min").font(.system(size: 11.5)).foregroundStyle(G.faint)
            }
            content.frame(height: 170)
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard()
    }
}

// MARK: - Charts

/// Recessive axes and a hover readout shared by the charts.
private extension View {
    func statsAxes(yLabel: @escaping (Double) -> String) -> some View {
        chartXAxis {
            AxisMarks(values: .stride(by: .minute)) { _ in
                AxisGridLine().foregroundStyle(Color.white.opacity(0.05))
                AxisValueLabel(format: .dateTime.hour().minute()).foregroundStyle(G.faint)
            }
        }
        .chartYAxis {
            AxisMarks(position: .leading, values: .automatic(desiredCount: 4)) { value in
                AxisGridLine().foregroundStyle(Color.white.opacity(0.05))
                AxisValueLabel { if let v = value.as(Double.self) { Text(yLabel(v)) } }.foregroundStyle(G.faint)
            }
        }
    }
}

private struct HoverReadout: View {
    var date: Date
    var rows: [(String, Color?, String)]

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(date, format: .dateTime.hour().minute().second()).font(.system(size: 11)).foregroundStyle(G.muted)
            ForEach(rows, id: \.0) { row in
                HStack(spacing: 6) {
                    if let color = row.1 { RoundedRectangle(cornerRadius: 1).fill(color).frame(width: 8, height: 2) }
                    Text(row.0).foregroundStyle(G.muted)
                    Text(row.2).foregroundStyle(G.text).monospacedDigit()
                }
                .font(.system(size: 11.5))
            }
        }
        .padding(8)
        .background(G.raised, in: RoundedRectangle(cornerRadius: 7))
        .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(G.borderStrong))
    }
}

/// The sample nearest a hovered date.
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
                    .foregroundStyle(LinearGradient(colors: [Series.primary.opacity(0.22), Series.primary.opacity(0)], startPoint: .top, endPoint: .bottom))
                LineMark(x: .value("Time", s.date), y: .value("CPU", s.cpuPercent))
                    .foregroundStyle(Series.primary)
                    .lineStyle(StrokeStyle(lineWidth: 2))
            }
            if let picked {
                RuleMark(x: .value("Time", picked.date)).foregroundStyle(Color.white.opacity(0.25))
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
                    .foregroundStyle(LinearGradient(colors: [Series.primary.opacity(0.22), Series.primary.opacity(0)], startPoint: .top, endPoint: .bottom))
                LineMark(x: .value("Time", s.date), y: .value("Memory", Double(s.memoryBytes)))
                    .foregroundStyle(Series.primary)
                    .lineStyle(StrokeStyle(lineWidth: 2))
            }
            if limit > 0 {
                RuleMark(y: .value("Limit", limit))
                    .foregroundStyle(G.faint.opacity(0.6))
                    .lineStyle(StrokeStyle(lineWidth: 1, dash: [3, 3]))
                    .annotation(position: .top, alignment: .trailing) {
                        Text("Limit").font(.system(size: 10.5)).foregroundStyle(G.faint)
                    }
            }
            if let picked {
                RuleMark(x: .value("Time", picked.date)).foregroundStyle(Color.white.opacity(0.25))
                PointMark(x: .value("Time", picked.date), y: .value("Memory", Double(picked.memoryBytes)))
                    .foregroundStyle(Series.primary).symbolSize(64)
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
                LegendKey(color: Series.primary, label: "Received")
                LegendKey(color: Series.secondary, label: "Sent")
            }
            Chart {
                ForEach(rates) { r in
                    LineMark(x: .value("Time", r.date), y: .value("Bytes/s", r.received), series: .value("Direction", "Received"))
                        .foregroundStyle(Series.primary)
                        .lineStyle(StrokeStyle(lineWidth: 2))
                    LineMark(x: .value("Time", r.date), y: .value("Bytes/s", r.sent), series: .value("Direction", "Sent"))
                        .foregroundStyle(Series.secondary)
                        .lineStyle(StrokeStyle(lineWidth: 2))
                }
                if let picked {
                    RuleMark(x: .value("Time", picked.date))
                        .foregroundStyle(Color.white.opacity(0.25))
                        .annotation(position: .top, overflowResolution: .init(x: .fit(to: .chart), y: .fit(to: .chart))) {
                            HoverReadout(date: picked.date, rows: [
                                ("Received", Series.primary, Format.bytes(UInt64(picked.received)) + "/s"),
                                ("Sent", Series.secondary, Format.bytes(UInt64(picked.sent)) + "/s"),
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
            Text(label).font(.system(size: 11.5)).foregroundStyle(G.muted)
        }
    }
}

// MARK: - Processes

private struct ProcessTable: View {
    var processes: [ContainerProcess]
    var running: Bool
    @State private var order = [KeyPathComparator(\ContainerProcess.rssBytes, order: .reverse)]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Processes").font(.system(size: 13, weight: .medium))
            Table(processes.sorted(using: order), sortOrder: $order) {
                TableColumn("PID", value: \.pid) { Text("\($0.pid)").monospacedDigit() }.width(60)
                TableColumn("User", value: \.uid) { Text($0.uid == 0 ? "root" : "\($0.uid)") }.width(60)
                TableColumn("State", value: \.state) { Text(stateName($0.state)) }.width(80)
                TableColumn("Memory", value: \.rssBytes) { Text(Format.bytes($0.rssBytes)).monospacedDigit() }.width(80)
                TableColumn("CPU time", value: \.cpuMs) { Text(cpuTime($0.cpuMs)).monospacedDigit() }.width(80)
                TableColumn("Command", value: \.command) { Text($0.command).font(G.mono(12)).help($0.command) }
            }
            .frame(minHeight: 160, idealHeight: CGFloat(60 + 26 * max(processes.count, 3)))
            .overlay {
                if processes.isEmpty {
                    Text(running ? "Waiting for the first sample…" : "Not running").foregroundStyle(G.faint).font(.system(size: 12))
                }
            }
        }
        .padding(16)
        .gravitonCard()
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
