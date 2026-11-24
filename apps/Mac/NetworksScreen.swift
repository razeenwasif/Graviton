import AppKit
import GravitonKit
import SwiftUI

struct NetworksScreen: View {
    @Environment(EngineStore.self) private var store
    @Environment(AppRouter.self) private var router
    @Environment(Dialogs.self) private var dialogs
    @State private var tab: Tab = .networks
    @State private var search = ""
    @State private var selection: NetworkEndpoint.ID?
    @State private var networkSelection: UserNetwork.ID?
    @State private var showCreateNetwork = false

    enum Tab: String, CaseIterable, Identifiable {
        case networks = "Networks", endpoints = "Containers", ports = "Published Ports"
        var id: String { rawValue }
    }

    private var info: NetworkInfo {
        store.network ?? NetworkInfo.synthesized(from: store.containers)
    }

    private var userNetworks: [UserNetwork] {
        !store.networks.isEmpty ? store.networks : info.networks
    }

    private var filteredNetworks: [UserNetwork] {
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !q.isEmpty else { return userNetworks }
        return userNetworks.filter { net in
            net.name.lowercased().contains(q)
                || net.driver.lowercased().contains(q)
                || net.containers.contains { $0.lowercased().contains(q) }
                || net.labels.contains { $0.key.lowercased().contains(q) || $0.value.lowercased().contains(q) }
        }
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
                || ep.attachedNetworks.contains { $0.lowercased().contains(q) }
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
        let netCount = userNetworks.count
        let conn = runningEndpoints.count
        let portLabel = activePortCount == 1 ? "1 active port" : "\(activePortCount) active ports"
        return "\(netCount) \(netCount == 1 ? "network" : "networks") · \(conn) connected · \(portLabel)"
    }

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader("Networks", subtitle: subtitle) {
                HeaderSearch(prompt: "Search networks, containers, IPs or ports", text: $search)
                Button { Task { await store.refresh() } } label: { Image(systemName: "arrow.clockwise") }
                    .buttonStyle(.gIconBordered)
                    .help("Refresh")
                Button("New Network") { showCreateNetwork = true }
                    .buttonStyle(.gPrimary)
                    .disabled(!store.canManage && store.connection != .demo)
            }

            VStack(alignment: .leading, spacing: 16) {
                summaryCards

                GSegmented(selection: $tab, options: [
                    (.networks, "Networks  \(userNetworks.count)"),
                    (.endpoints, "Containers  \(info.endpoints.count)"),
                    (.ports, "Published Ports  \(totalPortCount)"),
                ])

                switch tab {
                case .networks: networksContent
                case .endpoints: endpointsContent
                case .ports: portsContent
                }
            }
            .padding(.horizontal, 28)
            .padding(.top, 6)
            .padding(.bottom, 22)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        }
        .background(G.background)
        .sheet(isPresented: $showCreateNetwork) {
            CreateNetworkSheet { created in
                networkSelection = created.id
                tab = .networks
            }
        }
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

    @ViewBuilder private var networksContent: some View {
        if userNetworks.isEmpty {
            GEmptyState("No networks", symbol: "network",
                        detail: "Create an isolated bridge network with New Network or grav network create.")
                .gravitonCard()
        } else if filteredNetworks.isEmpty {
            GEmptyState("Nothing matches", symbol: "magnifyingglass", detail: "No network or container matches “\(search)”.")
                .gravitonCard()
        } else {
            UserNetworksTable(
                rows: filteredNetworks,
                containers: store.containers,
                selection: $networkSelection,
                canManage: store.canManage || store.connection == .demo,
                onOpenContainer: { openContainer($0, tab: .inspect) },
                onConnect: connectNetwork,
                onDisconnect: disconnectNetwork,
                onRemove: confirmRemoveNetwork
            )
        }
    }

    @ViewBuilder private var endpointsContent: some View {
        if info.endpoints.isEmpty {
            GEmptyState("No containers on the network", symbol: "point.3.connected.trianglepath.dotted",
                        detail: "Every container joins a switch network automatically and gets an IP address and DNS name.")
                .gravitonCard()
        } else if filteredEndpoints.isEmpty {
            GEmptyState("Nothing matches", symbol: "magnifyingglass", detail: "No container, network, IP or port matches “\(search)”.")
                .gravitonCard()
        } else {
            EndpointTable(
                rows: filteredEndpoints,
                userNetworks: userNetworks,
                canManage: store.canManage || store.connection == .demo,
                selection: $selection,
                onOpen: openContainer,
                onConnect: connectNetwork,
                onDisconnect: disconnectNetwork
            )
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

    private func connectNetwork(_ network: String, container: String) {
        Task {
            do {
                _ = try await store.connectNetwork(network, container: container)
            } catch {
                dialogs.alert("Couldn't connect \(container) to \(network)", message: error.localizedDescription)
            }
        }
    }

    private func disconnectNetwork(_ network: String, container: String) {
        Task {
            do {
                _ = try await store.disconnectNetwork(network, container: container)
            } catch {
                dialogs.alert("Couldn't disconnect \(container) from \(network)", message: error.localizedDescription)
            }
        }
    }

    private func confirmRemoveNetwork(_ net: UserNetwork) {
        dialogs.confirm(
            "Remove network \(net.name)?",
            message: "Deletes network \(net.name). Any stopped or running containers must be disconnected first.",
            confirm: "Remove"
        ) {
            Task {
                do {
                    _ = try await store.removeNetwork(net.name)
                } catch {
                    dialogs.alert("Couldn't remove \(net.name)", message: error.localizedDescription)
                }
            }
        }
    }
}

// MARK: - Endpoints table

private struct EndpointTable: View {
    var rows: [NetworkEndpoint]
    var userNetworks: [UserNetwork]
    var canManage: Bool
    @Binding var selection: NetworkEndpoint.ID?
    var onOpen: (String, DetailTab) -> Void
    var onConnect: (String, String) -> Void
    var onDisconnect: (String, String) -> Void
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
                    HStack(spacing: 6) {
                        Text(ep.containerName).font(.system(size: 13, weight: .medium)).foregroundStyle(G.text).lineLimit(1)
                        ForEach(ep.attachedNetworks, id: \.self) { net in
                            GBadge(net, color: net == "graviton" ? G.muted : G.accent)
                        }
                    }
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
        if canManage {
            let attached = Set(ep.attachedNetworks)
            let joinable = userNetworks.filter { !attached.contains($0.name) }
            if !joinable.isEmpty || !ep.attachedNetworks.isEmpty {
                Divider()
            }
            if !joinable.isEmpty {
                Menu("Connect to Network") {
                    ForEach(joinable) { net in
                        Button(net.name) { onConnect(net.name, ep.containerName) }
                    }
                }
            }
            if !ep.attachedNetworks.isEmpty {
                Menu("Disconnect from Network") {
                    ForEach(ep.attachedNetworks, id: \.self) { netName in
                        Button(netName) { onDisconnect(netName, ep.containerName) }
                    }
                }
            }
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

// MARK: - User-defined networks table

private struct UserNetworksTable: View {
    var rows: [UserNetwork]
    var containers: [Container]
    @Binding var selection: UserNetwork.ID?
    var canManage: Bool
    var onOpenContainer: (String) -> Void
    var onConnect: (String, String) -> Void
    var onDisconnect: (String, String) -> Void
    var onRemove: (UserNetwork) -> Void
    @State private var sort: Column = .name
    @State private var ascending = true

    enum Column: Hashable { case name, driver, endpoints, created }

    private var sorted: [UserNetwork] {
        let out = rows.sorted { a, b in
            switch sort {
            case .name:
                if a.isDefault != b.isDefault { return a.isDefault }
                return a.name.localizedStandardCompare(b.name) == .orderedAscending
            case .driver:
                return a.driver.localizedStandardCompare(b.driver) == .orderedAscending
            case .endpoints:
                return a.containers.count > b.containers.count
            case .created:
                return a.createdAt > b.createdAt
            }
        }
        return ascending ? out : out.reversed()
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                GColumnHeader("Network", key: Column.name, sort: $sort, ascending: $ascending).frame(minWidth: 180, maxWidth: .infinity)
                GColumnHeader("Driver", key: Column.driver, sort: $sort, ascending: $ascending).frame(width: 140)
                GColumnHeader("Attached Containers", key: Column.endpoints, sort: $sort, ascending: $ascending).frame(minWidth: 220, maxWidth: .infinity)
                GColumnHeader("Created", key: Column.created, sort: $sort, ascending: $ascending).frame(width: 140)
                Color.clear.frame(width: 28)
            }
            .padding(.horizontal, 14)
            .frame(height: 34)
            .overlay(alignment: .bottom) { Rectangle().fill(G.border).frame(height: 1) }

            ScrollView {
                LazyVStack(spacing: 2) {
                    ForEach(sorted) { net in
                        row(net)
                    }
                }
                .padding(6)
            }
        }
        .gravitonCard(G.surface)
    }

    private func row(_ net: UserNetwork) -> some View {
        HStack(spacing: 12) {
            HStack(spacing: 8) {
                Image(systemName: net.internalOnly ? "lock.shield" : "network")
                    .font(.system(size: 13))
                    .foregroundStyle(net.isDefault ? G.muted : G.accent)
                Text(net.name)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(G.text)
                    .lineLimit(1)
                if net.isDefault {
                    GBadge("Built-in", color: G.muted)
                } else if net.internalOnly {
                    GBadge("Internal", color: G.warning)
                }
            }
            .frame(minWidth: 180, maxWidth: .infinity, alignment: .leading)

            Text(net.driver)
                .font(G.mono(12))
                .foregroundStyle(G.textSecondary)
                .lineLimit(1)
                .frame(width: 140, alignment: .leading)

            attachedContainersView(net)
                .frame(minWidth: 220, maxWidth: .infinity, alignment: .leading)

            Text(net.createdAt == 0 ? "Built-in" : net.createdDate.formatted(.relative(presentation: .named)))
                .font(.system(size: 12))
                .foregroundStyle(G.muted)
                .lineLimit(1)
                .frame(width: 140, alignment: .leading)

            Menu { menuItems(net) } label: {
                Image(systemName: "ellipsis").font(.system(size: 12, weight: .bold)).foregroundStyle(G.muted)
                    .frame(width: 26, height: 26).contentShape(Rectangle())
            }
            .gravitonMenu()
            .frame(width: 28)
            .accessibilityLabel("More actions for \(net.name)")
        }
        .padding(.horizontal, 8)
        .frame(height: 46)
        .gravitonRow(selected: selection == net.id)
        .onTapGesture { selection = net.id }
        .contextMenu { menuItems(net) }
    }

    @ViewBuilder
    private func attachedContainersView(_ net: UserNetwork) -> some View {
        if net.containers.isEmpty {
            Text("—").font(G.mono(12)).foregroundStyle(G.faint)
        } else {
            HStack(spacing: 6) {
                GBadge("\(net.containers.count)", color: G.running)
                Text(net.containers.joined(separator: ", "))
                    .font(G.mono(11.5))
                    .foregroundStyle(G.textSecondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
        }
    }

    @ViewBuilder
    private func menuItems(_ net: UserNetwork) -> some View {
        let attachedSet = Set(net.containers)
        let joinable = containers.filter { !attachedSet.contains($0.name) }
        if canManage {
            Menu("Connect Container") {
                if joinable.isEmpty {
                    Text("All containers attached")
                } else {
                    ForEach(joinable) { c in
                        Button(c.name) { onConnect(net.name, c.name) }
                    }
                }
            }
            .disabled(joinable.isEmpty)

            Menu("Disconnect Container") {
                if net.containers.isEmpty {
                    Text("No attached containers")
                } else {
                    ForEach(net.containers, id: \.self) { cname in
                        Button(cname) { onDisconnect(net.name, cname) }
                    }
                }
            }
            .disabled(net.containers.isEmpty)
        }
        if let firstName = net.containers.first, let ctr = containers.first(where: { $0.name == firstName }) {
            Button("Inspect \(firstName)") { onOpenContainer(ctr.id) }
        }
        if canManage {
            Divider()
            Button("Remove Network", role: .destructive) { onRemove(net) }
                .disabled(net.isDefault || !net.containers.isEmpty)
        }
    }
}

// MARK: - Create network sheet

private struct CreateNetworkSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    var onCreated: (UserNetwork) -> Void
    @State private var name = ""
    @State private var internalOnly = false
    @State private var working = false
    @State private var problem: String?

    var body: some View {
        GSheet("New network", subtitle: "Creates an L2-isolated bridge network with scoped container DNS.", symbol: "network", width: 460) {
            GFormRow("Name", hint: "Start with a letter or digit; letters, digits, _, . and - are allowed.") {
                GTextField("e.g. app-net", text: $name)
                    .disabled(working)
                    .onSubmit(create)
                    .gravitonField(mono: true)
            }
            GFormRow("Isolation", hint: "Internal networks allow VM-to-VM traffic without outbound host NAT.") {
                Toggle("Internal only (no outbound NAT)", isOn: $internalOnly)
                    .toggleStyle(.gSwitch)
                    .disabled(working)
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
                let net = try await store.createNetwork(name: trimmed, internalOnly: internalOnly)
                onCreated(net)
                dismiss()
            } catch {
                working = false
                problem = error.localizedDescription
            }
        }
    }
}

private func copyToPasteboard(_ text: String) {
    NSPasteboard.general.clearContents()
    NSPasteboard.general.setString(text, forType: .string)
}
