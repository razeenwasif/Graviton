import AppKit
import GravitonKit
import SwiftUI

/// What the menu bar panel asks the main window to show.
@MainActor
@Observable
final class AppRouter {
    enum Request: Equatable {
        case runContainer
        case container(String, DetailTab)
        case images
        /// Open the Pair a Mac sheet.
        case pairMac
    }

    /// Set by the menu bar panel; the screen that handles it sets it back to `nil`.
    var pending: Request?
}

/// The menu bar item: the Graviton glyph, plus the running count if that's turned on.
struct MenuBarLabel: View {
    @Environment(EngineStore.self) private var store
    @AppStorage("menuBarShowsCount") private var showsCount = true

    var body: some View {
        let online = store.connection == .live
        HStack(spacing: 3) {
            Image(nsImage: online ? MenuBarGlyph.live : MenuBarGlyph.offline)
            if showsCount, online, store.runningCount > 0 {
                Text("\(store.runningCount)").monospacedDigit()
            }
        }
        .accessibilityLabel(online ? "Graviton, \(store.runningCount) running" : "Graviton, engine not running")
    }
}

/// The window that drops down from the menu bar: engine status, live totals,
/// every container with quick actions, and shortcuts into the main window.
struct MenuBarPanel: View {
    @Environment(EngineStore.self) private var store
    @Environment(AppRouter.self) private var router
    @Environment(\.openWindow) private var openWindow

    private var sorted: [Container] {
        store.containers.sorted { a, b in
            if a.isRunning != b.isRunning { return a.isRunning }
            return a.name.localizedStandardCompare(b.name) == .orderedAscending
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            PanelHeader(refresh: { Task { await store.refresh() } }, openApp: { show(nil) })
                .padding(.horizontal, 14)
                .padding(.top, 14)
                .padding(.bottom, 12)

            Totals()
                .padding(.horizontal, 14)
                .padding(.bottom, 12)

            Divider().overlay(G.border)

            containerList

            Divider().overlay(G.border)

            PanelFooter(run: { show(.runContainer) }, images: { show(.images) })
                .padding(.horizontal, 10)
                .padding(.vertical, 8)
        }
        .frame(width: 340)
        .background(G.surface)
        .preferredColorScheme(.dark)
        .tint(G.accent)
    }

    @ViewBuilder private var containerList: some View {
        if store.containers.isEmpty {
            VStack(spacing: 8) {
                Image(systemName: "shippingbox").font(.system(size: 22)).foregroundStyle(G.faint)
                Text(store.connection == .connecting ? "Connecting…" : "No containers yet")
                    .font(.system(size: 12.5, weight: .medium)).foregroundStyle(G.textSecondary)
                if store.canManage {
                    Button("Run a container…") { show(.runContainer) }
                        .buttonStyle(.plain)
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(G.accent)
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 26)
        } else {
            VStack(alignment: .leading, spacing: 2) {
                Text("Containers")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(G.faint)
                    .padding(.horizontal, 8)
                    .padding(.top, 8)
                    .padding(.bottom, 2)
                ScrollView {
                    VStack(spacing: 1) {
                        ForEach(sorted) { c in
                            ContainerQuickRow(container: c) { tab in show(.container(c.id, tab)) }
                        }
                    }
                }
                .scrollIndicators(.automatic)
                .frame(maxHeight: min(CGFloat(sorted.count) * 46, 46 * 6.5))
            }
            .padding(.horizontal, 6)
            .padding(.bottom, 6)
        }
    }

    /// Bring the main window forward (opening one if it was closed), then hand it the request.
    private func show(_ request: AppRouter.Request?) {
        NSApp.activate()
        if let window = NSApp.windows.first(where: { $0.identifier?.rawValue.hasPrefix("main") == true }) {
            if window.isMiniaturized { window.deminiaturize(nil) }
            window.makeKeyAndOrderFront(nil)
        } else {
            openWindow(id: "main")
        }
        router.pending = request
    }
}

private struct PanelHeader: View {
    @Environment(EngineStore.self) private var store
    var refresh: () -> Void
    var openApp: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            GravitonMark(size: 30)
            VStack(alignment: .leading, spacing: 2) {
                Text("Graviton").font(.system(size: 13.5, weight: .semibold)).foregroundStyle(G.text)
                HStack(spacing: 6) {
                    statusDot
                    Text(statusText).font(.system(size: 11.5)).foregroundStyle(G.muted).lineLimit(1)
                }
            }
            Spacer(minLength: 8)
            IconButton("arrow.clockwise", help: "Refresh", action: refresh)
            IconButton("macwindow", help: "Open Graviton", action: openApp)
        }
    }

    @ViewBuilder private var statusDot: some View {
        switch store.connection {
        case .live: StatusDot(running: true, size: 6)
        case .demo: Circle().fill(G.warning).frame(width: 6, height: 6)
        case .unreachable: Circle().fill(G.danger).frame(width: 6, height: 6)
        case .connecting: GSpinner(size: 8, color: G.warning)
        }
    }

    private var statusText: String {
        switch store.connection {
        case .live:
            if let e = store.engine { return "Engine running · \(e.version)" }
            return "Engine running"
        case .demo: return "Demo data · gravitond isn't running"
        case .unreachable: return "Can't reach gravitond"
        case .connecting: return "Connecting…"
        }
    }
}

/// Three small tiles: how many are running, total CPU (with a sparkline), engine memory.
private struct Totals: View {
    @Environment(EngineStore.self) private var store

    /// The containers' CPU histories added up, newest samples aligned.
    private var cpuSeries: [Double] {
        let series = store.containers.compactMap { store.cpuHistory[$0.id] }
        let n = series.map(\.count).max() ?? 0
        return (0..<n).map { i in
            series.reduce(0) { sum, s in
                let j = s.count - n + i
                return sum + (j >= 0 ? s[j] : 0)
            }
        }
    }

    var body: some View {
        HStack(spacing: 8) {
            tile("Running") {
                HStack(alignment: .firstTextBaseline, spacing: 3) {
                    Text("\(store.runningCount)").font(.system(size: 17, weight: .semibold)).monospacedDigit()
                    Text("/ \(store.containers.count)").font(.system(size: 11.5)).foregroundStyle(G.faint)
                }
            } footer: {
                MeterBar(store.containers.isEmpty ? 0 : Double(store.runningCount) / Double(store.containers.count),
                         color: G.running, height: 3)
            }

            tile("CPU") {
                Text(Format.percent(store.totalCPU)).font(.system(size: 17, weight: .semibold)).monospacedDigit()
            } footer: {
                Sparkline(cpuSeries, color: G.accent, filled: true, lineWidth: 1.2).frame(height: 12)
            }

            tile("Memory") {
                Text(Format.bytes(store.engine?.memoryUsedBytes ?? store.totalMemory))
                    .font(.system(size: 17, weight: .semibold)).monospacedDigit()
            } footer: {
                if let e = store.engine, e.memoryBytes > 0 {
                    MeterBar(Double(e.memoryUsedBytes) / Double(e.memoryBytes), height: 3)
                } else {
                    MeterBar(0, height: 3)
                }
            }
        }
    }

    private func tile(_ title: String, @ViewBuilder value: () -> some View, @ViewBuilder footer: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(title).font(.system(size: 10.5, weight: .medium)).foregroundStyle(G.faint)
            value().foregroundStyle(G.text).lineLimit(1).minimumScaleFactor(0.7)
            footer().frame(height: 12, alignment: .bottom)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(G.card, radius: 9)
    }
}

/// One container: status, name, image and usage; quick actions appear on hover.
private struct ContainerQuickRow: View {
    @Environment(EngineStore.self) private var store
    var container: Container
    var open: (DetailTab) -> Void
    @State private var hovering = false
    @State private var busy = false

    private var webPorts: [PortMapping] { container.ports.filter { $0.protocol == .tcp } }

    var body: some View {
        HStack(spacing: 10) {
            StatusDot(running: container.isRunning, size: 7)
            VStack(alignment: .leading, spacing: 1) {
                Text(container.name).font(.system(size: 12.5, weight: .medium)).foregroundStyle(G.text).lineLimit(1)
                Text(container.image).font(G.mono(10.5)).foregroundStyle(G.muted).lineLimit(1).truncationMode(.middle)
            }
            Spacer(minLength: 6)
            if hovering || busy {
                actions
            } else {
                usage
            }
        }
        .padding(.horizontal, 8)
        .frame(height: 45)
        .background(hovering ? Color.white.opacity(0.05) : .clear, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .onTapGesture { open(.logs) }
        .contextMenu { menu }
        .help("\(container.name) — click to show its logs")
    }

    private var usage: some View {
        VStack(alignment: .trailing, spacing: 1) {
            if container.isRunning {
                Text(Format.percent(container.cpuPercent)).font(G.mono(11)).foregroundStyle(G.textSecondary)
                Text(container.memoryBytes.map(Format.bytes) ?? container.statusText())
                    .font(G.mono(10.5)).foregroundStyle(G.faint)
            } else {
                Text(container.statusText()).font(.system(size: 11)).foregroundStyle(G.faint)
            }
        }
        .monospacedDigit()
    }

    private var actions: some View {
        HStack(spacing: 2) {
            if busy {
                GSpinner(size: 14).frame(width: 24)
            } else if container.isRunning {
                if let port = webPorts.first {
                    IconButton("safari", help: "Open localhost:\(port.hostPort)") { openInBrowser(port) }
                }
                IconButton("terminal", help: "Open a shell") { open(.terminal) }
                IconButton("arrow.counterclockwise", help: "Restart") { lifecycle { await store.restart(container) } }
                IconButton("stop.fill", help: "Stop", tint: G.danger) { lifecycle { await store.stop(container) } }
            } else {
                IconButton("play.fill", help: "Start", tint: G.running) { lifecycle { await store.start(container) } }
            }
        }
    }

    @ViewBuilder private var menu: some View {
        if container.isRunning {
            Button("Stop") { lifecycle { await store.stop(container) } }
            Button("Restart") { lifecycle { await store.restart(container) } }
        } else {
            Button("Start") { lifecycle { await store.start(container) } }
        }
        Divider()
        Button("Show Logs") { open(.logs) }
        Button("Open Shell") { open(.terminal) }.disabled(!container.isRunning)
        Button("Show Stats") { open(.stats) }
        if container.isRunning, !webPorts.isEmpty {
            Divider()
            ForEach(webPorts, id: \.self) { port in
                Button("Open localhost:\(port.hostPort)") { openInBrowser(port) }
            }
        }
        Divider()
        Button("Copy Name") { copy(container.name) }
        Button("Copy ID") { copy(container.id) }
        if let ip = container.ipAddress { Button("Copy IP Address (\(ip))") { copy(ip) } }
    }

    private func lifecycle(_ action: @escaping () async -> Void) {
        busy = true
        Task {
            await action()
            busy = false
        }
    }

    private func openInBrowser(_ port: PortMapping) {
        let host = switch port.hostIp {
        case nil, "127.0.0.1", "0.0.0.0", "::", "::1": "localhost"
        case let ip?: ip.contains(":") ? "[\(ip)]" : ip
        }
        if let url = URL(string: "http://\(host):\(port.hostPort)") { NSWorkspace.shared.open(url) }
    }

    private func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}

private struct PanelFooter: View {
    @Environment(EngineStore.self) private var store
    @AppStorage("menuBarShowsCount") private var showsCount = true
    var run: () -> Void
    var images: () -> Void

    var body: some View {
        HStack(spacing: 4) {
            FooterButton("Run Container…", symbol: "plus", action: run)
                .disabled(!store.canManage)
                .help(store.canManage ? "Run a new container" : "Start gravitond to run containers")
            FooterButton("Images", symbol: "square.stack.3d.up", action: images)
            Spacer()
            Menu {
                Toggle("Show Running Count in Menu Bar", isOn: $showsCount)
                Divider()
                Button("Quit Graviton") { NSApp.terminate(nil) }
                    .keyboardShortcut("q")
            } label: {
                Image(systemName: "ellipsis")
                    .font(.system(size: 12, weight: .bold))
                    .foregroundStyle(G.muted)
                    .frame(width: 26, height: 24)
                    .contentShape(Rectangle())
            }
            .gravitonMenu()
            .padding(.trailing, 4)
        }
    }
}

private struct FooterButton: View {
    var title: String
    var symbol: String
    var action: () -> Void
    @State private var hovering = false
    @Environment(\.isEnabled) private var enabled

    init(_ title: String, symbol: String, action: @escaping () -> Void) {
        self.title = title
        self.symbol = symbol
        self.action = action
    }

    var body: some View {
        Button(action: action) {
            Label(title, systemImage: symbol)
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(enabled ? G.textSecondary : G.faint)
                .padding(.horizontal, 8)
                .padding(.vertical, 5)
                .background(hovering && enabled ? Color.white.opacity(0.07) : .clear, in: RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
    }
}

/// A small borderless icon button with a hover highlight.
private struct IconButton: View {
    var symbol: String
    var help: String
    var tint: Color
    var action: () -> Void
    @State private var hovering = false

    init(_ symbol: String, help: String, tint: Color = G.textSecondary, action: @escaping () -> Void) {
        self.symbol = symbol
        self.help = help
        self.tint = tint
        self.action = action
    }

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(hovering ? tint : G.muted)
                .frame(width: 24, height: 24)
                .background(hovering ? Color.white.opacity(0.08) : .clear, in: RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help(help)
        .accessibilityLabel(help)
    }
}
