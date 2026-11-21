import AppKit
import GravitonKit
import SwiftUI

struct NetworksScreen: View {
    @Environment(EngineStore.self) private var store
    @Environment(AppRouter.self) private var router
    @State private var tab: Tab = .endpoints
    @State private var search = ""
    @State private var selection: NetworkEndpoint.ID?

    enum Tab: String, CaseIterable, Identifiable {
        case endpoints = "Containers", ports = "Published Ports"
        var id: String { rawValue }
    }

    private var info: NetworkInfo {
        store.network ?? NetworkInfo.synthesized(from: store.containers)
    }

    private var runningEndpoints: [NetworkEndpoint] {
        info.endpoints.filter(\.isRunning)
    }

    private var filteredEndpoints: [NetworkEndpoint] {
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !q.isEmpty else { return info.endpoints }
        return info.endpoints.filter { ep in
            ep.containerName.lowercased().contains(q)
                || ep.image.lowercased().contains(q)
                || (ep.ipAddress?.lowercased().contains(q) ?? false)
                || ep.dnsNames.contains { $0.lowercased().contains(q) }
                || ep.ports.contains { $0.fullLabel.lowercased().contains(q) }
        }
    }

    private var publishedPorts: [PublishedPortRow] {
        let all = info.endpoints.flatMap { ep in
            ep.ports.map { PublishedPortRow(endpoint: ep, port: $0) }
        }
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !q.isEmpty else { return all }
        return all.filter {
            $0.endpoint.containerName.lowercased().contains(q)
                || $0.port.fullLabel.lowercased().contains(q)
                || $0.port.bindIp.lowercased().contains(q)
                || "\($0.port.hostPort)".contains(q)
                || "\($0.port.containerPort)".contains(q)
        }
    }

    private var totalPortCount: Int {
        info.endpoints.reduce(0) { $0 + $1.ports.count }
    }

    private var activePortCount: Int {
        runningEndpoints.reduce(0) { $0 + $1.ports.count }
    }

    private var activeTcpCount: Int {
        runningEndpoints.flatMap(\.ports).filter { $0.protocol == .tcp }.count
    }

    private var activeUdpCount: Int {
        runningEndpoints.flatMap(\.ports).filter { $0.protocol == .udp }.count
    }

    private var subtitle: String {
        let conn = runningEndpoints.count
        let portLabel = activePortCount == 1 ? "1 active port" : "\(activePortCount) active ports"
        return "\(info.name) switch · \(conn) connected · \(portLabel)"
    }

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader("Networks", subtitle: subtitle) {
                HeaderSearch(prompt: "Search containers, IPs or ports", text: $search)
                Button { Task { await store.refresh() } } label: { Image(systemName: "arrow.clockwise") }
                    .buttonStyle(.gIconBordered)
                    .help("Refresh")
            }

            VStack(alignment: .leading, spacing: 16) {
                summaryCards

                GSegmented(selection: $tab, options: [
                    (.endpoints, "Containers  \(info.endpoints.count)"),
                    (.ports, "Published Ports  \(totalPortCount)"),
                ])

                if tab == .endpoints {
                    endpointsContent
                } else {
                    portsContent
                }
            }
            .padding(.horizontal, 28)
            .padding(.top, 6)
            .padding(.bottom, 22)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        }
        .background(G.background)
    }

    // MARK: Summary cards

    private var summaryCards: some View {
        HStack(spacing: 14) {
            // 1. Container switch + NAT
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    GCaption("Container Switch")
                    Spacer()
                    GBadge(info.connectedPorts > 0 ? "Active" : "Idle", color: info.connectedPorts > 0 ? G.running : G.muted)
                }
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(info.subnet).font(G.mono(16, weight: .semibold)).foregroundStyle(G.text)
                    Text("gw \(info.gateway)").font(G.mono(11.5)).foregroundStyle(G.faint)
                }
                Text("\(info.driver) · \(info.connectedPorts) \(info.connectedPorts == 1 ? "port" : "ports") · \(info.learnedMacs) MACs · \(Format.bytes(info.bytesForwarded)) forwarded")
                    .font(.system(size: 12))
                    .foregroundStyle(G.muted)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .gravitonCard(G.card)

            // 2. Embedded DNS
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    GCaption("Embedded DNS")
                    Spacer()
                    GBadge(".\(info.dnsDomain)", color: G.accent)
                }
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(info.dnsServer).font(G.mono(16, weight: .semibold)).foregroundStyle(G.text)
                    Text("UDP + TCP").font(.system(size: 11.5)).foregroundStyle(G.faint)
                }
                let dnsCount = runningEndpoints.filter { $0.ipAddress != nil }.count
                Text(dnsCount == 0
                     ? "Resolves running containers by name, .\(info.dnsDomain) and ID"
                     : "\(dnsCount) \(dnsCount == 1 ? "container" : "containers") resolvable by name, .\(info.dnsDomain) & PTR")
                    .font(.system(size: 12))
                    .foregroundStyle(G.muted)
                    .lineLimit(1)
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .gravitonCard(G.card)

            // 3. Published ports
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    GCaption("Published Ports")
                    Spacer()
                    GBadge("\(activePortCount) live", color: activePortCount > 0 ? G.running : G.muted)
                }
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text("\(activeTcpCount) TCP").font(.system(size: 16, weight: .semibold)).foregroundStyle(G.text)
                    Text("·").foregroundStyle(G.faint)
                    Text("\(activeUdpCount) UDP").font(.system(size: 16, weight: .semibold)).foregroundStyle(activeUdpCount > 0 ? G.accent : G.muted)
                }
                let lanCount = runningEndpoints.flatMap(\.ports).filter { $0.bindIp != "127.0.0.1" }.count
                Text(lanCount > 0
                     ? "\(lanCount) bound on LAN/all interfaces · \(activePortCount - lanCount) loopback"
                     : "Forwarded on 127.0.0.1 (pass 0.0.0.0:HOST:PORT for LAN)")
                    .font(.system(size: 12))
                    .foregroundStyle(G.muted)
                    .lineLimit(1)
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .gravitonCard(G.card)
        }
    }

    // MARK: Tables

    @ViewBuilder private var endpointsContent: some View {
        if info.endpoints.isEmpty {
            GEmptyState("No containers on the network", symbol: "point.3.connected.trianglepath.dotted",
                        detail: "Every container joins the shared switch automatically and gets an IP address and DNS name.")
                .gravitonCard()
        } else if filteredEndpoints.isEmpty {
            GEmptyState("Nothing matches", symbol: "magnifyingglass", detail: "No container, IP or port matches “\(search)”.")
                .gravitonCard()
        } else {
            EndpointTable(rows: filteredEndpoints, selection: $selection, onOpen: openContainer)
        }
    }

    @ViewBuilder private var portsContent: some View {
        if totalPortCount == 0 {
            GEmptyState("No published ports", symbol: "network",
                        detail: "Publish a container's TCP or UDP port with -p [IP:]HOST:CONTAINER[/udp] or in the Run sheet.")
                .gravitonCard()
        } else if publishedPorts.isEmpty {
            GEmptyState("Nothing matches", symbol: "magnifyingglass", detail: "No published port matches “\(search)”.")
                .gravitonCard()
        } else {
            PublishedPortTable(rows: publishedPorts, onOpen: openContainer)
        }
    }

    private func openContainer(_ id: String, tab: DetailTab = .inspect) {
        router.pending = .container(id, tab)
    }
}

// MARK: - Endpoints table

private struct EndpointTable: View {
    var rows: [NetworkEndpoint]
    @Binding var selection: NetworkEndpoint.ID?
    var onOpen: (String, DetailTab) -> Void
    @State private var sort: Column = .status
    @State private var ascending = true
    @FocusState private var focused: Bool

    enum Column: Hashable { case name, ip, dns, ports, status }

    private var sorted: [NetworkEndpoint] {
        let out = rows.sorted { a, b in
            switch sort {
            case .name: a.containerName.localizedStandardCompare(b.containerName) == .orderedAscending
            case .ip: (a.ipAddress ?? "~").localizedStandardCompare(b.ipAddress ?? "~") == .orderedAscending
            case .dns: (a.dnsNames.first ?? "~").localizedStandardCompare(b.dnsNames.first ?? "~") == .orderedAscending
            case .ports: a.ports.count > b.ports.count
            case .status:
                a.isRunning != b.isRunning ? a.isRunning : a.containerName.localizedStandardCompare(b.containerName) == .orderedAscending
            }
        }
        return ascending ? out : out.reversed()
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                GColumnHeader("Container", key: Column.name, sort: $sort, ascending: $ascending).frame(minWidth: 160, maxWidth: .infinity)
                GColumnHeader("IP Address", key: Column.ip, sort: $sort, ascending: $ascending).frame(width: 132)
                GColumnHeader("DNS Names", key: Column.dns, sort: $sort, ascending: $ascending).frame(minWidth: 170, maxWidth: .infinity)
                GColumnHeader("Published Ports", key: Column.ports, sort: $sort, ascending: $ascending).frame(minWidth: 160, maxWidth: .infinity)
                GColumnHeader("Status", key: Column.status, sort: $sort, ascending: $ascending).frame(width: 84)
                Color.clear.frame(width: 28)
            }
            .padding(.horizontal, 14)
            .frame(height: 34)
            .overlay(alignment: .bottom) { Rectangle().fill(G.border).frame(height: 1) }

            ScrollView {
                LazyVStack(spacing: 2) {
                    ForEach(sorted) { ep in
                        row(ep)
                    }
                }
                .padding(6)
            }
        }
        .gravitonCard(G.surface)
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        .onKeyPress(.downArrow) { move(1) }
        .onKeyPress(.upArrow) { move(-1) }
        .onKeyPress(.return) {
            guard let selection else { return .ignored }
            onOpen(selection, .inspect)
            return .handled
        }
    }

    private func row(_ ep: NetworkEndpoint) -> some View {
        HStack(spacing: 12) {
            HStack(spacing: 10) {
                StatusDot(running: ep.isRunning)
                VStack(alignment: .leading, spacing: 2) {
                    Text(ep.containerName).font(.system(size: 13, weight: .medium)).foregroundStyle(G.text).lineLimit(1)
                    Text(ep.image).font(G.mono(11)).foregroundStyle(G.muted).lineLimit(1).truncationMode(.middle)
                }
            }
            .frame(minWidth: 160, maxWidth: .infinity, alignment: .leading)

            Text(ep.ipAddress ?? "—")
                .font(G.mono(12))
                .foregroundStyle(ep.ipAddress != nil ? G.text : G.faint)
                .textSelection(.enabled)
                .frame(width: 132, alignment: .leading)

            Group {
                if ep.dnsNames.isEmpty {
                    Text("—").font(G.mono(12)).foregroundStyle(G.faint)
                } else {
                    HStack(spacing: 6) {
                        Text(ep.dnsNames.prefix(2).joined(separator: ", "))
                            .font(G.mono(11.5))
                            .foregroundStyle(G.textSecondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
            }
            .frame(minWidth: 170, maxWidth: .infinity, alignment: .leading)

            Text(ep.ports.isEmpty ? "—" : ep.ports.map(\.compactLabel).joined(separator: ", "))
                .font(G.mono(11.5))
                .foregroundStyle(ep.ports.isEmpty ? G.faint : G.textSecondary)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(minWidth: 160, maxWidth: .infinity, alignment: .leading)

            GBadge(ep.isRunning ? "Connected" : "Stopped", color: ep.isRunning ? G.running : G.muted)
                .frame(width: 84, alignment: .leading)

            Menu { menuItems(ep) } label: {
                Image(systemName: "ellipsis").font(.system(size: 12, weight: .bold)).foregroundStyle(G.muted)
                    .frame(width: 26, height: 26).contentShape(Rectangle())
            }
            .gravitonMenu()
            .frame(width: 28)
            .accessibilityLabel("More actions for \(ep.containerName)")
        }
        .padding(.horizontal, 8)
        .frame(height: 50)
        .gravitonRow(selected: selection == ep.id)
        .onTapGesture(count: 2) { onOpen(ep.containerId, .inspect) }
        .simultaneousGesture(TapGesture().onEnded {
            selection = ep.id
            focused = true
        })
        .contextMenu { menuItems(ep) }
    }

    @ViewBuilder private func menuItems(_ ep: NetworkEndpoint) -> some View {
        Button("Inspect \(ep.containerName)") { onOpen(ep.containerId, .inspect) }
        if ep.isRunning {
            Button("Open Terminal") { onOpen(ep.containerId, .terminal) }
        }
        Divider()
        if let ip = ep.ipAddress {
            Button("Copy IP Address (\(ip))") { copyToPasteboard(ip) }
        }
        if ep.dnsNames.count >= 2 {
            Button("Copy DNS Name (\(ep.dnsNames[1]))") { copyToPasteboard(ep.dnsNames[1]) }
        }
    }

    private func move(_ by: Int) -> KeyPress.Result {
        let list = sorted
        guard !list.isEmpty else { return .ignored }
        let index = list.firstIndex { $0.id == selection } ?? (by > 0 ? -1 : list.count)
        selection = list[min(max(index + by, 0), list.count - 1)].id
        return .handled
    }
}

// MARK: - Published ports table

private struct PublishedPortRow: Identifiable {
    let endpoint: NetworkEndpoint
    let port: PortMapping
    var id: String { "\(endpoint.containerId)|\(port.bindIp)|\(port.hostPort)|\(port.containerPort)|\(port.protocol.rawValue)" }

    var isLAN: Bool { port.bindIp != "127.0.0.1" && port.bindIp != "::1" }
    var hostAddress: String {
        port.bindIp.contains(":") ? "[\(port.bindIp)]:\(port.hostPort)" : "\(port.bindIp):\(port.hostPort)"
    }
    var browserURL: URL? {
        guard port.protocol == .tcp else { return nil }
        let host = (port.bindIp == "0.0.0.0" || port.bindIp == "::") ? "127.0.0.1" : port.bindIp
        let formatted = host.contains(":") ? "[\(host)]" : host
        return URL(string: "http://\(formatted):\(port.hostPort)")
    }
}

private struct PublishedPortTable: View {
    var rows: [PublishedPortRow]
    var onOpen: (String, DetailTab) -> Void
    @State private var sort: Column = .hostPort
    @State private var ascending = true

    enum Column: Hashable { case hostPort, containerPort, proto, container, scope }

    private var sorted: [PublishedPortRow] {
        let out = rows.sorted { a, b in
            switch sort {
            case .hostPort: a.port.hostPort < b.port.hostPort
            case .containerPort: a.port.containerPort < b.port.containerPort
            case .proto: a.port.protocol.rawValue < b.port.protocol.rawValue
            case .container: a.endpoint.containerName.localizedStandardCompare(b.endpoint.containerName) == .orderedAscending
            case .scope: (a.isLAN && !b.isLAN)
            }
        }
        return ascending ? out : out.reversed()
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                GColumnHeader("Host Binding", key: Column.hostPort, sort: $sort, ascending: $ascending).frame(minWidth: 160, maxWidth: .infinity)
                GColumnHeader("Container Port", key: Column.containerPort, sort: $sort, ascending: $ascending).frame(width: 120)
                GColumnHeader("Protocol", key: Column.proto, sort: $sort, ascending: $ascending).frame(width: 78)
                GColumnHeader("Scope", key: Column.scope, sort: $sort, ascending: $ascending).frame(width: 90)
                GColumnHeader("Container", key: Column.container, sort: $sort, ascending: $ascending).frame(minWidth: 160, maxWidth: .infinity)
                Color.clear.frame(width: 28)
            }
            .padding(.horizontal, 14)
            .frame(height: 34)
            .overlay(alignment: .bottom) { Rectangle().fill(G.border).frame(height: 1) }

            ScrollView {
                LazyVStack(spacing: 2) {
                    ForEach(sorted) { item in
                        HStack(spacing: 12) {
                            Text(item.hostAddress)
                                .font(G.mono(12.5, weight: .medium))
                                .foregroundStyle(item.endpoint.isRunning ? G.text : G.muted)
                                .frame(minWidth: 160, maxWidth: .infinity, alignment: .leading)

                            Text("→ \(item.port.containerPort)")
                                .font(G.mono(12))
                                .foregroundStyle(G.textSecondary)
                                .frame(width: 120, alignment: .leading)

                            GBadge(item.port.protocol.rawValue.uppercased(), color: item.port.protocol == .udp ? G.accent : G.muted)
                                .frame(width: 78, alignment: .leading)

                            GBadge(item.isLAN ? "LAN" : "Loopback", color: item.isLAN ? G.warning : G.muted)
                                .frame(width: 90, alignment: .leading)

                            Button { onOpen(item.endpoint.containerId, .inspect) } label: {
                                HStack(spacing: 8) {
                                    StatusDot(running: item.endpoint.isRunning, size: 7)
                                    Text(item.endpoint.containerName).font(.system(size: 12.5, weight: .medium)).foregroundStyle(G.text).lineLimit(1)
                                }
                            }
                            .buttonStyle(.plain)
                            .frame(minWidth: 160, maxWidth: .infinity, alignment: .leading)

                            Menu {
                                Button("Open \(item.endpoint.containerName)") { onOpen(item.endpoint.containerId, .inspect) }
                                Button("Copy Address (\(item.hostAddress))") { copyToPasteboard(item.hostAddress) }
                                if item.endpoint.isRunning, let url = item.browserURL {
                                    Button("Open in Browser") { NSWorkspace.shared.open(url) }
                                }
                            } label: {
                                Image(systemName: "ellipsis").font(.system(size: 12, weight: .bold)).foregroundStyle(G.muted)
                                    .frame(width: 26, height: 26).contentShape(Rectangle())
                            }
                            .gravitonMenu()
                            .frame(width: 28)
                        }
                        .padding(.horizontal, 8)
                        .frame(height: 44)
                        .gravitonRow(selected: false)
                        .onTapGesture(count: 2) { onOpen(item.endpoint.containerId, .inspect) }
                    }
                }
                .padding(6)
            }
        }
        .gravitonCard(G.surface)
    }
}

private func copyToPasteboard(_ text: String) {
    NSPasteboard.general.clearContents()
    NSPasteboard.general.setString(text, forType: .string)
}
