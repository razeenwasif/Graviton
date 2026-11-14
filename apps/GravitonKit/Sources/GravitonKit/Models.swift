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
    /// `host:container` or `port`, published on 127.0.0.1.
    public var ports: [String]
    /// `/host/dir:/path[:ro]`, `~/dir:/path` or `name:/path`.
    public var volumes: [String]

    public init(image: String, name: String? = nil, command: [String]? = nil, env: [String] = [], cpus: UInt32? = nil,
                memoryMib: UInt64? = nil, ports: [String] = [], volumes: [String] = []) {
        self.image = image
        self.name = name
        self.command = command
        self.env = env
        self.cpus = cpus
        self.memoryMib = memoryMib
        self.ports = ports
        self.volumes = volumes
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

    public init(context: String, dockerfile: String? = nil, tag: String? = nil, buildArgs: [String] = [], target: String? = nil, noCache: Bool = false) {
        self.context = context
        self.dockerfile = dockerfile
        self.tag = tag
        self.buildArgs = buildArgs
        self.target = target
        self.noCache = noCache
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
