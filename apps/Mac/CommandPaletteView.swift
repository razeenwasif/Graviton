import GravitonKit
import SwiftUI

/// Stratus-style ⌘K Command Palette for Graviton: jump to containers, images, volumes,
/// networks, switch paired Macs, or trigger lifecycle and daemon actions in one keystroke.
struct CommandPaletteView: View {
    @Environment(EngineStore.self) private var store
    @Environment(SharedHosts.self) private var hosts
    @Environment(ShellSessions.self) private var shells
    @Environment(FairyController.self) private var fairy

    @Binding var isPresented: Bool
    @Binding var selection: Section
    var onOpenContainer: (String, DetailTab) -> Void
    var onRunContainer: (String) -> Void
    var onPullImage: () -> Void
    var onBuildImage: () -> Void
    var onCleanUp: () -> Void
    var onPairMac: () -> Void
    var onToggleSidebar: () -> Void
    var onToast: (String, Bool) -> Void

    @State private var query = ""
    @State private var selectedIndex = 0
    @FocusState private var searchFocused: Bool

    private struct Entry: Identifiable {
        let id: String
        let group: String
        let title: String
        let subtitle: String?
        let icon: String
        let tint: Color
        let badge: String?
        let shortcut: String?
        let perform: () -> Void
    }

    private var entries: [Entry] {
        var list: [Entry] = []

        // Sections
        for sec in Section.allCases {
            list.append(Entry(
                id: "sec-\(sec.rawValue)",
                group: "Navigate",
                title: "Go to \(sec.rawValue)",
                subtitle: sec.subtitle,
                icon: sec.symbol,
                tint: sec.accent,
                badge: nil,
                shortcut: sec.shortcutLabel,
                perform: { selection = sec }
            ))
        }

        // Actions
        if store.canManage {
            list.append(Entry(
                id: "act-run",
                group: "Actions",
                title: "Run Container…",
                subtitle: "Boot a new container VM from an image",
                icon: "play.circle",
                tint: G.indigo,
                badge: nil,
                shortcut: "⌘N",
                perform: { onRunContainer("") }
            ))
            list.append(Entry(
                id: "act-pull",
                group: "Actions",
                title: "Pull Image…",
                subtitle: "Download an OCI image from a registry",
                icon: "arrow.down.circle",
                tint: G.sky,
                badge: nil,
                shortcut: nil,
                perform: { onPullImage() }
            ))
            list.append(Entry(
                id: "act-cleanup",
                group: "Actions",
                title: "Clean Up Disk…",
                subtitle: "Prune build cache, snapshots, unused images or stopped containers",
                icon: "sparkles",
                tint: G.amber,
                badge: nil,
                shortcut: nil,
                perform: { onCleanUp() }
            ))
        }
        if store.canBuild {
            list.append(Entry(
                id: "act-build",
                group: "Actions",
                title: "Build Image…",
                subtitle: "Build from a local folder containing a Dockerfile",
                icon: "hammer",
                tint: G.sky,
                badge: nil,
                shortcut: nil,
                perform: { onBuildImage() }
            ))
        }
        list.append(Entry(
            id: "act-fairy",
            group: "Actions",
            title: fairy.isOpen ? "Hide \(fairy.fairyName)" : "Ask \(fairy.fairyName)",
            subtitle: "Toggle the autonomous container assistant drawer",
            icon: "sparkles",
            tint: G.violet,
            badge: nil,
            shortcut: "⌥⌘I",
            perform: {
                withAnimation(.snappy(duration: 0.22)) { fairy.toggle() }
            }
        ))
        list.append(Entry(
            id: "act-sidebar",
            group: "Actions",
            title: "Toggle Sidebar",
            subtitle: "Show or collapse the workspace navigation rail",
            icon: "sidebar.left",
            tint: G.textSecondary,
            badge: nil,
            shortcut: "⌘\\",
            perform: onToggleSidebar
        ))
        list.append(Entry(
            id: "act-refresh",
            group: "Actions",
            title: "Refresh Engine State",
            subtitle: "Poll containers, images, volumes, and networks now",
            icon: "arrow.clockwise",
            tint: G.mint,
            badge: nil,
            shortcut: "⌘R",
            perform: {
                Task {
                    await store.refresh()
                    onToast("Engine state refreshed", false)
                }
            }
        ))
        list.append(Entry(
            id: "act-pair",
            group: "Actions",
            title: "Pair a Mac…",
            subtitle: "Connect another Mac running gravitond --remote",
            icon: "desktopcomputer.and.arrow.down",
            tint: G.indigo,
            badge: nil,
            shortcut: nil,
            perform: onPairMac
        ))

        // Containers (jump + quick lifecycle)
        for c in store.containers {
            list.append(Entry(
                id: "cnt-open-\(c.id)",
                group: "Containers",
                title: c.name,
                subtitle: "\(c.image) · \(c.statusText())",
                icon: "shippingbox.fill",
                tint: c.isRunning ? G.mint : (c.isPaused ? G.amber : G.slate),
                badge: c.isRunning ? "Running" : (c.isPaused ? "Paused" : "Stopped"),
                shortcut: nil,
                perform: {
                    selection = .containers
                    onOpenContainer(c.id, .logs)
                }
            ))
            if c.isRunning {
                list.append(Entry(
                    id: "cnt-term-\(c.id)",
                    group: "Containers",
                    title: "Terminal: \(c.name)",
                    subtitle: "Open interactive shell inside \(c.name)",
                    icon: "terminal",
                    tint: G.sky,
                    badge: nil,
                    shortcut: nil,
                    perform: {
                        selection = .containers
                        onOpenContainer(c.id, .terminal)
                    }
                ))
                list.append(Entry(
                    id: "cnt-diff-\(c.id)",
                    group: "Containers",
                    title: "Diff: \(c.name)",
                    subtitle: "Inspect overlay filesystem changes inside \(c.name)",
                    icon: "doc.text.magnifyingglass",
                    tint: G.amber,
                    badge: nil,
                    shortcut: nil,
                    perform: {
                        selection = .containers
                        onOpenContainer(c.id, .diff)
                    }
                ))
                list.append(Entry(
                    id: "cnt-xray-\(c.id)",
                    group: "Containers",
                    title: "X-Ray: \(c.name)",
                    subtitle: "Live DNS, egress firewall & chaos controls for \(c.name)",
                    icon: "waveform.path.ecg",
                    tint: G.indigo,
                    badge: nil,
                    shortcut: nil,
                    perform: {
                        selection = .containers
                        onOpenContainer(c.id, .xray)
                    }
                ))
                list.append(Entry(
                    id: "cnt-stop-\(c.id)",
                    group: "Containers",
                    title: "Stop \(c.name)",
                    subtitle: c.image,
                    icon: "stop.circle",
                    tint: G.rose,
                    badge: nil,
                    shortcut: nil,
                    perform: {
                        Task {
                            await store.stop(c)
                            onToast("Stopped \(c.name)", false)
                        }
                    }
                ))
            } else if !c.isPaused {
                list.append(Entry(
                    id: "cnt-start-\(c.id)",
                    group: "Containers",
                    title: "Start \(c.name)",
                    subtitle: c.image,
                    icon: "play.circle",
                    tint: G.mint,
                    badge: nil,
                    shortcut: nil,
                    perform: {
                        Task {
                            await store.start(c)
                            onToast("Started \(c.name)", false)
                        }
                    }
                ))
            }
            if store.canManage {
                list.append(Entry(
                    id: "cnt-fork-\(c.id)",
                    group: "Containers",
                    title: "Fork \(c.name)",
                    subtitle: "APFS copy-on-write clone of \(c.name)",
                    icon: "arrow.triangle.branch",
                    tint: G.mint,
                    badge: nil,
                    shortcut: nil,
                    perform: {
                        Task {
                            do {
                                let created = try await store.fork(ForkSpec(source: c.id))
                                onToast("Forked \(created.map(\.name).joined(separator: ", "))", false)
                            } catch {
                                onToast(error.localizedDescription, true)
                            }
                        }
                    }
                ))
            }
        }

        // Images
        for img in store.images {
            let ref = "\(img.repository):\(img.tag)"
            list.append(Entry(
                id: "img-\(img.id)",
                group: "Images",
                title: ref,
                subtitle: "\(Format.bytes(img.sizeBytes)) · \(img.architecture)",
                icon: "square.stack.3d.up",
                tint: G.sky,
                badge: "Run",
                shortcut: nil,
                perform: {
                    if store.canManage {
                        onRunContainer(ref)
                    } else {
                        selection = .images
                    }
                }
            ))
        }

        // Volumes
        for vol in store.volumes {
            list.append(Entry(
                id: "vol-\(vol.name)",
                group: "Volumes",
                title: vol.name,
                subtitle: Format.bytes(vol.sizeBytes),
                icon: "internaldrive",
                tint: G.amber,
                badge: nil,
                shortcut: nil,
                perform: { selection = .volumes }
            ))
        }

        // Networks
        for net in store.networks {
            list.append(Entry(
                id: "net-\(net.name)",
                group: "Networks",
                title: net.name,
                subtitle: net.driver,
                icon: "network",
                tint: G.mint,
                badge: "\(net.containers.count) attached",
                shortcut: nil,
                perform: { selection = .networks }
            ))
        }

        // Hosts
        list.append(Entry(
            id: "host-local",
            group: "Hosts",
            title: "Switch Host: This Mac",
            subtitle: "Local gravitond socket",
            icon: "laptopcomputer",
            tint: G.indigo,
            badge: hosts.selected == nil ? "Active" : nil,
            shortcut: "⌥⌘0",
            perform: { switchHost(to: nil, store: store, hosts: hosts, shells: shells) }
        ))
        for (i, mac) in hosts.hosts.enumerated() {
            list.append(Entry(
                id: "host-\(mac.name)",
                group: "Hosts",
                title: "Switch Host: \(mac.name)",
                subtitle: mac.address,
                icon: "desktopcomputer",
                tint: G.indigo,
                badge: hosts.selected == mac.name ? "Active" : nil,
                shortcut: i < 9 ? "⌥⌘\(i + 1)" : nil,
                perform: { switchHost(to: mac, store: store, hosts: hosts, shells: shells) }
            ))
        }

        let needle = query.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        guard !needle.isEmpty else {
            return Array(list.prefix(24))
        }
        return list.filter { entry in
            entry.title.lowercased().contains(needle)
                || entry.group.lowercased().contains(needle)
                || (entry.subtitle?.lowercased().contains(needle) ?? false)
        }
    }

    var body: some View {
        ZStack {
            Color.black.opacity(0.60)
                .ignoresSafeArea()
                .onTapGesture { dismiss() }

            VStack(spacing: 0) {
                HStack(spacing: 10) {
                    Image(systemName: "magnifyingglass")
                        .font(.system(size: 14, weight: .medium))
                        .foregroundStyle(G.indigo)
                    TextField("", text: $query, prompt: Text("Search containers, images, hosts, or actions…").foregroundStyle(G.textTertiary))
                        .textFieldStyle(.plain)
                        .font(.system(size: 14.5))
                        .foregroundStyle(G.textPrimary)
                        .focused($searchFocused)
                        .onSubmit { executeSelected() }
                    if !query.isEmpty {
                        Button {
                            query = ""
                        } label: {
                            Image(systemName: "xmark.circle.fill")
                                .font(.system(size: 13))
                                .foregroundStyle(G.textMuted)
                        }
                        .buttonStyle(.plain)
                    }
                    Text("ESC")
                        .font(G.mono(10, weight: .semibold))
                        .foregroundStyle(G.textMuted)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 3)
                        .background(G.surfacePill, in: RoundedRectangle(cornerRadius: 5, style: .continuous))
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 14)
                .background(Color(hex: 0x101016))

                let visible = entries
                if visible.isEmpty {
                    VStack(spacing: 8) {
                        Image(systemName: "magnifyingglass")
                            .font(.system(size: 20))
                            .foregroundStyle(G.textTertiary)
                        Text("No matching commands or resources")
                            .font(.system(size: 13, weight: .medium))
                            .foregroundStyle(G.textSecondary)
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 42)
                } else {
                    ScrollViewReader { proxy in
                        ScrollView {
                            LazyVStack(alignment: .leading, spacing: 2) {
                                ForEach(Array(visible.enumerated()), id: \.element.id) { idx, entry in
                                    paletteRow(entry, selected: idx == selectedIndex) {
                                        selectedIndex = idx
                                        execute(entry)
                                    }
                                    .id(entry.id)
                                }
                            }
                            .padding(8)
                        }
                        .frame(maxHeight: 380)
                        .onChange(of: selectedIndex) { _, next in
                            guard visible.indices.contains(next) else { return }
                            proxy.scrollTo(visible[next].id, anchor: .center)
                        }
                    }
                }

                HStack(spacing: 14) {
                    hintKey("↑↓", "Navigate")
                    hintKey("↩", "Run")
                    hintKey("Esc", "Close")
                    Spacer()
                    Text("\(entries.count) results")
                        .font(G.mono(10.5))
                        .foregroundStyle(G.textTertiary)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
                .background(Color(hex: 0x08080C))
            }
            .frame(width: 580)
            .background(G.palette, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
            .shadow(color: .black.opacity(0.75), radius: 36, y: 16)
            .padding(.top, 76)
            .frame(maxHeight: .infinity, alignment: .top)
        }
        .onAppear {
            selectedIndex = 0
            searchFocused = true
        }
        .onChange(of: query) { _, _ in
            selectedIndex = 0
        }
        .onKeyPress(.downArrow) {
            let count = entries.count
            guard count > 0 else { return .ignored }
            selectedIndex = min(selectedIndex + 1, count - 1)
            return .handled
        }
        .onKeyPress(.upArrow) {
            guard !entries.isEmpty else { return .ignored }
            selectedIndex = max(selectedIndex - 1, 0)
            return .handled
        }
        .onKeyPress(.escape) {
            dismiss()
            return .handled
        }
    }

    private func paletteRow(_ entry: Entry, selected: Bool, tap: @escaping () -> Void) -> some View {
        Button(action: tap) {
            HStack(spacing: 11) {
                Image(systemName: entry.icon)
                    .font(.system(size: 12.5, weight: .medium))
                    .foregroundStyle(entry.tint)
                    .frame(width: 26, height: 26)
                    .background(entry.tint.opacity(0.12), in: RoundedRectangle(cornerRadius: 7, style: .continuous))
                VStack(alignment: .leading, spacing: 1.5) {
                    Text(entry.title)
                        .font(.system(size: 13, weight: .medium))
                        .foregroundStyle(G.textPrimary)
                        .lineLimit(1)
                    if let sub = entry.subtitle, !sub.isEmpty {
                        Text(sub)
                            .font(G.mono(11))
                            .foregroundStyle(G.textMuted)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                Spacer(minLength: 8)
                if let badge = entry.badge {
                    GTagPill(badge, color: entry.tint)
                }
                Text(entry.group)
                    .font(.system(size: 10.5, weight: .medium))
                    .foregroundStyle(G.textTertiary)
                if let shortcut = entry.shortcut {
                    Text(shortcut)
                        .font(G.mono(10.5))
                        .foregroundStyle(G.textMuted)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 5, style: .continuous))
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(selected ? G.surfaceActive : .clear, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func hintKey(_ key: String, _ label: String) -> some View {
        HStack(spacing: 5) {
            Text(key)
                .font(G.mono(10, weight: .semibold))
                .foregroundStyle(G.textSecondary)
                .padding(.horizontal, 5)
                .padding(.vertical, 2)
                .background(G.surfacePill, in: RoundedRectangle(cornerRadius: 4, style: .continuous))
            Text(label)
                .font(.system(size: 11))
                .foregroundStyle(G.textTertiary)
        }
    }

    private func executeSelected() {
        let visible = entries
        guard visible.indices.contains(selectedIndex) else { return }
        execute(visible[selectedIndex])
    }

    private func execute(_ entry: Entry) {
        dismiss()
        entry.perform()
    }

    private func dismiss() {
        withAnimation(.easeOut(duration: 0.14)) {
            isPresented = false
        }
    }
}
