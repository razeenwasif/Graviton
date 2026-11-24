import GravitonKit
import SwiftUI
import UIKit

/// Resources tab on iPhone: named volumes + bind mounts and the shared container network.
struct MobileResourcesView: View {
    @Environment(EngineStore.self) private var store
    @Environment(Dialogs.self) private var dialogs
    @State private var section: Section = .volumes
    @State private var volumeFilter: VolumeFilter = .named
    @State private var networkFilter: NetworkFilter = .networks
    @State private var search = ""
    @State private var showCreateVolume = false
    @State private var showCreateNetwork = false
    @State private var refreshing = false

    enum Section: String, CaseIterable, Identifiable {
        case volumes = "Volumes"
        case network = "Network"
        var id: String { rawValue }
    }

    enum VolumeFilter: String, CaseIterable, Identifiable {
        case named = "Named"
        case unused = "Unused"
        case bind = "Bind"
        var id: String { rawValue }
    }

    enum NetworkFilter: String, CaseIterable, Identifiable {
        case networks = "Networks"
        case containers = "Containers"
        case ports = "Ports"
        var id: String { rawValue }
    }

    private struct BindMountRow: Identifiable {
        var container: Container
        var mount: Mount
        var id: String { "\(container.id):\(mount.source):\(mount.target)" }
    }

    private struct PublishedPortRow: Identifiable {
        var endpoint: NetworkEndpoint
        var port: PortMapping
        var id: String { "\(endpoint.containerId):\(port.bindIp):\(port.hostPort)->\(port.containerPort)/\(port.protocol.rawValue)" }
    }

    private var totalVolumeBytes: UInt64 { store.volumes.map(\.sizeBytes).reduce(0, +) }
    private var unusedVolumes: [VolumeInfo] { store.volumes.filter { !$0.isInUse } }

    private var shownVolumes: [VolumeInfo] {
        let base = volumeFilter == .unused ? unusedVolumes : store.volumes
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !q.isEmpty else { return base }
        return base.filter { v in
            v.name.lowercased().contains(q)
                || v.path.lowercased().contains(q)
                || v.usedBy.contains { $0.containerName.lowercased().contains(q) || $0.target.lowercased().contains(q) }
        }
    }

    private var bindMounts: [BindMountRow] {
        let all = store.containers.flatMap { c in
            c.mounts.filter { $0.source.hasPrefix("/") || $0.source.hasPrefix("~") }.map { m in
                BindMountRow(container: c, mount: m)
            }
        }
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !q.isEmpty else { return all }
        return all.filter {
            $0.mount.source.lowercased().contains(q)
                || $0.mount.target.lowercased().contains(q)
                || $0.container.name.lowercased().contains(q)
        }
    }

    private var networkInfo: NetworkInfo {
        store.network ?? NetworkInfo.synthesized(from: store.containers)
    }

    private var userNetworks: [UserNetwork] {
        !store.networks.isEmpty ? store.networks : networkInfo.networks
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

    private var runningEndpoints: [NetworkEndpoint] { networkInfo.endpoints.filter(\.isRunning) }

    private var filteredEndpoints: [NetworkEndpoint] {
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !q.isEmpty else { return networkInfo.endpoints }
        return networkInfo.endpoints.filter { ep in
            ep.containerName.lowercased().contains(q)
                || ep.image.lowercased().contains(q)
                || (ep.ipAddress?.lowercased().contains(q) ?? false)
                || ep.dnsNames.contains { $0.lowercased().contains(q) }
                || ep.attachedNetworks.contains { $0.lowercased().contains(q) }
                || ep.ports.contains { $0.fullLabel.lowercased().contains(q) }
        }
    }

    private var publishedPorts: [PublishedPortRow] {
        let all = networkInfo.endpoints.flatMap { ep in
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

    private var activePortCount: Int { runningEndpoints.reduce(0) { $0 + $1.ports.count } }
    private var totalPortCount: Int { networkInfo.endpoints.reduce(0) { $0 + $1.ports.count } }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    if section == .volumes {
                        volumesBody
                    } else {
                        networkBody
                    }
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                VStack(alignment: .leading, spacing: 0) {
                    if section == .volumes {
                        MobileHeader("Volumes", subtitle: "\(store.volumes.count) named · \(Format.bytes(totalVolumeBytes))", large: true) {
                            Button { showCreateVolume = true } label: { Label("New volume", systemImage: "plus") }
                                .buttonStyle(CircleIconStyle(prominent: true))
                                .disabled(!store.canManage)
                        }
                    } else {
                        let portNoun = activePortCount == 1 ? "active port" : "active ports"
                        MobileHeader("Network", subtitle: "\(userNetworks.count) \(userNetworks.count == 1 ? "network" : "networks") · \(runningEndpoints.count) connected · \(activePortCount) \(portNoun)", large: true) {
                            HStack(spacing: 10) {
                                Button { Task { await reload() } } label: { Label("Refresh", systemImage: "arrow.clockwise") }
                                    .buttonStyle(CircleIconStyle())
                                    .disabled(refreshing)
                                Button { showCreateNetwork = true } label: { Label("New network", systemImage: "plus") }
                                    .buttonStyle(CircleIconStyle(prominent: true))
                                    .disabled(!store.canManage)
                            }
                        }
                    }
                    GSegmented(selection: $section, options: [
                        (.volumes, "Volumes  \(store.volumes.count)"),
                        (.network, "Network  \(userNetworks.count)"),
                    ], stretch: true)
                    .padding(.horizontal, 20)
                    .padding(.bottom, 12)
                }
                .background(G.background)
            }
            .refreshable { await reload() }
            .mobileScreen(pushed: false)
            .navigationDestination(for: String.self) { ContainerDetailView(containerID: $0) }
            .sheet(isPresented: $showCreateVolume) { MobileCreateVolumeSheet() }
            .sheet(isPresented: $showCreateNetwork) { MobileCreateNetworkSheet() }
            .onAppear { Task { await store.loadVolumes() } }
        }
    }

    // MARK: - Volumes

    @ViewBuilder
    private var volumesBody: some View {
        MobileSearchField(prompt: "Search volumes", text: $search)

        GSegmented(selection: $volumeFilter, options: [
            (.named, "Named \(store.volumes.count)"),
            (.unused, "Unused \(unusedVolumes.count)"),
            (.bind, "Bind \(bindMounts.count)"),
        ], stretch: true)

        if volumeFilter == .bind {
            if bindMounts.isEmpty {
                GEmptyState("No bind mounts", symbol: "folder",
                            detail: search.isEmpty ? "Pass -v /host/dir:/path when starting a container to share a Mac folder."
                                : "No bind mount matches “\(search)”.")
                    .frame(minHeight: 300)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(bindMounts.enumerated()), id: \.element.id) { idx, row in
                        if idx > 0 { RowDivider(inset: 14) }
                        NavigationLink(value: row.container.id) {
                            HStack(alignment: .top, spacing: 12) {
                                StatusDot(running: row.container.isRunning, size: 8).padding(.top, 6)
                                VStack(alignment: .leading, spacing: 3) {
                                    HStack(spacing: 6) {
                                        Text(row.container.name).font(.system(size: 14.5, weight: .semibold)).foregroundStyle(G.text)
                                        if row.mount.readOnly {
                                            GBadge("RO", color: G.warning)
                                        }
                                        Spacer()
                                        Text(row.mount.target).font(G.mono(11.5)).foregroundStyle(G.accent).lineLimit(1)
                                    }
                                    Text(row.mount.source).font(G.mono(11)).foregroundStyle(G.muted).lineLimit(2)
                                }
                                Image(systemName: "chevron.right").font(.system(size: 12, weight: .semibold)).foregroundStyle(Color(hex: 0x5F6672)).padding(.top, 4)
                            }
                            .padding(.horizontal, 14)
                            .padding(.vertical, 12)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(PressRowStyle())
                    }
                }
                .mobileCard()
            }
        } else if shownVolumes.isEmpty {
            GEmptyState(volumeFilter == .unused ? "No unused volumes" : "No named volumes",
                        symbol: "internaldrive",
                        detail: volumeFilter == .unused
                            ? "Every named volume is currently attached to a container."
                            : "Create a named volume to persist data across container runs.",
                        actionTitle: volumeFilter == .named && store.canManage ? "New volume" : nil) { showCreateVolume = true }
                .frame(minHeight: 300)
        } else {
            VStack(spacing: 0) {
                ForEach(Array(shownVolumes.enumerated()), id: \.element.id) { idx, volume in
                    if idx > 0 { RowDivider(inset: 14) }
                    volumeRow(volume)
                }
            }
            .mobileCard()
        }
    }

    private func volumeRow(_ volume: VolumeInfo) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Image(systemName: "internaldrive")
                    .font(.system(size: 14, weight: .medium))
                    .foregroundStyle(volume.isInUse ? G.accent : G.muted)
                    .frame(width: 32, height: 32)
                    .background((volume.isInUse ? G.accent : G.muted).opacity(0.12), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                VStack(alignment: .leading, spacing: 2) {
                    Text(volume.name).font(G.mono(14, weight: .semibold)).foregroundStyle(G.text).lineLimit(1)
                    Text("\(Format.bytes(volume.sizeBytes)) · \(volume.fileCount) \(volume.fileCount == 1 ? "file" : "files")")
                        .font(.system(size: 12)).foregroundStyle(G.muted)
                }
                Spacer()
                if volume.isInUse {
                    GBadge("\(volume.usedBy.count) mounted", color: G.running)
                } else {
                    GBadge("Unused", color: G.faint)
                    if store.canManage {
                        Button { confirmRemove(volume) } label: {
                            Image(systemName: "trash").font(.system(size: 12.5)).foregroundStyle(G.danger)
                                .frame(width: 30, height: 30)
                                .background(G.danger.opacity(0.1), in: Circle())
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel("Remove volume \(volume.name)")
                    }
                }
            }

            if !volume.usedBy.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 6) {
                        ForEach(volume.usedBy) { mount in
                            NavigationLink(value: mount.containerId) {
                                HStack(spacing: 5) {
                                    StatusDot(running: mount.running, size: 6)
                                    Text(mount.containerName).font(.system(size: 11.5, weight: .medium)).foregroundStyle(G.text)
                                    Text(mount.target).font(G.mono(10.5)).foregroundStyle(G.muted)
                                    if mount.readOnly {
                                        Text(":ro").font(G.mono(10)).foregroundStyle(G.warning)
                                    }
                                }
                                .padding(.horizontal, 9)
                                .padding(.vertical, 5)
                                .background(G.control, in: Capsule())
                                .overlay(Capsule().strokeBorder(G.border))
                            }
                            .buttonStyle(.plain)
                        }
                    }
                }
            }
        }
        .padding(14)
        .contextMenu {
            Button("Copy Path", systemImage: "doc.on.doc") { UIPasteboard.general.string = volume.path }
            if !volume.isInUse && store.canManage {
                Button("Remove Volume", systemImage: "trash", role: .destructive) { confirmRemove(volume) }
            }
        }
    }

    // MARK: - Network

    @ViewBuilder
    private var networkBody: some View {
        HStack(spacing: 10) {
            SummaryTile(title: "Subnet", symbol: "network") {
                Text(networkInfo.subnet).font(G.mono(13, weight: .semibold)).foregroundStyle(G.text).lineLimit(1).minimumScaleFactor(0.75)
            }
            SummaryTile(title: "DNS", symbol: "globe") {
                Text(".\(networkInfo.dnsDomain)").font(G.mono(14, weight: .semibold)).foregroundStyle(G.text).lineLimit(1)
            }
            SummaryTile(title: "Traffic", symbol: "arrow.left.arrow.right") {
                Text(Format.bytes(networkInfo.bytesForwarded)).font(G.mono(14, weight: .semibold)).foregroundStyle(G.text).lineLimit(1).minimumScaleFactor(0.75)
            }
        }

        MobileSearchField(prompt: "Search networks, containers, IPs or ports", text: $search)

        GSegmented(selection: $networkFilter, options: [
            (.networks, "Networks \(userNetworks.count)"),
            (.containers, "Containers \(networkInfo.endpoints.count)"),
            (.ports, "Ports \(totalPortCount)"),
        ], stretch: true)

        if networkFilter == .networks {
            if filteredNetworks.isEmpty {
                GEmptyState("No networks", symbol: "network",
                            detail: search.isEmpty ? "Tap + to create an isolated container bridge network."
                                : "No network matches “\(search)”.")
                    .frame(minHeight: 260)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(filteredNetworks.enumerated()), id: \.element.id) { idx, net in
                        if idx > 0 { RowDivider(inset: 14) }
                        networkRow(net)
                            .contextMenu {
                                if store.canManage {
                                    let joinable = store.containers.filter { !net.containers.contains($0.name) }
                                    if !joinable.isEmpty {
                                        Menu("Connect Container", systemImage: "link.badge.plus") {
                                            ForEach(joinable) { c in
                                                Button(c.name) { connect(net.name, container: c.name) }
                                            }
                                        }
                                    }
                                    if !net.containers.isEmpty {
                                        Menu("Disconnect Container", systemImage: "link") {
                                            ForEach(net.containers, id: \.self) { cname in
                                                Button(cname) { disconnect(net.name, container: cname) }
                                            }
                                        }
                                    }
                                    if !net.isDefault && net.containers.isEmpty {
                                        Divider()
                                        Button("Remove Network", systemImage: "trash", role: .destructive) { confirmRemove(net) }
                                    }
                                }
                            }
                    }
                }
                .mobileCard()
            }
        } else if networkFilter == .containers {
            if filteredEndpoints.isEmpty {
                GEmptyState("No containers on network", symbol: "network",
                            detail: search.isEmpty ? "Containers get a DHCP IP on \(networkInfo.subnet) when started."
                                : "No endpoint matches “\(search)”.")
                    .frame(minHeight: 260)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(filteredEndpoints.enumerated()), id: \.element.id) { idx, ep in
                        if idx > 0 { RowDivider(inset: 14) }
                        NavigationLink(value: ep.containerId) {
                            endpointRow(ep)
                        }
                        .buttonStyle(PressRowStyle())
                        .contextMenu {
                            if let ip = ep.ipAddress {
                                Button("Copy IP Address", systemImage: "doc.on.doc") { UIPasteboard.general.string = ip }
                            }
                            if let host = ep.dnsNames.first {
                                Button("Copy Hostname", systemImage: "doc.on.doc") { UIPasteboard.general.string = host }
                            }
                            if store.canManage {
                                let attached = Set(ep.attachedNetworks)
                                let joinable = userNetworks.filter { !attached.contains($0.name) }
                                if !joinable.isEmpty {
                                    Menu("Connect to Network", systemImage: "link.badge.plus") {
                                        ForEach(joinable) { net in
                                            Button(net.name) { connect(net.name, container: ep.containerName) }
                                        }
                                    }
                                }
                                if !ep.attachedNetworks.isEmpty {
                                    Menu("Disconnect from Network", systemImage: "link") {
                                        ForEach(ep.attachedNetworks, id: \.self) { netName in
                                            Button(netName) { disconnect(netName, container: ep.containerName) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                .mobileCard()
            }
        } else if publishedPorts.isEmpty {
            GEmptyState("No published ports", symbol: "arrow.left.arrow.right",
                        detail: search.isEmpty ? "Use -p HOST:CONTAINER when running a container to publish a TCP or UDP port."
                            : "No published port matches “\(search)”.")
                .frame(minHeight: 260)
        } else {
            VStack(spacing: 0) {
                ForEach(Array(publishedPorts.enumerated()), id: \.element.id) { idx, row in
                    if idx > 0 { RowDivider(inset: 14) }
                    NavigationLink(value: row.endpoint.containerId) {
                        portRow(row)
                    }
                    .buttonStyle(PressRowStyle())
                    .contextMenu {
                        Button("Copy Host Address", systemImage: "doc.on.doc") {
                            UIPasteboard.general.string = "\(row.port.bindIp):\(row.port.hostPort)"
                        }
                    }
                }
            }
            .mobileCard()
        }
    }

    private func networkRow(_ net: UserNetwork) -> some View {
        HStack(spacing: 12) {
            Image(systemName: net.internalOnly ? "lock.shield" : "network")
                .font(.system(size: 15, weight: .medium))
                .foregroundStyle(net.isDefault ? G.muted : G.accent)
                .frame(width: 24)
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(net.name).font(.system(size: 15, weight: .semibold)).foregroundStyle(G.text).lineLimit(1)
                    if net.isDefault {
                        GBadge("Built-in", color: G.muted)
                    } else if net.internalOnly {
                        GBadge("Internal", color: G.warning)
                    }
                }
                Text(net.containers.isEmpty ? "\(net.driver) · No containers" : "\(net.driver) · \(net.containers.joined(separator: ", "))")
                    .font(G.mono(11.5))
                    .foregroundStyle(G.muted)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer()
            GBadge("\(net.containers.count)", color: net.containers.isEmpty ? G.muted : G.running)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
        .contentShape(Rectangle())
    }

    private func endpointRow(_ ep: NetworkEndpoint) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 10) {
                StatusDot(running: ep.isRunning, size: 8)
                VStack(alignment: .leading, spacing: 1) {
                    Text(ep.containerName).font(.system(size: 15, weight: .semibold)).foregroundStyle(G.text).lineLimit(1)
                    Text(ep.image).font(G.mono(11)).foregroundStyle(G.muted).lineLimit(1)
                }
                Spacer()
                if let ip = ep.ipAddress {
                    Text(ip).font(G.mono(12.5, weight: .medium)).foregroundStyle(G.accent)
                } else {
                    Text("Stopped").font(.system(size: 12)).foregroundStyle(G.faint)
                }
                Image(systemName: "chevron.right").font(.system(size: 12, weight: .semibold)).foregroundStyle(Color(hex: 0x5F6672))
            }

            if !ep.attachedNetworks.isEmpty || !ep.dnsNames.isEmpty || !ep.ports.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 6) {
                        ForEach(ep.attachedNetworks, id: \.self) { net in
                            GBadge(net, color: net == "graviton" ? G.muted : G.running)
                        }
                        ForEach(ep.dnsNames, id: \.self) { dns in
                            GBadge(dns, color: G.textSecondary)
                        }
                        ForEach(ep.ports, id: \.fullLabel) { p in
                            GBadge(p.compactLabel, color: G.accent)
                        }
                    }
                }
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
        .contentShape(Rectangle())
    }

    private func portRow(_ row: PublishedPortRow) -> some View {
        HStack(spacing: 12) {
            StatusDot(running: row.endpoint.isRunning, size: 8)
            VStack(alignment: .leading, spacing: 2) {
                Text(row.port.label).font(G.mono(14, weight: .semibold)).foregroundStyle(G.text)
                HStack(spacing: 6) {
                    Text(row.endpoint.containerName).font(.system(size: 12.5, weight: .medium)).foregroundStyle(G.muted)
                    Text("·").foregroundStyle(G.faint)
                    Text(row.port.bindIp).font(G.mono(11)).foregroundStyle(G.faint)
                }
            }
            Spacer()
            GBadge(row.port.protocol.rawValue.uppercased(), color: row.port.protocol == .udp ? G.warning : G.accent)
            Image(systemName: "chevron.right").font(.system(size: 12, weight: .semibold)).foregroundStyle(Color(hex: 0x5F6672))
        }
        .padding(.horizontal, 14)
        .frame(minHeight: 60)
        .contentShape(Rectangle())
    }

    // MARK: - Actions

    private func reload() async {
        refreshing = true
        await store.refresh()
        await store.loadVolumes()
        refreshing = false
    }

    private func confirmRemove(_ volume: VolumeInfo) {
        dialogs.confirm("Remove volume \(volume.name)?",
                        message: "Deletes its directory (\(Format.bytes(volume.sizeBytes)), \(volume.fileCount) files) on \(store.liveName ?? "the Mac").",
                        confirm: "Remove") {
            Task {
                do {
                    _ = try await store.removeVolume(volume)
                } catch {
                    dialogs.alert("Couldn't remove volume", message: error.localizedDescription)
                }
            }
        }
    }

    private func connect(_ network: String, container: String) {
        Task {
            do {
                _ = try await store.connectNetwork(network, container: container)
            } catch {
                dialogs.alert("Couldn't connect \(container)", message: error.localizedDescription)
            }
        }
    }

    private func disconnect(_ network: String, container: String) {
        Task {
            do {
                _ = try await store.disconnectNetwork(network, container: container)
            } catch {
                dialogs.alert("Couldn't disconnect \(container)", message: error.localizedDescription)
            }
        }
    }

    private func confirmRemove(_ network: UserNetwork) {
        dialogs.confirm("Remove network \(network.name)?",
                        message: "Removes the isolated bridge network \(network.name) from \(store.liveName ?? "the Mac").",
                        confirm: "Remove") {
            Task {
                do {
                    _ = try await store.removeNetwork(network.name)
                } catch {
                    dialogs.alert("Couldn't remove network", message: error.localizedDescription)
                }
            }
        }
    }
}

private struct MobileCreateVolumeSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var phase: Phase = .idle
    @FocusState private var focused: Bool

    enum Phase {
        case idle, creating
        case failed(String)
    }

    private var busy: Bool { if case .creating = phase { true } else { false } }
    private var trimmed: String { name.trimmingCharacters(in: .whitespaces) }

    var body: some View {
        MobileSheet("New Volume", subtitle: "Creates a persistent directory under Graviton's volumes folder.",
                    actionTitle: "Create", actionEnabled: !busy && !trimmed.isEmpty && store.canManage, action: create) {
            GFormRow("Name", hint: "Letters, numbers, hyphens, underscores, or periods.") {
                MobileField(placeholder: "pgdata", text: $name)
                    .focused($focused)
                    .onSubmit(create)
            }
            switch phase {
            case .idle: EmptyView()
            case .creating: GStatusLine(.working, "Creating…")
            case let .failed(msg): GStatusLine(.failure, msg)
            }
        }
        .disabled(busy)
        .interactiveDismissDisabled(busy)
        .onAppear { focused = true }
    }

    private func create() {
        guard !busy, !trimmed.isEmpty else { return }
        phase = .creating
        Task {
            do {
                _ = try await store.createVolume(name: trimmed)
                dismiss()
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}

private struct MobileCreateNetworkSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var internalOnly = false
    @State private var phase: Phase = .idle
    @FocusState private var focused: Bool

    enum Phase {
        case idle, creating
        case failed(String)
    }

    private var busy: Bool { if case .creating = phase { true } else { false } }
    private var trimmed: String { name.trimmingCharacters(in: .whitespaces) }

    var body: some View {
        MobileSheet("New Network", subtitle: "Creates an L2-isolated bridge network with scoped container DNS.",
                    actionTitle: "Create", actionEnabled: !busy && !trimmed.isEmpty && store.canManage, action: create) {
            GFormRow("Name", hint: "Letters, numbers, hyphens, underscores, or periods.") {
                MobileField(placeholder: "app-net", text: $name)
                    .focused($focused)
                    .onSubmit(create)
            }
            GFormRow("Isolation", hint: "Internal networks block outbound NAT to the internet.") {
                Toggle("Internal only", isOn: $internalOnly)
                    .toggleStyle(.gSwitch)
                    .disabled(busy)
            }
            switch phase {
            case .idle: EmptyView()
            case .creating: GStatusLine(.working, "Creating…")
            case let .failed(msg): GStatusLine(.failure, msg)
            }
        }
        .disabled(busy)
        .interactiveDismissDisabled(busy)
        .onAppear { focused = true }
    }

    private func create() {
        guard !busy, !trimmed.isEmpty else { return }
        phase = .creating
        Task {
            do {
                _ = try await store.createNetwork(name: trimmed, internalOnly: internalOnly)
                dismiss()
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}
