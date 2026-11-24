import Foundation

// Swift mirrors of the types in `crates/graviton-proto`. JSON keys are
// snake_case on the wire; `Wire.decoder` converts them.

public enum ContainerStatus: String, Codable, Sendable, Hashable {
    case created, running, paused, exited
}

public enum PortProtocol: String, Codable, Sendable, Hashable {
    case tcp, udp
}

/// When gravitond should restart a container automatically (`RestartPolicy` in `graviton-proto`).
public enum RestartPolicy: String, Codable, CaseIterable, Hashable, Sendable {
    case no
    case onFailure = "on-failure"
    case always

    public var label: String {
        switch self {
        case .no: "Never"
        case .onFailure: "On failure"
        case .always: "Always"
        }
    }
}

public struct PortMapping: Codable, Hashable, Sendable {
    /// Interface address on the Mac to bind (`nil` means `127.0.0.1`).
    public var hostIp: String?
    public var hostPort: UInt16
    public var containerPort: UInt16
    public var `protocol`: PortProtocol

    public init(hostIp: String? = nil, hostPort: UInt16, containerPort: UInt16, protocol: PortProtocol) {
        self.hostIp = hostIp
        self.hostPort = hostPort
        self.containerPort = containerPort
        self.protocol = `protocol`
    }

    public var bindIp: String { hostIp ?? "127.0.0.1" }
    public var hostAddress: String { bindIp.contains(":") ? "[\(bindIp)]:\(hostPort)" : "\(bindIp):\(hostPort)" }
    public var label: String { "\(hostPort) → \(containerPort)/\(`protocol`.rawValue)" }
    public var fullLabel: String { "\(hostAddress) → \(containerPort)/\(`protocol`.rawValue)" }
    public var compactLabel: String {
        let prefix = hostIp.map { $0.contains(":") ? "[\($0)]:" : "\($0):" } ?? ""
        let proto = `protocol` == .udp ? "/udp" : ""
        return "\(prefix)\(hostPort)→\(containerPort)\(proto)"
    }
}

public struct Mount: Codable, Hashable, Sendable {
    public var source: String
    public var target: String
    public var readOnly: Bool
}

public enum HealthStatus: String, Codable, Hashable, Sendable {
    case starting
    case healthy
    case unhealthy

    public var label: String {
        switch self {
        case .starting: "Starting"
        case .healthy: "Healthy"
        case .unhealthy: "Unhealthy"
        }
    }
}

public struct Healthcheck: Codable, Hashable, Sendable {
    public var test: [String]
    public var intervalSecs: UInt64
    public var timeoutSecs: UInt64
    public var startPeriodSecs: UInt64
    public var retries: UInt32

    public init(test: [String], intervalSecs: UInt64 = 30, timeoutSecs: UInt64 = 30,
                startPeriodSecs: UInt64 = 0, retries: UInt32 = 3) {
        self.test = test
        self.intervalSecs = intervalSecs
        self.timeoutSecs = timeoutSecs
        self.startPeriodSecs = startPeriodSecs
        self.retries = retries
    }

    public var commandDisplay: String {
        if test.first == "CMD-SHELL", test.count >= 2 {
            return test.dropFirst().joined(separator: " ")
        }
        if test.first == "CMD" {
            return test.dropFirst().joined(separator: " ")
        }
        return test.joined(separator: " ")
    }
}

public struct HealthLog: Codable, Hashable, Sendable {
    public var start: UInt64
    public var end: UInt64
    public var exitCode: Int32
    public var output: String
}

public struct HealthState: Codable, Hashable, Sendable {
    public var status: HealthStatus
    public var failingStreak: UInt32
    public var log: [HealthLog] = []
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
    /// Docker Compose project (stack) name, if created via `grav compose`.
    public var project: String? = nil
    /// Service name inside `project`.
    public var service: String? = nil
    /// Container health status when running with an active `HEALTHCHECK`.
    public var health: HealthStatus? = nil

    public var isRunning: Bool { status == .running }
    public var isPaused: Bool { status == .paused }
    public var isActive: Bool { status == .running || status == .paused }

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

/// A Docker Compose project grouping one or more containers.
public struct ContainerStack: Identifiable, Hashable, Sendable {
    public var name: String
    public var containers: [Container]

    public init(name: String, containers: [Container]) {
        self.name = name
        self.containers = containers
    }

    public var id: String { name }
    public var runningCount: Int { containers.filter(\.isRunning).count }
    public var allRunning: Bool { !containers.isEmpty && runningCount == containers.count }
    public var anyRunning: Bool { runningCount > 0 }
    public var totalCPU: Double { containers.compactMap(\.cpuPercent).reduce(0, +) }
    public var totalMemory: UInt64 { containers.compactMap(\.memoryBytes).reduce(0, +) }

    /// Group containers into Compose stacks (sorted by project name) and standalone containers.
    public static func group(_ containers: [Container]) -> (stacks: [ContainerStack], standalone: [Container]) {
        var grouped: [String: [Container]] = [:]
        var standalone: [Container] = []
        for c in containers {
            if let project = c.project, !project.isEmpty {
                grouped[project, default: []].append(c)
            } else {
                standalone.append(c)
            }
        }
        let stacks = grouped
            .map { ContainerStack(name: $0.key, containers: $0.value) }
            .sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
        return (stacks, standalone)
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
    public var rosetta: Bool? = nil
    public var hostMemoryUsedBytes: UInt64? = nil

    public var hasHostMemory: Bool { hostMemoryUsedBytes != nil }

    public var vmMemoryUsedBytes: UInt64 {
        memoryBytes > 0 ? min(memoryBytes, memoryUsedBytes) : memoryUsedBytes
    }

    public var totalMemoryUsedBytes: UInt64 {
        guard let host = hostMemoryUsedBytes else { return vmMemoryUsedBytes }
        let combined = max(host, memoryUsedBytes)
        return memoryBytes > 0 ? min(memoryBytes, combined) : combined
    }

    public var otherHostMemoryUsedBytes: UInt64 {
        let total = totalMemoryUsedBytes
        let vm = vmMemoryUsedBytes
        return total > vm ? total - vm : 0
    }

    public var hostMemoryFraction: Double {
        memoryBytes > 0 ? Double(totalMemoryUsedBytes) / Double(memoryBytes) : 0
    }

    public var vmMemoryFraction: Double {
        memoryBytes > 0 ? Double(vmMemoryUsedBytes) / Double(memoryBytes) : 0
    }

    public var otherHostMemoryFraction: Double {
        memoryBytes > 0 ? Double(otherHostMemoryUsedBytes) / Double(memoryBytes) : 0
    }
}

/// What to run (mirrors `RunSpec` in `graviton-proto`).
public struct RunSpec: Codable, Hashable, Sendable {
    public var image: String
    public var name: String?
    /// Replaces the image's Cmd; the entrypoint is kept.
    public var command: [String]?
    /// `KEY=value` pairs added to the image's environment.
    public var env: [String]
    public var cpus: UInt32?
    public var memoryMib: UInt64?
    /// `[ip:]host:container[/tcp|udp]` or `port[/tcp|udp]` (default bind IP: 127.0.0.1).
    public var ports: [String]
    /// `/host/dir:/path[:ro]`, `~/dir:/path` or `name:/path`.
    public var volumes: [String]
    /// Automatic restart policy (`no`, `on-failure`, `always`); nil defaults to `no`.
    public var restart: RestartPolicy?
    /// Target platform when pulling the image (`linux/amd64`, `linux/arm64`).
    public var platform: String?
    /// Replaces the image's Entrypoint when set.
    public var entrypoint: [String]?
    /// Overrides the container's working directory.
    public var workingDir: String?
    /// Overrides the container's user (`uid`, `uid:gid`, or name).
    public var user: String?
    /// Overrides the container's hostname.
    public var hostname: String?
    /// Compose project (stack) name.
    public var project: String?
    /// Service name inside `project`.
    public var service: String?
    /// Arbitrary metadata labels.
    public var labels: [String: String]?
    /// Extra project-scoped DNS aliases.
    public var dnsAliases: [String]?
    /// Overrides or disables (`test: ["NONE"]`) the image's healthcheck.
    public var healthcheck: Healthcheck? = nil
    /// Mount the container's root filesystem read-only (`--read-only`).
    public var readOnly: Bool? = nil
    /// Linux capabilities added to the container (`--cap-add`).
    public var capAdd: [String]? = nil
    /// Linux capabilities dropped from the container (`--cap-drop`).
    public var capDrop: [String]? = nil
    /// Ephemeral tmpfs mounts (`--tmpfs`).
    public var tmpfs: [TmpfsMount]? = nil
    /// Run `docker-init` as container PID 1 (`--init`).
    public var `init`: Bool? = nil

    public init(image: String, name: String? = nil, command: [String]? = nil, env: [String] = [], cpus: UInt32? = nil,
                memoryMib: UInt64? = nil, ports: [String] = [], volumes: [String] = [], restart: RestartPolicy? = nil,
                platform: String? = nil, entrypoint: [String]? = nil, workingDir: String? = nil, user: String? = nil,
                hostname: String? = nil, project: String? = nil, service: String? = nil, labels: [String: String]? = nil,
                dnsAliases: [String]? = nil, healthcheck: Healthcheck? = nil,
                readOnly: Bool? = nil, capAdd: [String]? = nil, capDrop: [String]? = nil,
                tmpfs: [TmpfsMount]? = nil, initProcess: Bool? = nil) {
        self.image = image
        self.name = name
        self.command = command
        self.env = env
        self.cpus = cpus
        self.memoryMib = memoryMib
        self.ports = ports
        self.volumes = volumes
        self.restart = restart
        self.platform = platform
        self.entrypoint = entrypoint
        self.workingDir = workingDir
        self.user = user
        self.hostname = hostname
        self.project = project
        self.service = service
        self.labels = labels
        self.dnsAliases = dnsAliases
        self.healthcheck = healthcheck
        self.readOnly = readOnly
        self.capAdd = capAdd
        self.capDrop = capDrop
        self.tmpfs = tmpfs
        self.`init` = initProcess
    }

    /// Split a list typed as "8080:80, 5432" or one per line.
    public static func list(_ text: String) -> [String] {
        text.split(whereSeparator: { $0 == "," || $0.isNewline }).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
    }

    /// Split a command line into words, honouring single and double quotes and backslashes.
    public static func words(_ line: String) -> [String] {
        var words: [String] = []
        var current = ""
        var quote: Character?
        var escaped = false
        var inWord = false
        for ch in line {
            if escaped {
                current.append(ch); escaped = false; inWord = true
            } else if ch == "\\" && quote != "'" {
                escaped = true
            } else if let q = quote {
                if ch == q { quote = nil } else { current.append(ch) }
            } else if ch == "\"" || ch == "'" {
                quote = ch; inWord = true
            } else if ch.isWhitespace {
                if inWord { words.append(current); current = ""; inWord = false }
            } else {
                current.append(ch); inWord = true
            }
        }
        if inWord { words.append(current) }
        return words
    }
}

/// Push alert set-up on a Mac (`PushInfo` in `graviton-proto`).
public struct PushInfo: Codable, Hashable, Sendable {
    public var configured: Bool
    public var keyId: String?
    public var teamId: String?
    public var devices: UInt32
    public var note: String?
}

/// What `grav pair` shows (mirrors `PairingInfo` in `graviton-proto`).
public struct PairingInfo: Codable, Hashable, Sendable {
    public var code: String
    public var expiresInSecs: UInt64
    public var host: String
    public var port: UInt16
    public var fingerprint: String
    public var addresses: [String]
}

public struct PairedDevice: Codable, Hashable, Sendable, Identifiable {
    public var id: String
    public var name: String
    public var pairedAt: UInt64
    public var lastSeen: UInt64?
}

// MARK: - Milestone 5: stats, files, details, exec

/// One sample of a container's resource use.
public struct StatSample: Codable, Hashable, Sendable {
    /// Unix milliseconds.
    public var timestampMs: UInt64
    /// 100 is one CPU fully busy.
    public var cpuPercent: Double
    public var memoryBytes: UInt64
    public var memoryLimitBytes: UInt64
    /// Totals since the VM booted.
    public var netRxBytes: UInt64
    public var netTxBytes: UInt64

    public var date: Date { Date(timeIntervalSince1970: Double(timestampMs) / 1000) }
}

/// A process inside a container (`ProcessInfo` in `graviton-proto`).
public struct ContainerProcess: Codable, Hashable, Sendable, Identifiable {
    public var pid: Int32
    public var ppid: Int32
    public var uid: UInt32
    public var state: String
    public var command: String
    public var rssBytes: UInt64
    /// CPU time used, in milliseconds.
    public var cpuMs: UInt64

    public var id: Int32 { pid }

    enum CodingKeys: String, CodingKey { case pid, ppid, uid, state, command, rssBytes, cpuMs }
}

public struct ContainerStats: Codable, Hashable, Sendable {
    public var id: String
    public var cpus: UInt32
    public var intervalSecs: UInt32
    /// Oldest first.
    public var samples: [StatSample]
    public var processes: [ContainerProcess]
}

public enum FileKind: String, Codable, Hashable, Sendable {
    case file, directory, symlink, other
}

public struct FileEntry: Codable, Hashable, Sendable, Identifiable {
    public var name: String
    public var kind: FileKind
    public var size: UInt64
    public var mode: UInt32
    public var uid: UInt32
    public var gid: UInt32
    /// Unix seconds.
    public var modified: Int64
    public var linkTarget: String?

    public var id: String { name }
    public var isDirectory: Bool { kind == .directory }
    public var modifiedDate: Date { Date(timeIntervalSince1970: Double(modified)) }

    /// `drwxr-xr-x`-style permissions.
    public var permissions: String {
        let type: Character = switch kind {
        case .directory: "d"
        case .symlink: "l"
        case .file: "-"
        case .other: "?"
        }
        let bits = Array("rwxrwxrwx")
        return String(type) + String((0..<9).map { mode & (0o400 >> UInt32($0)) != 0 ? bits[$0] : "-" })
    }
}

public struct DirectoryListing: Codable, Hashable, Sendable {
    public var path: String
    public var entries: [FileEntry]
}

public struct FileContents: Codable, Hashable, Sendable {
    public var path: String
    public var size: UInt64
    public var data: [UInt8]
    public var truncated: Bool

    /// The text, if the bytes look like UTF-8 text.
    public var text: String? {
        guard !data.prefix(8192).contains(0) else { return nil }
        return String(bytes: data, encoding: .utf8)
    }
}

/// Saved OCI registry credential entry (`RegistryLoginInfo` in `graviton-proto`).
public struct RegistryLoginInfo: Codable, Identifiable, Hashable, Sendable {
    public var id: String { registry }
    public var registry: String
    public var username: String

    public init(registry: String, username: String) {
        self.registry = registry
        self.username = username
    }
}

/// Ephemeral in-memory tmpfs mount inside a container (`TmpfsMount` in `graviton-proto`).
public struct TmpfsMount: Codable, Hashable, Sendable {
    public var target: String
    public var options: String = ""

    public init(target: String, options: String = "") {
        self.target = target
        self.options = options
    }

    /// Parse `/target[:options]`; returns `nil` when empty or missing leading `/`.
    public static func parse(_ spec: String) -> TmpfsMount? {
        let trimmed = spec.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        let target: String
        let options: String
        if let colon = trimmed.firstIndex(of: ":") {
            target = String(trimmed[..<colon]).trimmingCharacters(in: .whitespaces)
            options = String(trimmed[trimmed.index(after: colon)...]).trimmingCharacters(in: .whitespaces)
        } else {
            target = trimmed
            options = ""
        }
        guard target.hasPrefix("/"), target.count >= 1 else { return nil }
        return TmpfsMount(target: target, options: options)
    }

    /// Parse newline-separated tmpfs lines (commas stay inside options like `rw,noexec,size=64m`).
    public static func parseLines(_ text: String) -> [TmpfsMount] {
        text.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
            .compactMap(parse)
    }

    /// Returns the first non-empty line in `text` that fails `parse(_:)`, if any.
    public static func invalidLine(in text: String) -> String? {
        text.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
            .first { parse($0) == nil }
    }
}

/// How a container is configured (`ContainerDetails` in `graviton-proto`).
public struct ContainerDetails: Codable, Hashable, Sendable {
    public var container: Container
    public var imageReference: String
    public var imageDigest: String
    public var command: [String]
    public var env: [String]
    public var workingDir: String
    public var user: String?
    public var hostname: String
    public var cpus: UInt32
    public var memoryMib: UInt64
    public var kernel: String
    public var vmDir: String
    public var restart: RestartPolicy?
    public var architecture: String? = nil
    public var rosetta: Bool? = nil
    public var project: String? = nil
    public var service: String? = nil
    public var labels: [String: String]? = nil
    public var healthcheck: Healthcheck? = nil
    public var health: HealthState? = nil
    public var readOnly: Bool? = nil
    public var capAdd: [String]? = nil
    public var capDrop: [String]? = nil
    public var tmpfs: [TmpfsMount]? = nil
    public var `init`: Bool? = nil

    public var usesInit: Bool { self.`init` ?? false }
}

/// What `grav exec` asks for.
public struct ExecSpec: Codable, Hashable, Sendable {
    public var id: String
    public var command: [String]
    public var tty: Bool
    public var rows: UInt16
    public var cols: UInt16
    public var env: [String]

    public init(id: String, command: [String], tty: Bool = true, rows: UInt16 = 24, cols: UInt16 = 80, env: [String] = []) {
        self.id = id
        self.command = command
        self.tty = tty
        self.rows = rows
        self.cols = cols
        self.env = env
    }
}

/// What `grav build` asks for (mirrors `BuildSpec` in `graviton-proto`).
public struct BuildSpec: Codable, Hashable, Sendable {
    /// Absolute path of the build context.
    public var context: String
    /// Relative to the context, or absolute; default `Dockerfile`.
    public var dockerfile: String?
    /// `name` or `name:tag`; default: the folder's name.
    public var tag: String?
    /// `KEY=value` for ARGs.
    public var buildArgs: [String]
    public var target: String?
    public var noCache: Bool
    /// Bytes of tar that follow the request (a folder sent to another Mac).
    public var upload: UInt64?
    /// Target platform (`linux/amd64`, `linux/arm64`).
    public var platform: String?

    public init(context: String, dockerfile: String? = nil, tag: String? = nil, buildArgs: [String] = [], target: String? = nil, noCache: Bool = false, platform: String? = nil) {
        self.context = context
        self.dockerfile = dockerfile
        self.tag = tag
        self.buildArgs = buildArgs
        self.target = target
        self.noCache = noCache
        self.platform = platform
    }

    /// The image name a build of `context` gets when none is given: the folder's name.
    public static func defaultName(for context: String) -> String {
        let base = URL(fileURLWithPath: context).lastPathComponent.lowercased()
        let cleaned = String(base.map { $0.isASCII && ($0.isLetter || $0.isNumber || "-_.".contains($0)) ? $0 : "-" })
            .trimmingCharacters(in: CharacterSet(charactersIn: "-._"))
        return cleaned.isEmpty ? "graviton-build" : cleaned
    }
}

/// Progress from a build.
public enum BuildEvent: Sendable, Equatable {
    case log(String)
    case built(ImageInfo)
}

/// Progress on one layer during a pull (mirrors `PullProgress::Layer` in `graviton-proto`).
public struct LayerPullProgress: Hashable, Sendable, Identifiable {
    public var index: Int
    public var totalLayers: Int
    public var digest: String
    public var done: UInt64
    public var total: UInt64
    public var cached: Bool

    public init(index: Int, totalLayers: Int, digest: String, done: UInt64, total: UInt64, cached: Bool) {
        self.index = index
        self.totalLayers = totalLayers
        self.digest = digest
        self.done = done
        self.total = total
        self.cached = cached
    }

    public var id: Int { index }

    public var shortDigest: String {
        let stripped = digest.hasPrefix("sha256:") ? String(digest.dropFirst(7)) : digest
        return String(stripped.prefix(12))
    }

    public var isComplete: Bool { cached || (total > 0 && done >= total) }

    public var fraction: Double {
        if cached { return 1 }
        guard total > 0 else { return 0 }
        return min(1, max(0, Double(done) / Double(total)))
    }
}

/// Per-layer pull progress streamed by `gravitond` before the final image.
public enum PullProgress: Decodable, Hashable, Sendable {
    case resolved(layers: Int, totalBytes: UInt64)
    case layer(LayerPullProgress)

    private enum Keys: String, CodingKey {
        case stage, layers, totalBytes, index, totalLayers, digest, done, total, cached
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let stage = try c.decode(String.self, forKey: .stage)
        switch stage {
        case "resolved":
            self = .resolved(
                layers: try c.decode(Int.self, forKey: .layers),
                totalBytes: try c.decode(UInt64.self, forKey: .totalBytes)
            )
        case "layer":
            self = .layer(LayerPullProgress(
                index: try c.decode(Int.self, forKey: .index),
                totalLayers: try c.decode(Int.self, forKey: .totalLayers),
                digest: try c.decode(String.self, forKey: .digest),
                done: try c.decode(UInt64.self, forKey: .done),
                total: try c.decode(UInt64.self, forKey: .total),
                cached: try c.decode(Bool.self, forKey: .cached)
            ))
        default:
            throw DecodingError.dataCorruptedError(forKey: .stage, in: c, debugDescription: "unknown pull stage \(stage)")
        }
    }
}

/// Progress from a streaming image pull.
public enum PullEvent: Sendable, Equatable {
    case progress(PullProgress)
    case pulled(ImageInfo)
}

/// Accumulates streamed `PullProgress` events for Mac and iOS Pull sheets.
public struct PullState: Hashable, Sendable {
    public var totalLayers: Int = 0
    public var totalBytes: UInt64 = 0
    public var layers: [LayerPullProgress] = []
    public var resolved: Bool = false

    public init() {}

    public mutating func apply(_ progress: PullProgress) {
        switch progress {
        case let .resolved(count, bytes):
            resolved = true
            totalLayers = count
            totalBytes = bytes
        case let .layer(layer):
            resolved = true
            totalLayers = max(totalLayers, layer.totalLayers, layer.index + 1)
            if let existing = layers.firstIndex(where: { $0.index == layer.index }) {
                layers[existing] = layer
            } else {
                layers.append(layer)
                layers.sort { $0.index < $1.index }
            }
        }
    }

    public var completedLayers: Int { layers.filter(\.isComplete).count }

    public var downloadedBytes: UInt64 {
        layers.reduce(0) { sum, l in sum + (l.cached ? l.total : min(l.done, l.total)) }
    }

    public var fraction: Double? {
        guard resolved else { return nil }
        if totalBytes > 0 {
            return min(1, Double(downloadedBytes) / Double(totalBytes))
        }
        if totalLayers > 0 {
            return min(1, Double(completedLayers) / Double(totalLayers))
        }
        return 1
    }
}

/// Bytes per second between consecutive stat samples.
public struct NetworkRate: Identifiable, Sendable {
    public var date: Date
    public var received: Double
    public var sent: Double
    public var id: Date { date }

    public static func series(_ samples: [StatSample]) -> [NetworkRate] {
        zip(samples, samples.dropFirst()).compactMap { a, b in
            guard b.timestampMs > a.timestampMs else { return nil }
            let secs = Double(b.timestampMs - a.timestampMs) / 1000
            let rx = b.netRxBytes >= a.netRxBytes ? b.netRxBytes - a.netRxBytes : b.netRxBytes
            let tx = b.netTxBytes >= a.netTxBytes ? b.netTxBytes - a.netTxBytes : b.netTxBytes
            return NetworkRate(date: b.date,
                               received: Double(rx) / secs,
                               sent: Double(tx) / secs)
        }
    }
}

/// Something that happened on the engine (`EngineEvent` in `graviton-proto`).
public struct EngineEvent: Codable, Identifiable, Hashable, Sendable {
    public enum Kind: String, Codable, Sendable {
        case started, stopped, paused, unpaused, failed, healthy, unhealthy, pulled, pushed, built, buildFailed = "build_failed", removed
    }

    public var seq: UInt64
    public var timestampMs: UInt64
    public var kind: Kind
    public var containerId: String?
    public var container: String?
    public var image: String?
    public var message: String

    public var id: UInt64 { seq }
    public var date: Date { Date(timeIntervalSince1970: Double(timestampMs) / 1000) }
    public var isProblem: Bool { kind == .failed || kind == .unhealthy || kind == .buildFailed }

    public init(seq: UInt64, timestampMs: UInt64, kind: Kind, containerId: String? = nil, container: String? = nil, image: String? = nil, message: String) {
        self.seq = seq
        self.timestampMs = timestampMs
        self.kind = kind
        self.containerId = containerId
        self.container = container
        self.image = image
        self.message = message
    }

    enum CodingKeys: String, CodingKey { case seq, timestampMs, kind, containerId, container, image, message }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        seq = try c.decode(UInt64.self, forKey: .seq)
        timestampMs = try c.decode(UInt64.self, forKey: .timestampMs)
        // convertFromSnakeCase turns "build_failed" into "buildFailed"; accept both.
        let raw = try c.decode(String.self, forKey: .kind)
        kind = Kind(rawValue: raw) ?? (raw == "buildFailed" ? .buildFailed : .stopped)
        containerId = try c.decodeIfPresent(String.self, forKey: .containerId)
        container = try c.decodeIfPresent(String.self, forKey: .container)
        image = try c.decodeIfPresent(String.self, forKey: .image)
        message = try c.decode(String.self, forKey: .message)
    }
}

// MARK: - Disk space

/// Where the disk space goes (`grav df`).
public struct DiskUsage: Codable, Hashable, Sendable {
    public var images: UInt32
    public var unusedImages: UInt32
    public var blobsBytes: UInt64
    public var unusedImagesBytes: UInt64
    public var containers: UInt32
    public var stoppedContainers: UInt32
    public var containerDisksBytes: UInt64
    public var stoppedDisksBytes: UInt64
    public var buildCacheSteps: UInt32
    public var buildCacheBytes: UInt64
    public var unreferencedBytes: UInt64
    public var snapshots: UInt32? = nil
    public var snapshotsBytes: UInt64? = nil

    /// What a plain prune frees: the build cache, idle VM snapshots, and unreferenced layers.
    public var alwaysReclaimable: UInt64 { buildCacheBytes + (snapshotsBytes ?? 0) + unreferencedBytes }
}

/// What a prune removes; the build cache and unreferenced layers always go.
public struct PruneSpec: Codable, Hashable, Sendable {
    public var images: Bool
    public var containers: Bool
    public var dryRun: Bool

    public init(images: Bool = false, containers: Bool = false, dryRun: Bool = false) {
        self.images = images
        self.containers = containers
        self.dryRun = dryRun
    }
}

public struct PruneReport: Codable, Hashable, Sendable {
    public var containers: [String]
    public var images: [String]
    public var buildCacheSteps: UInt32
    public var blobs: UInt32
    public var freedBytes: UInt64
    public var dryRun: Bool
    public var snapshots: UInt32? = nil
}

// MARK: - Volumes & network

/// One container's mount of a named volume (mirrors `VolumeMountRef` in `graviton-proto`).
public struct VolumeMountRef: Codable, Hashable, Sendable, Identifiable {
    public var containerId: String
    public var containerName: String
    public var target: String
    public var readOnly: Bool
    public var running: Bool

    public var id: String { "\(containerId):\(target)" }

    public init(containerId: String, containerName: String, target: String, readOnly: Bool, running: Bool) {
        self.containerId = containerId
        self.containerName = containerName
        self.target = target
        self.readOnly = readOnly
        self.running = running
    }
}

/// One named volume (`-v name:/path`) stored under Graviton's `volumes/` folder.
public struct VolumeInfo: Codable, Hashable, Sendable, Identifiable {
    public var name: String
    public var path: String
    public var sizeBytes: UInt64
    public var fileCount: UInt64
    public var createdAt: UInt64
    public var usedBy: [VolumeMountRef]

    public var id: String { name }
    public var isInUse: Bool { !usedBy.isEmpty }
    public var createdDate: Date { Date(timeIntervalSince1970: Double(createdAt)) }

    public init(name: String, path: String, sizeBytes: UInt64, fileCount: UInt64, createdAt: UInt64, usedBy: [VolumeMountRef] = []) {
        self.name = name
        self.path = path
        self.sizeBytes = sizeBytes
        self.fileCount = fileCount
        self.createdAt = createdAt
        self.usedBy = usedBy
    }
}

/// One container on Graviton's shared container network (`NetworkEndpoint` in `graviton-proto`).
public struct NetworkEndpoint: Codable, Hashable, Sendable, Identifiable {
    public var containerId: String
    public var containerName: String
    public var image: String
    public var status: ContainerStatus
    public var ipAddress: String?
    public var dnsNames: [String]
    public var dnsEnabled: Bool
    public var ports: [PortMapping]

    public var id: String { containerId }
    public var isRunning: Bool { status == .running }
    public var isPaused: Bool { status == .paused }
    public var isActive: Bool { status == .running || status == .paused }

    public init(containerId: String, containerName: String, image: String, status: ContainerStatus,
                ipAddress: String? = nil, dnsNames: [String] = [], dnsEnabled: Bool = false, ports: [PortMapping] = []) {
        self.containerId = containerId
        self.containerName = containerName
        self.image = image
        self.status = status
        self.ipAddress = ipAddress
        self.dnsNames = dnsNames
        self.dnsEnabled = dnsEnabled
        self.ports = ports
    }
}

/// Live view of Graviton's shared container network, switch, and embedded DNS.
public struct NetworkInfo: Codable, Hashable, Sendable {
    public var name: String
    public var driver: String
    public var subnet: String
    public var gateway: String
    public var dnsServer: String
    public var dnsDomain: String
    public var connectedPorts: UInt32
    public var learnedMacs: UInt32
    public var framesForwarded: UInt64
    public var bytesForwarded: UInt64
    public var endpoints: [NetworkEndpoint]

    public init(name: String, driver: String, subnet: String, gateway: String, dnsServer: String, dnsDomain: String,
                connectedPorts: UInt32, learnedMacs: UInt32, framesForwarded: UInt64, bytesForwarded: UInt64, endpoints: [NetworkEndpoint]) {
        self.name = name
        self.driver = driver
        self.subnet = subnet
        self.gateway = gateway
        self.dnsServer = dnsServer
        self.dnsDomain = dnsDomain
        self.connectedPorts = connectedPorts
        self.learnedMacs = learnedMacs
        self.framesForwarded = framesForwarded
        self.bytesForwarded = bytesForwarded
        self.endpoints = endpoints
    }

    /// Synthesise a network view from `containers` when talking to an older gravitond.
    public static func synthesized(from containers: [Container]) -> NetworkInfo {
        let running = containers.filter(\.isRunning)
        let subnetAndGateway: (String, String) = running.compactMap(\.ipAddress).compactMap { ip -> (String, String)? in
            let parts = ip.split(separator: ".")
            guard parts.count == 4 else { return nil }
            let prefix = "\(parts[0]).\(parts[1]).\(parts[2])"
            return ("\(prefix).0/24", "\(prefix).1")
        }.first ?? ("192.168.64.0/24", "192.168.64.1")
        let endpoints = containers.map { c in
            var dns: [String] = []
            if c.isRunning && c.ipAddress != nil {
                dns = [c.name, "\(c.name).graviton", c.id]
                if let svc = c.service, !svc.isEmpty, svc.lowercased() != c.name.lowercased() {
                    dns.append(svc)
                    dns.append("\(svc).graviton")
                }
            }
            return NetworkEndpoint(
                containerId: c.id,
                containerName: c.name,
                image: c.image,
                status: c.status,
                ipAddress: c.isRunning ? c.ipAddress : nil,
                dnsNames: dns,
                dnsEnabled: c.isRunning && c.ipAddress != nil,
                ports: c.ports
            )
        }.sorted { a, b in
            if a.isRunning != b.isRunning { return a.isRunning }
            return a.containerName.localizedStandardCompare(b.containerName) == .orderedAscending
        }
        return NetworkInfo(
            name: "graviton",
            driver: "L2 switch + NAT",
            subnet: subnetAndGateway.0,
            gateway: subnetAndGateway.1,
            dnsServer: "127.0.0.11:53",
            dnsDomain: "graviton",
            connectedPorts: UInt32(running.count),
            learnedMacs: UInt32(running.count),
            framesForwarded: 0,
            bytesForwarded: 0,
            endpoints: endpoints
        )
    }
}

/// Cached VM memory snapshot pool for one VM shape (`grav snapshot status`).
public struct SnapshotGroup: Codable, Hashable, Sendable {
    public var cpus: UInt32
    public var memoryMib: UInt64
    public var rosetta: Bool
    public var slots: UInt32
    public var inUse: UInt32
    public var sizeBytes: UInt64

    public init(cpus: UInt32, memoryMib: UInt64, rosetta: Bool, slots: UInt32, inUse: UInt32, sizeBytes: UInt64) {
        self.cpus = cpus
        self.memoryMib = memoryMib
        self.rosetta = rosetta
        self.slots = slots
        self.inUse = inUse
        self.sizeBytes = sizeBytes
    }
}

/// Summary of the daemon's VM snapshot pool (`grav snapshot status|warm|clear`).
public struct SnapshotInfo: Codable, Hashable, Sendable {
    public var enabled: Bool
    public var totalSlots: UInt32
    public var inUse: UInt32
    public var totalBytes: UInt64
    public var maxSlotsPerShape: UInt32
    public var groups: [SnapshotGroup]
    public var pausedReason: String?

    public init(enabled: Bool = true, totalSlots: UInt32 = 0, inUse: UInt32 = 0, totalBytes: UInt64 = 0,
                maxSlotsPerShape: UInt32 = 8, groups: [SnapshotGroup] = [], pausedReason: String? = nil) {
        self.enabled = enabled
        self.totalSlots = totalSlots
        self.inUse = inUse
        self.totalBytes = totalBytes
        self.maxSlotsPerShape = maxSlotsPerShape
        self.groups = groups
        self.pausedReason = pausedReason
    }
}

/// Result of pushing a local OCI image to a registry (`grav push IMAGE`).
public struct ImagePushResult: Codable, Hashable, Sendable {
    public var reference: String
    public var digest: String
    public var sizeBytes: UInt64
    public var blobsPushed: UInt32
    public var blobsSkipped: UInt32

    public init(reference: String, digest: String, sizeBytes: UInt64, blobsPushed: UInt32, blobsSkipped: UInt32) {
        self.reference = reference
        self.digest = digest
        self.sizeBytes = sizeBytes
        self.blobsPushed = blobsPushed
        self.blobsSkipped = blobsSkipped
    }
}

/// Result of running a non-interactive command inside a running container (`grav exec`).
public struct ExecResult: Hashable, Sendable {
    public var exitCode: Int32
    public var output: String
    public var truncated: Bool

    public init(exitCode: Int32, output: String, truncated: Bool = false) {
        self.exitCode = exitCode
        self.output = output
        self.truncated = truncated
    }
}

/// Snapshot a container's filesystem and configuration changes into a local image (`grav commit`).
public struct CommitSpec: Codable, Hashable, Sendable {
    public var container: String
    /// Target image reference (`repo[:tag]`); empty lets the daemon default to `committed:<short_id>`.
    public var reference: String
    public var author: String?
    public var message: String?
    /// Dockerfile config instructions (`ENV`, `CMD`, `ENTRYPOINT`, `WORKDIR`, `USER`, `EXPOSE`, `LABEL`, `VOLUME`, `STOPSIGNAL`, `HEALTHCHECK`).
    public var changes: [String]

    public init(container: String, reference: String = "", author: String? = nil, message: String? = nil, changes: [String] = []) {
        self.container = container
        self.reference = reference
        self.author = author
        self.message = message
        self.changes = changes
    }

    enum CodingKeys: String, CodingKey {
        case container, reference, author, message, changes
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        container = try c.decode(String.self, forKey: .container)
        reference = try c.decodeIfPresent(String.self, forKey: .reference) ?? ""
        author = try c.decodeIfPresent(String.self, forKey: .author)
        message = try c.decodeIfPresent(String.self, forKey: .message)
        changes = try c.decodeIfPresent([String].self, forKey: .changes) ?? []
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(container, forKey: .container)
        try c.encode(reference, forKey: .reference)
        try c.encodeIfPresent(author, forKey: .author)
        try c.encodeIfPresent(message, forKey: .message)
        if !changes.isEmpty {
            try c.encode(changes, forKey: .changes)
        }
    }
}

/// Time-window filter presets for container log views (`All`, `5m`, `15m`, `1h`).
public enum LogTimeWindow: String, CaseIterable, Identifiable, Sendable, Hashable {
    case all = "All"
    case fiveMinutes = "5m"
    case fifteenMinutes = "15m"
    case oneHour = "1h"

    public var id: String { rawValue }

    public func sinceMs(now: Date = .now) -> UInt64? {
        let seconds: TimeInterval
        switch self {
        case .all: return nil
        case .fiveMinutes: seconds = 5 * 60
        case .fifteenMinutes: seconds = 15 * 60
        case .oneHour: seconds = 60 * 60
        }
        return UInt64(max(0, (now.timeIntervalSince1970 - seconds) * 1000))
    }
}
