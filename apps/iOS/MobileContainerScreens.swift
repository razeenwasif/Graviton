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
                    GStatusLine(.failure, problem)
                }
                if !container.isRunning && samples.isEmpty {
                    GEmptyState("Not running", symbol: "chart.xyaxis.line", detail: "Start \(container.name) to see its use.",
                                actionTitle: "Start \(container.name)") { Task { await store.start(container) } }
                        .frame(minHeight: 420)
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
            .padding(.vertical, 4)
            .padding(.bottom, 20)
        }
        .safeAreaInset(edge: .top, spacing: 0) { MobileHeader("Stats", subtitle: container.name).background(G.background) }
        .mobileScreen()
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
            Text(title.uppercased()).font(.system(size: 10.5, weight: .semibold)).kerning(0.5).foregroundStyle(G.faint)
            Text(value).font(.system(size: 19, weight: .semibold)).foregroundStyle(G.text).monospacedDigit()
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .mobileCard(radius: 14)
    }

    private func card<Content: View>(_ title: String, detail: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(.system(size: 14, weight: .semibold)).foregroundStyle(G.text)
                Text(detail).font(.system(size: 11.5)).foregroundStyle(G.faint)
            }
            content().frame(height: 140)
        }
        .padding(14)
        .mobileCard()
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
            HStack {
                Text("Processes").font(.system(size: 14, weight: .semibold)).foregroundStyle(G.text)
                Spacer()
                if let n = stats?.processes.count, n > 0 { GBadge("\(n)") }
            }
            .padding(.bottom, 8)
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
                RowDivider(inset: 0)
            }
            if stats?.processes.isEmpty ?? true {
                Text(container.isRunning ? "Waiting for the first sample…" : "Not running").font(.system(size: 12)).foregroundStyle(G.faint)
            }
        }
        .padding(14)
        .mobileCard()
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
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                if let problem {
                    GStatusLine(.failure, problem)
                }
                if listing == nil && problem == nil {
                    GSpinner(size: 22).frame(maxWidth: .infinity, minHeight: 300)
                } else if listing?.entries.isEmpty == true {
                    GEmptyState("Empty folder", symbol: "folder").frame(minHeight: 300)
                } else if let entries = listing?.entries {
                    VStack(spacing: 0) {
                        ForEach(Array(entries.enumerated()), id: \.element.id) { i, e in
                            if i > 0 { RowDivider(inset: 54) }
                            let target = path == "/" ? "/" + e.name : path + "/" + e.name
                            NavigationLink {
                                if e.isDirectory {
                                    MobileFilesView(container: container, path: target)
                                } else {
                                    MobileFilePreview(container: container, path: target)
                                }
                            } label: {
                                HStack(spacing: 12) {
                                    Image(systemName: e.isDirectory ? "folder.fill" : e.kind == .symlink ? "arrow.turn.up.right" : "doc")
                                        .font(.system(size: 14))
                                        .foregroundStyle(e.isDirectory ? G.chartPrimary : G.muted)
                                        .frame(width: 30, height: 30)
                                        .background((e.isDirectory ? G.chartPrimary : G.muted).opacity(0.12), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                                    VStack(alignment: .leading, spacing: 2) {
                                        Text(e.name).font(.system(size: 15)).foregroundStyle(G.text).lineLimit(1)
                                        Text(e.linkTarget.map { "→ \($0)" } ?? "\(e.permissions)  \(e.isDirectory ? "" : Format.bytes(e.size))")
                                            .font(G.mono(11)).foregroundStyle(G.faint).lineLimit(1)
                                    }
                                    Spacer()
                                    Image(systemName: "chevron.right").font(.system(size: 11, weight: .semibold)).foregroundStyle(Color(hex: 0x5F6672))
                                }
                                .padding(.horizontal, 12)
                                .frame(minHeight: 54)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(PressRowStyle())
                        }
                    }
                    .mobileCard()
                }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 20)
        }
        .safeAreaInset(edge: .top, spacing: 0) {
            MobileHeader(path == "/" ? "Files" : (path as NSString).lastPathComponent, subtitle: path).background(G.background)
        }
        .mobileScreen()
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
                    GStatusLine(.failure, problem)
                } else if let c = contents {
                    Text(c.truncated ? "First \(Format.bytes(UInt64(c.data.count))) of \(Format.bytes(c.size))" : Format.bytes(c.size))
                        .font(.system(size: 12)).foregroundStyle(G.faint)
                    Text(c.text ?? hexDump(c.data))
                        .font(G.mono(11.5))
                        .foregroundStyle(c.text == nil ? G.muted : G.textSecondary)
                        .textSelection(.enabled)
                } else {
                    GSpinner(size: 22).frame(maxWidth: .infinity, minHeight: 300)
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(G.console.ignoresSafeArea())
        .safeAreaInset(edge: .top, spacing: 0) {
            MobileHeader((path as NSString).lastPathComponent, subtitle: path) {
                if let text = contents?.text {
                    Button { UIPasteboard.general.string = text } label: { Label("Copy", systemImage: "doc.on.doc") }
                        .buttonStyle(CircleIconStyle())
                    ShareLink(item: text) { Image(systemName: "square.and.arrow.up") }
                        .buttonStyle(CircleIconStyle())
                }
            }
            .background(G.background)
        }
        .toolbar(.hidden, for: .navigationBar)
        .safeAreaPadding(.bottom, GravitonTabBar.height)
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
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                if let problem {
                    GStatusLine(.failure, problem)
                }
                if let d = details {
                    if let project = d.project {
                        section("Stack") {
                            row("Project", project)
                            if let service = d.service {
                                row("Service", service)
                            }
                        }
                    }
                    section("Process") {
                        row("Command", d.command.joined(separator: " "))
                        row("Working directory", d.workingDir.isEmpty ? "/" : d.workingDir)
                        row("User", d.user ?? "root")
                        row("Hostname", d.hostname)
                    }
                    section("Image") {
                        row("Reference", d.imageReference)
                        row("Digest", String(d.imageDigest.prefix(19)) + "…")
                        if let arch = d.architecture {
                            row("Architecture", d.rosetta == true ? "\(arch) · Rosetta" : arch)
                        }
                    }
                    section("Virtual machine") {
                        row("vCPUs", "\(d.cpus)")
                        row("Memory", Format.bytes(d.memoryMib << 20))
                        row("Kernel", d.kernel)
                    }
                    section("Network") {
                        row("IP address", d.container.ipAddress ?? "—")
                        row("Ports", d.container.ports.isEmpty ? "—" : d.container.ports.map(\.fullLabel).joined(separator: "\n"))
                    }
                    if !d.container.mounts.isEmpty {
                        section("Volumes") {
                            ForEach(d.container.mounts, id: \.target) { m in row(m.target, m.source + (m.readOnly ? " (read-only)" : "")) }
                        }
                    }
                    VStack(alignment: .leading, spacing: 8) {
                        HStack {
                            GCaption("Environment")
                            Spacer()
                            Toggle("Show values", isOn: $showValues).toggleStyle(.gSwitch)
                        }
                        .padding(.horizontal, 4)
                        VStack(spacing: 0) {
                            ForEach(Array(d.env.enumerated()), id: \.offset) { i, pair in
                                if i > 0 { RowDivider() }
                                let parts = pair.split(separator: "=", maxSplits: 1).map(String.init)
                                row(parts.first ?? pair, showValues ? (parts.count > 1 ? parts[1] : "") : "••••••")
                            }
                        }
                        .mobileCard()
                    }
                    if let labels = d.labels, !labels.isEmpty {
                        section("Labels") {
                            ForEach(labels.sorted { $0.key < $1.key }, id: \.key) { key, value in
                                row(key, value)
                            }
                        }
                    }
                    if d.healthcheck != nil || d.health != nil {
                        section("Healthcheck") {
                            if let h = d.health {
                                HStack {
                                    Text("Status").font(.system(size: 12)).foregroundStyle(G.muted)
                                    Spacer()
                                    HealthBadge(h.status)
                                }
                                .padding(.horizontal, 16)
                                .padding(.vertical, 11)
                                row("Failures", "\(h.failingStreak)")
                            }
                            if let hc = d.healthcheck {
                                row("Command", hc.commandDisplay)
                                row("Schedule", "every \(hc.intervalSecs)s · timeout \(hc.timeoutSecs)s · retries \(hc.retries)")
                                if hc.startPeriodSecs > 0 {
                                    row("Start period", "\(hc.startPeriodSecs)s")
                                }
                            }
                            if let logs = d.health?.log, !logs.isEmpty {
                                ForEach(Array(logs.reversed().prefix(3).enumerated()), id: \.offset) { _, entry in
                                    let out = entry.output.trimmingCharacters(in: .whitespacesAndNewlines)
                                    let ts = Date(timeIntervalSince1970: Double(entry.end)).formatted(date: .omitted, time: .standard)
                                    row("Exit \(entry.exitCode) · \(ts)", out.isEmpty ? "—" : out)
                                }
                            }
                        }
                    }
                    section("Lifecycle") {
                        row("ID", d.container.id)
                        row("Status", d.container.statusText())
                        row("Restart policy", (d.restart ?? .no).rawValue)
                    }
                } else if problem == nil {
                    GSpinner(size: 22).frame(maxWidth: .infinity, minHeight: 300)
                }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 20)
        }
        .safeAreaInset(edge: .top, spacing: 0) { MobileHeader("Inspect", subtitle: container.name).background(G.background) }
        .mobileScreen()
        .task(id: container.id) {
            do {
                details = try await store.inspect(container.id)
            } catch {
                problem = error.localizedDescription
            }
        }
    }

    private func section<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            GCaption(title).padding(.horizontal, 4)
            VStack(spacing: 0) {
                Group(subviews: content()) { rows in
                    ForEach(Array(rows.enumerated()), id: \.offset) { i, r in
                        if i > 0 { RowDivider() }
                        r
                    }
                }
            }
            .mobileCard()
        }
    }

    private func row(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(label).font(.system(size: 12)).foregroundStyle(G.muted)
            Text(value).font(G.mono(13)).foregroundStyle(G.text).textSelection(.enabled)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 11)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
