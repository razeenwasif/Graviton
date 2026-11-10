import Foundation

// Swift mirrors of the types in `crates/graviton-proto`. JSON keys are
// snake_case on the wire; `Wire.decoder` converts them.

public enum ContainerStatus: String, Codable, Sendable, Hashable {
    case created, running, paused, exited
}

public enum PortProtocol: String, Codable, Sendable, Hashable {
    case tcp, udp
}

public struct PortMapping: Codable, Hashable, Sendable {
    public var hostPort: UInt16
    public var containerPort: UInt16
    public var `protocol`: PortProtocol

    public var label: String { "\(hostPort) → \(containerPort)/\(`protocol`.rawValue)" }
}

public struct Mount: Codable, Hashable, Sendable {
    public var source: String
    public var target: String
    public var readOnly: Bool
}

public struct Container: Codable, Identifiable, Hashable, Sendable {
    public var id: String
    public var name: String
    public var image: String
    public var status: ContainerStatus
    public var exitCode: Int32?
    public var ports: [PortMapping]
    public var mounts: [Mount]
    public var ipAddress: String?
    public var cpuPercent: Double?
    public var memoryBytes: UInt64?
    public var memoryLimitBytes: UInt64?
    /// Unix seconds.
    public var createdAt: UInt64
    /// Unix seconds; set while running.
    public var startedAt: UInt64?

    public var isRunning: Bool { status == .running }

    /// "Up 3h 12m", "Exited (137)".
    public func statusText(now: Date = .now) -> String {
        switch status {
        case .running:
            guard let startedAt else { return "Running" }
            return Format.uptime(now.timeIntervalSince1970 - Double(startedAt))
        case .exited: return exitCode.map { "Exited (\($0))" } ?? "Exited"
        case .paused: return "Paused"
        case .created: return "Created"
        }
    }
}

public struct ImageInfo: Codable, Identifiable, Hashable, Sendable {
    public var reference: String
    public var repository: String
    public var tag: String
    public var digest: String
    public var sizeBytes: UInt64
    public var architecture: String
    public var usedBy: UInt32

    public var id: String { reference }

    /// Two-letter monogram for the image card ("postgres" → "po", "storefront/api" → "sa").
    public var monogram: String {
        let parts = repository.split(separator: "/").suffix(2)
        if parts.count == 2, let a = parts.first?.first, let b = parts.last?.first { return "\(a)\(b)" }
        return String(repository.split(separator: "/").last?.prefix(2) ?? "")
    }
}

public enum LogLevel: String, Codable, Sendable, CaseIterable, Comparable, Hashable {
    case debug, info, warn, error

    private var rank: Int { Self.allCases.firstIndex(of: self)! }
    public static func < (a: Self, b: Self) -> Bool { a.rank < b.rank }
    public var label: String { rawValue.uppercased() }
}

public struct LogLine: Codable, Identifiable, Hashable, Sendable {
    public var seq: UInt64
    /// Unix milliseconds.
    public var timestampMs: UInt64
    public var level: LogLevel
    public var message: String

    public var id: UInt64 { seq }
    public var date: Date { Date(timeIntervalSince1970: Double(timestampMs) / 1000) }
}

public struct EngineInfo: Codable, Hashable, Sendable {
    public var version: String
    public var `protocol`: UInt32
    public var kernel: String
    public var arch: String
    public var cpus: UInt32
    public var memoryBytes: UInt64
    public var memoryUsedBytes: UInt64
    public var diskBytes: UInt64
    public var diskUsedBytes: UInt64
}
