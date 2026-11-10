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
        /// Using sample data (no daemon, or on iOS before remote hosts exist).
        case demo
    }

    public private(set) var connection: Connection = .connecting
    public private(set) var containers: [Container] = []
    public private(set) var images: [ImageInfo] = []
    public private(set) var engine: EngineInfo?
    public private(set) var logs: [String: [LogLine]] = [:]
    public private(set) var cpuHistory: [String: [Double]] = [:]
    public private(set) var memoryHistory: [String: [Double]] = [:]
    public var lastError: String?

    public static let historyLength = 40

    private let live: SocketEngine?
    private let demo: DemoEngine

    /// - Parameter useDaemon: try gravitond first (macOS). Pass `false` for demo-only.
    public init(useDaemon: Bool = true) {
        live = useDaemon ? SocketEngine() : nil
        demo = DemoEngine()
    }

    private var service: any EngineService {
        connection == .live ? live! : demo
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
        if let live, (try? await live.ping()) != nil {
            connection = .live
        } else {
            connection = .demo
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
