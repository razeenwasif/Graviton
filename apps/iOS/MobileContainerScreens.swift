import Charts
import GravitonKit
import SwiftUI

// MARK: - Stats

/// CPU, memory and network over the last five minutes, and the processes.
struct MobileStatsView: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    @State private var stats: ContainerStats?
    @State private var problem: String?
    @State private var hover: Date?

    private var samples: [StatSample] { stats?.samples ?? [] }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                if let problem {
                    Label(problem, systemImage: "exclamationmark.triangle").font(.system(size: 13)).foregroundStyle(G.danger)
                }
                if !container.isRunning && samples.isEmpty {
                    ContentUnavailableView("Not running", systemImage: "chart.xyaxis.line", description: Text("Start \(container.name) to see its use."))
                        .padding(.top, 60)
                } else {
                    tiles
                    card("CPU", detail: "100% is one busy vCPU, of \(stats?.cpus ?? 0)") {
                        chart(samples.map { ($0.date, $0.cpuPercent) }, format: { String(format: "%.0f%%", $0) }, floor: 100)
                    }
                    card("Memory", detail: samples.last.map { "limit \(Format.bytes($0.memoryLimitBytes))" } ?? "") {
                        chart(samples.map { ($0.date, Double($0.memoryBytes)) }, format: { Format.bytes(UInt64(max(0, $0))) },
                              floor: Double(samples.last?.memoryLimitBytes ?? 0))
                    }
                    card("Network", detail: "per second") { network }
                    processes
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
        }
        .background(G.background)
        .navigationTitle("Stats")
        .navigationBarTitleDisplayMode(.inline)
        .task(id: container.id) {
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

    private var tiles: some View {
        let last = samples.last
        let rate = NetworkRate.series(samples).last
        return Grid(horizontalSpacing: 10, verticalSpacing: 10) {
            GridRow {
                tile("CPU", last.map { String(format: "%.1f%%", $0.cpuPercent) } ?? "—")
                tile("Memory", last.map { Format.bytes($0.memoryBytes) } ?? "—")
            }
            GridRow {
                tile("Received", rate.map { Format.bytes(UInt64($0.received)) + "/s" } ?? "—")
                tile("Sent", rate.map { Format.bytes(UInt64($0.sent)) + "/s" } ?? "—")
            }
        }
    }

    private func tile(_ title: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.system(size: 12)).foregroundStyle(G.muted)
            Text(value).font(.system(size: 19, weight: .semibold)).monospacedDigit()
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(G.card, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
    }

    private func card<Content: View>(_ title: String, detail: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(.system(size: 14, weight: .semibold))
                Text(detail).font(.system(size: 11.5)).foregroundStyle(G.faint)
            }
            content().frame(height: 140)
        }
        .padding(14)
        .background(G.card, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
    }

    /// One series with a touch readout; `floor` keeps the y-axis from hugging small values.
    private func chart(_ points: [(Date, Double)], format: @escaping (Double) -> String, floor: Double) -> some View {
        let picked = hover.flatMap { h in points.min { abs($0.0.timeIntervalSince(h)) < abs($1.0.timeIntervalSince(h)) } }
        return Chart {
            ForEach(points, id: \.0) { p in
                AreaMark(x: .value("Time", p.0), y: .value("Value", p.1))
                    .foregroundStyle(LinearGradient(colors: [G.chartPrimary.opacity(0.22), .clear], startPoint: .top, endPoint: .bottom))
                LineMark(x: .value("Time", p.0), y: .value("Value", p.1))
                    .foregroundStyle(G.chartPrimary)
                    .lineStyle(StrokeStyle(lineWidth: 2))
            }
            if let picked {
                RuleMark(x: .value("Time", picked.0))
                    .foregroundStyle(Color.white.opacity(0.25))
                    .annotation(position: .top, overflowResolution: .init(x: .fit(to: .chart), y: .disabled)) {
                        Text("\(picked.0.formatted(date: .omitted, time: .standard))  \(format(picked.1))")
                            .font(.system(size: 11)).monospacedDigit()
                            .padding(5).background(G.raised, in: RoundedRectangle(cornerRadius: 6))
                    }
            }
        }
        .chartYScale(domain: 0...max(floor, (points.map(\.1).max() ?? 0) * 1.15, 1))
        .chartYAxis {
            AxisMarks(position: .leading, values: .automatic(desiredCount: 3)) { v in
                AxisGridLine().foregroundStyle(Color.white.opacity(0.05))
                AxisValueLabel { if let d = v.as(Double.self) { Text(format(d)) } }.foregroundStyle(G.faint)
            }
        }
        .chartXAxis {
            AxisMarks(values: .stride(by: .minute, count: 2)) { _ in
                AxisValueLabel(format: .dateTime.hour().minute()).foregroundStyle(G.faint)
            }
        }
        .chartXSelection(value: $hover)
    }

    private var network: some View {
        let rates = NetworkRate.series(samples)
        return VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 12) {
                legend(G.chartPrimary, "Received")
                legend(G.chartSecondary, "Sent")
            }
            Chart {
                ForEach(rates) { r in
                    LineMark(x: .value("Time", r.date), y: .value("Bytes/s", r.received), series: .value("Direction", "Received"))
                        .foregroundStyle(G.chartPrimary).lineStyle(StrokeStyle(lineWidth: 2))
                    LineMark(x: .value("Time", r.date), y: .value("Bytes/s", r.sent), series: .value("Direction", "Sent"))
                        .foregroundStyle(G.chartSecondary).lineStyle(StrokeStyle(lineWidth: 2))
                }
            }
            .chartYAxis {
                AxisMarks(position: .leading, values: .automatic(desiredCount: 3)) { v in
                    AxisGridLine().foregroundStyle(Color.white.opacity(0.05))
                    AxisValueLabel { if let d = v.as(Double.self) { Text(Format.bytes(UInt64(max(0, d)))) } }.foregroundStyle(G.faint)
                }
            }
            .chartXAxis(.hidden)
        }
    }

    private func legend(_ color: Color, _ label: String) -> some View {
        HStack(spacing: 5) {
            RoundedRectangle(cornerRadius: 1).fill(color).frame(width: 12, height: 2)
            Text(label).font(.system(size: 11.5)).foregroundStyle(G.muted)
        }
    }

    private var processes: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Processes").font(.system(size: 14, weight: .semibold)).padding(.bottom, 8)
            ForEach((stats?.processes ?? []).sorted { $0.rssBytes > $1.rssBytes }) { p in
                HStack(alignment: .firstTextBaseline) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(p.command).font(G.mono(12)).lineLimit(2)
                        Text("pid \(p.pid) · \(p.uid == 0 ? "root" : "uid \(p.uid)")").font(.system(size: 11)).foregroundStyle(G.faint)
                    }
                    Spacer()
                    Text(Format.bytes(p.rssBytes)).font(.system(size: 12)).foregroundStyle(G.muted).monospacedDigit()
                }
                .padding(.vertical, 8)
                Divider().overlay(G.border)
            }
            if stats?.processes.isEmpty ?? true {
                Text(container.isRunning ? "Waiting for the first sample…" : "Not running").font(.system(size: 12)).foregroundStyle(G.faint)
            }
        }
        .padding(14)
        .background(G.card, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
    }
}

// MARK: - Files

/// A folder in the container; tapping a folder goes in, a file opens its preview.
struct MobileFilesView: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    var path: String = "/"
    @State private var listing: DirectoryListing?
    @State private var problem: String?

    var body: some View {
        List {
            if let problem {
                Label(problem, systemImage: "exclamationmark.triangle").foregroundStyle(G.danger).font(.system(size: 13))
            }
            ForEach(listing?.entries ?? []) { e in
                let target = path == "/" ? "/" + e.name : path + "/" + e.name
                NavigationLink {
                    if e.isDirectory {
                        MobileFilesView(container: container, path: target)
                    } else {
                        MobileFilePreview(container: container, path: target)
                    }
                } label: {
                    HStack(spacing: 10) {
                        Image(systemName: e.isDirectory ? "folder.fill" : e.kind == .symlink ? "arrow.turn.up.right" : "doc")
                            .foregroundStyle(e.isDirectory ? G.chartPrimary : G.muted).frame(width: 22)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(e.name).font(.system(size: 15)).lineLimit(1)
                            Text(e.linkTarget.map { "→ \($0)" } ?? "\(e.permissions)  \(e.isDirectory ? "" : Format.bytes(e.size))")
                                .font(G.mono(11)).foregroundStyle(G.faint).lineLimit(1)
                        }
                    }
                }
                .listRowBackground(G.card)
            }
        }
        .scrollContentBackground(.hidden)
        .background(G.background)
        .overlay {
            if listing == nil && problem == nil { ProgressView() }
            if listing?.entries.isEmpty == true { Text("Empty folder").foregroundStyle(G.faint) }
        }
        .navigationTitle(path == "/" ? "Files" : (path as NSString).lastPathComponent)
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { await load() }
        .task(id: path) { await load() }
    }

    private func load() async {
        do {
            listing = try await store.listFiles(in: container.id, path: path)
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }
}

/// The first 256 KB of a file: text, or a hex dump.
struct MobileFilePreview: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    var path: String
    @State private var contents: FileContents?
    @State private var problem: String?

    var body: some View {
        ScrollView([.vertical, .horizontal]) {
            VStack(alignment: .leading, spacing: 8) {
                if let problem {
                    Label(problem, systemImage: "exclamationmark.triangle").foregroundStyle(G.danger)
                } else if let c = contents {
                    Text(c.truncated ? "First \(Format.bytes(UInt64(c.data.count))) of \(Format.bytes(c.size))" : Format.bytes(c.size))
                        .font(.system(size: 12)).foregroundStyle(G.faint)
                    Text(c.text ?? hexDump(c.data))
                        .font(G.mono(11.5))
                        .foregroundStyle(c.text == nil ? G.muted : G.textSecondary)
                        .textSelection(.enabled)
                } else {
                    ProgressView()
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(G.console)
        .navigationTitle((path as NSString).lastPathComponent)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if let text = contents?.text {
                ToolbarItem(placement: .primaryAction) {
                    ShareLink(item: text) { Image(systemName: "square.and.arrow.up") }
                }
            }
        }
        .task(id: path) {
            do {
                contents = try await store.readFile(in: container.id, path: path)
            } catch {
                problem = error.localizedDescription
            }
        }
    }

    private func hexDump(_ bytes: [UInt8]) -> String {
        let shown = bytes.prefix(512)
        return stride(from: 0, to: shown.count, by: 8).map { offset in
            let row = shown[(shown.startIndex + offset)..<min(shown.startIndex + offset + 8, shown.endIndex)]
            return String(format: "%04x  ", offset) + row.map { String(format: "%02x", $0) }.joined(separator: " ")
        }.joined(separator: "\n")
    }
}

// MARK: - Inspect

/// How the container is set up.
struct MobileInspectView: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    @State private var details: ContainerDetails?
    @State private var problem: String?
    @State private var showValues = false

    var body: some View {
        List {
            if let problem {
                Label(problem, systemImage: "exclamationmark.triangle").foregroundStyle(G.danger)
            }
            if let d = details {
                Section("Process") {
                    row("Command", d.command.joined(separator: " "))
                    row("Working directory", d.workingDir.isEmpty ? "/" : d.workingDir)
                    row("User", d.user ?? "root")
                }
                Section("Image") {
                    row("Reference", d.imageReference)
                    row("Digest", String(d.imageDigest.prefix(19)) + "…")
                }
                Section("Virtual machine") {
                    row("vCPUs", "\(d.cpus)")
                    row("Memory", Format.bytes(d.memoryMib << 20))
                    row("Kernel", d.kernel)
                }
                Section("Network") {
                    row("IP address", d.container.ipAddress ?? "—")
                    row("Ports", d.container.ports.isEmpty ? "—" : d.container.ports.map(\.label).joined(separator: "\n"))
                }
                if !d.container.mounts.isEmpty {
                    Section("Volumes") {
                        ForEach(d.container.mounts, id: \.target) { m in row(m.target, m.source + (m.readOnly ? " (read-only)" : "")) }
                    }
                }
                Section {
                    ForEach(d.env, id: \.self) { pair in
                        let parts = pair.split(separator: "=", maxSplits: 1).map(String.init)
                        row(parts.first ?? pair, showValues ? (parts.count > 1 ? parts[1] : "") : "••••••")
                    }
                } header: {
                    HStack {
                        Text("Environment")
                        Spacer()
                        Button(showValues ? "Hide values" : "Show values") { showValues.toggle() }.font(.system(size: 12)).textCase(nil)
                    }
                }
                Section("Lifecycle") {
                    row("ID", d.container.id)
                    row("Status", d.container.statusText())
                }
            }
        }
        .scrollContentBackground(.hidden)
        .background(G.background)
        .overlay { if details == nil && problem == nil { ProgressView() } }
        .navigationTitle("Inspect")
        .navigationBarTitleDisplayMode(.inline)
        .task(id: container.id) {
            do {
                details = try await store.inspect(container.id)
            } catch {
                problem = error.localizedDescription
            }
        }
    }

    private func row(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(label).font(.system(size: 12)).foregroundStyle(G.muted)
            Text(value).font(G.mono(13)).textSelection(.enabled)
        }
        .listRowBackground(G.card)
    }
}
