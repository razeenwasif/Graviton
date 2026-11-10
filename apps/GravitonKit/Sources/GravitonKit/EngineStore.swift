import Foundation
import Observation

/// The app's single source of truth. Polls the engine, keeps short metric
/// histories for sparklines, and falls back to `DemoEngine` when gravitond
/// isn't reachable.
@MainActor
@Observable
public final class EngineStore {
    public enum Connection: Equatable, Sendable {
        case connecting
        /// Connected to gravitond.
        case live
        /// Using sample data (no daemon, or no Mac chosen on iOS).
        case demo
        /// A chosen Mac isn't answering; the last data it sent stays on screen.
        case unreachable
    }

    public private(set) var connection: Connection = .connecting
    public private(set) var containers: [Container] = []
    public private(set) var images: [ImageInfo] = []
    public private(set) var engine: EngineInfo?
    public private(set) var logs: [String: [LogLine]] = [:]
    public private(set) var cpuHistory: [String: [Double]] = [:]
    public private(set) var memoryHistory: [String: [Double]] = [:]
    public var lastError: String?
    /// Why the live engine isn't answering (shown on iOS's host chip).
    public private(set) var liveError: String?

    public static let historyLength = 40

    /// The engine tried first: gravitond's socket on the Mac, a paired Mac on iOS.
    private var live: (any EngineService)?
    private let demo: DemoEngine
    /// What `live` is called in the UI ("This Mac", or the paired Mac's name).
    public private(set) var liveName: String?
    /// Show sample data when `live` doesn't answer (the Mac app without gravitond).
    /// Off for a chosen Mac on iOS, where sample data would be mistaken for it.
    private var fallBackToDemo = true

    /// - Parameter useDaemon: try gravitond's local socket first (macOS). Pass `false` for demo data.
    public init(useDaemon: Bool = true) {
        live = useDaemon ? SocketEngine() : nil
        liveName = useDaemon ? "This Mac" : nil
        demo = DemoEngine()
    }

    /// Switch to another engine (a paired Mac), or `nil` for demo data. Clears what was shown.
    public func use(_ engine: (any EngineService)?, named name: String?) {
        live = engine
        liveName = name
        fallBackToDemo = false
        connection = .connecting
        containers = []
        images = []
        self.engine = nil
        logs = [:]
        cpuHistory = [:]
        memoryHistory = [:]
        Task { await refresh() }
    }

    private var service: any EngineService {
        if connection == .live, let live { return live }
        return demo
    }

    public var runningCount: Int { containers.filter(\.isRunning).count }
    public var totalCPU: Double { containers.compactMap(\.cpuPercent).reduce(0, +) }
    public var totalMemory: UInt64 { containers.compactMap(\.memoryBytes).reduce(0, +) }

    public func container(_ id: String?) -> Container? {
        guard let id else { return nil }
        return containers.first { $0.id == id }
    }

    /// Poll until the surrounding task is cancelled (use from `.task {}`).
    public func run(interval: Duration = .seconds(2)) async {
        while !Task.isCancelled {
            await refresh()
            try? await Task.sleep(for: interval)
        }
    }

    public func refresh() async {
        if let live {
            do {
                _ = try await live.ping()
                connection = .live
                liveError = nil
            } catch {
                liveError = error.localizedDescription
                if !fallBackToDemo {
                    connection = .unreachable
                    return
                }
                connection = .demo
            }
        } else {
            connection = .demo
            liveError = nil
        }
        do {
            let service = self.service
            async let list = service.containers()
            async let imgs = service.images()
            async let info = service.engineInfo()
            containers = try await list
            images = try await imgs
            engine = try await info
            recordHistory()
        } catch {
            lastError = error.localizedDescription
        }
    }

    public func loadLogs(for id: String, tail: Int? = 500) async {
        do {
            logs[id] = try await service.logs(id: id, tail: tail)
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// Pulling and running need gravitond; the demo engine can't.
    public var canManage: Bool { connection == .live }

    /// Pull an image. Throws so a sheet can show the error in place.
    @discardableResult
    public func pull(_ reference: String) async throws -> ImageInfo {
        let image = try await service.pull(reference: reference)
        await refresh()
        return image
    }

    /// Run a container (pulling its image first if needed). Throws so a sheet can show the error.
    @discardableResult
    public func run(_ spec: RunSpec) async throws -> Container {
        let container = try await service.run(spec)
        await refresh()
        return container
    }

    public func remove(_ c: Container) async {
        do {
            _ = try await service.remove(id: c.id)
            logs[c.id] = nil
            await refresh()
        } catch {
            lastError = error.localizedDescription
        }
    }

    // Panels call these directly and show errors in place.
    public func stats(for id: String) async throws -> ContainerStats { try await service.stats(id: id) }
    public func listFiles(in id: String, path: String) async throws -> DirectoryListing { try await service.listFiles(id: id, path: path) }
    public func readFile(in id: String, path: String) async throws -> FileContents { try await service.readFile(id: id, path: path) }
    public func inspect(_ id: String) async throws -> ContainerDetails { try await service.inspect(id: id) }
    public func openTerminal(_ spec: ExecSpec) async throws -> any TerminalSession { try await service.openTerminal(spec) }

    public func start(_ c: Container) async { await lifecycle { try await $0.start(id: c.id) } }
    public func stop(_ c: Container) async { await lifecycle { try await $0.stop(id: c.id) } }
    public func restart(_ c: Container) async { await lifecycle { try await $0.restart(id: c.id) } }

    private func lifecycle(_ action: (any EngineService) async throws -> Container) async {
        do {
            let updated = try await action(service)
            if let i = containers.firstIndex(where: { $0.id == updated.id }) { containers[i] = updated }
            await refresh()
        } catch {
            lastError = error.localizedDescription
        }
    }

    private func recordHistory() {
        for c in containers {
            append(&cpuHistory, c.id, c.cpuPercent ?? 0)
            append(&memoryHistory, c.id, Double(c.memoryBytes ?? 0))
        }
    }

    private func append(_ history: inout [String: [Double]], _ id: String, _ value: Double) {
        var series = history[id] ?? []
        series.append(value)
        if series.count > Self.historyLength { series.removeFirst(series.count - Self.historyLength) }
        history[id] = series
    }
}

public enum ContainerFilter: String, CaseIterable, Identifiable, Sendable {
    case all = "All", running = "Running", stopped = "Stopped"
    public var id: String { rawValue }

    public func apply(_ list: [Container], search: String = "") -> [Container] {
        list.filter { c in
            let statusOK = switch self {
            case .all: true
            case .running: c.isRunning
            case .stopped: !c.isRunning
            }
            let q = search.trimmingCharacters(in: .whitespaces).lowercased()
            return statusOK && (q.isEmpty || c.name.lowercased().contains(q) || c.image.lowercased().contains(q))
        }
    }
}
