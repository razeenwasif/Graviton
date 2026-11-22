import Foundation
#if os(macOS)
import Security
#endif

// MARK: - JSON value for Fairy tool calls

/// Strictly `Sendable` JSON value for Fairy tool arguments and structured tool results.
public enum FairyJSON: Codable, Hashable, Sendable {
    case null
    case bool(Bool)
    case int(Int64)
    case double(Double)
    case string(String)
    case array([FairyJSON])
    case object([String: FairyJSON])

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let b = try? c.decode(Bool.self) {
            self = .bool(b)
        } else if let i = try? c.decode(Int64.self) {
            self = .int(i)
        } else if let d = try? c.decode(Double.self) {
            self = .double(d)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else if let arr = try? c.decode([FairyJSON].self) {
            self = .array(arr)
        } else if let obj = try? c.decode([String: FairyJSON].self) {
            self = .object(obj)
        } else {
            throw DecodingError.dataCorruptedError(in: c, debugDescription: "Unsupported JSON value")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case let .bool(b): try c.encode(b)
        case let .int(i): try c.encode(i)
        case let .double(d): try c.encode(d)
        case let .string(s): try c.encode(s)
        case let .array(arr): try c.encode(arr)
        case let .object(obj): try c.encode(obj)
        }
    }

    public var stringValue: String? {
        if case let .string(s) = self { return s }
        return nil
    }

    public var boolValue: Bool? {
        if case let .bool(b) = self { return b }
        return nil
    }

    public var intValue: Int? {
        if case let .int(i) = self, let exact = Int(exactly: i) { return exact }
        return nil
    }

    public var stringArrayValue: [String]? {
        guard case let .array(arr) = self else { return nil }
        return arr.compactMap(\.stringValue)
    }

    public var objectValue: [String: FairyJSON]? {
        if case let .object(obj) = self { return obj }
        return nil
    }

    public static func fromEncodable<T: Encodable>(_ value: T) throws -> FairyJSON {
        let data = try JSONEncoder.gravitonPlain.encode(value)
        return try JSONDecoder().decode(FairyJSON.self, from: data)
    }
}

private extension JSONEncoder {
    static var gravitonPlain: JSONEncoder {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        e.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return e
    }
}

// MARK: - Approval Policy & Summaries

public struct FairyActionSummary: Hashable, Sendable {
    public var title: String
    public var detail: String
    public var touchIDReason: String
    public var isDestructive: Bool

    public init(title: String, detail: String, touchIDReason: String, isDestructive: Bool) {
        self.title = title
        self.detail = detail
        self.touchIDReason = touchIDReason
        self.isDestructive = isDestructive
    }
}

/// Enforces Graviton's tiered Touch ID policy and formats prompts for Fairy tool calls.
public enum FairyApprovalPolicy {
    public static let supportedTools: Set<String> = [
        "status", "events", "switch_host", "navigate",
        "containers", "inspect", "logs", "stats",
        "container_action", "run", "exec", "stack_action", "compose",
        "list_files", "read_file", "copy_files",
        "images", "pull", "build", "tag_image", "push_image", "remove_image",
        "volumes", "volume_action", "network", "disk_and_prune", "snapshots",
    ]

    /// Tiered Touch ID gate:
    /// - Reads & reversible low-risk actions (`start`, `restart`, `pull`, `tag_image`, `create` volume, `navigate`, `switch_host`, dry-runs) run automatically.
    /// - High-impact execution & destructive mutations (or registry uploads using credentials) require Touch ID approval.
    /// - Unknown tools/actions fail closed (`true`).
    public static func requiresTouchID(tool: String, args: [String: FairyJSON]) -> Bool {
        let action = args["action"]?.stringValue ?? ""
        switch tool {
        case "status", "events", "containers", "inspect", "logs", "stats",
             "list_files", "read_file", "images", "volumes", "network",
             "navigate", "switch_host", "pull", "tag_image":
            return false
        case "container_action", "stack_action":
            return action != "start" && action != "restart"
        case "volume_action":
            return action != "create"
        case "disk_and_prune":
            return action != "usage" && action != "prune_dry_run"
        case "snapshots":
            return action != "status"
        case "compose":
            return !["config", "ps", "logs", "start", "restart"].contains(action)
        case "run", "exec", "build", "push_image", "remove_image", "copy_files":
            return true
        default:
            return true
        }
    }

    /// Mask secret-looking environment variables unless the user explicitly requested values.
    public static func redactEnv(_ env: [String], includeValues: Bool) -> [String] {
        let secretMarkers = ["KEY", "SECRET", "TOKEN", "PASS", "PWD", "AUTH", "CREDENTIAL", "PRIVATE"]
        return env.map { entry in
            guard let eq = entry.firstIndex(of: "=") else { return entry }
            let key = String(entry[..<eq])
            let val = String(entry[entry.index(after: eq)...])
            let upper = key.uppercased()
            let looksSecret = secretMarkers.contains { upper.contains($0) }
            if looksSecret || !includeValues {
                return "\(key)=[redacted (\(val.count) chars)]"
            }
            return entry
        }
    }

    public static func summarize(tool: String, args: [String: FairyJSON], host: String) -> FairyActionSummary {
        let str = { (k: String) -> String in args[k]?.stringValue ?? "" }
        let reasonArg = str("reason")
        let suffix = host.isEmpty ? "" : " on \(host)"

        let title: String
        let detail: String
        let macReason: String
        let destructive: Bool

        switch tool {
        case "container_action":
            let act = str("action")
            let target = str("container")
            destructive = act == "remove" || act == "stop"
            title = "\(act.capitalized) container \(target)"
            detail = reasonArg.isEmpty ? "Target: \(target)" : "\(target) — \(reasonArg)"
            macReason = "let Fairy \(act) container \(target)\(suffix)"
        case "stack_action":
            let act = str("action")
            let proj = str("project")
            destructive = act == "stop"
            title = "\(act.capitalized) stack \(proj)"
            detail = reasonArg.isEmpty ? "Project: \(proj)" : "\(proj) — \(reasonArg)"
            macReason = "let Fairy \(act) stack \(proj)\(suffix)"
        case "run":
            let img = str("image")
            let name = str("name")
            destructive = false
            title = "Run \(name.isEmpty ? img : "\(name) (\(img))")"
            var parts: [String] = ["Image: \(img)"]
            if let cmd = args["command"]?.stringArrayValue, !cmd.isEmpty { parts.append("Cmd: \(cmd.joined(separator: " "))") }
            if let ports = args["ports"]?.stringArrayValue, !ports.isEmpty { parts.append("Ports: \(ports.joined(separator: ", "))") }
            if let vols = args["volumes"]?.stringArrayValue, !vols.isEmpty { parts.append("Volumes: \(vols.joined(separator: ", "))") }
            if !reasonArg.isEmpty { parts.append(reasonArg) }
            detail = parts.joined(separator: "\n")
            macReason = "let Fairy run container \(img)\(suffix)"
        case "exec":
            let c = str("container")
            let cmd = (args["command"]?.stringArrayValue ?? []).joined(separator: " ")
            destructive = false
            title = "Exec in \(c)"
            detail = cmd + (reasonArg.isEmpty ? "" : "\n\(reasonArg)")
            macReason = "let Fairy run a command in \(c)\(suffix)"
        case "build":
            let ctx = str("context")
            let tag = str("tag")
            destructive = false
            title = "Build \(tag.isEmpty ? ctx : tag)"
            detail = "Context: \(ctx)" + (reasonArg.isEmpty ? "" : "\n\(reasonArg)")
            macReason = "let Fairy build an image from \(ctx)\(suffix)"
        case "compose":
            let act = str("action")
            let dir = str("directory")
            let removeVols = args["volumes"]?.boolValue == true
            destructive = act == "down" || act == "stop"
            title = "Compose \(act)\(removeVols && act == "down" ? " -v" : "")"
            detail = "Folder: \(dir)" + (reasonArg.isEmpty ? "" : "\n\(reasonArg)")
            macReason = "let Fairy run grav compose \(act) in \(dir)\(suffix)"
        case "copy_files":
            let dir = str("direction")
            let c = str("container")
            let localPath = str("local_path")
            let remotePath = str("container_path")
            destructive = false
            title = "\(dir == "upload" ? "Upload to" : "Download from") \(c)"
            detail = dir == "upload" ? "\(localPath) → \(c):\(remotePath)" : "\(c):\(remotePath) → \(localPath)"
            macReason = "let Fairy copy files \(dir == "upload" ? "into" : "from") \(c)\(suffix)"
        case "remove_image":
            let ref = str("reference")
            destructive = true
            title = "Remove image \(ref)"
            detail = reasonArg.isEmpty ? ref : "\(ref) — \(reasonArg)"
            macReason = "let Fairy remove image \(ref)\(suffix)"
        case "volume_action":
            let act = str("action")
            let name = str("name")
            destructive = act == "remove"
            title = "\(act.capitalized) volume \(name)"
            detail = reasonArg.isEmpty ? name : "\(name) — \(reasonArg)"
            macReason = "let Fairy \(act) volume \(name)\(suffix)"
        case "disk_and_prune":
            let act = str("action")
            destructive = act == "prune"
            var scopes = ["build cache & unreferenced layers"]
            if args["images"]?.boolValue == true { scopes.append("unused images") }
            if args["containers"]?.boolValue == true { scopes.append("stopped containers") }
            title = act == "prune" ? "Prune Graviton storage" : "Disk action: \(act)"
            detail = scopes.joined(separator: ", ") + (reasonArg.isEmpty ? "" : "\n\(reasonArg)")
            macReason = "let Fairy prune unused Graviton data\(suffix)"
        case "snapshots":
            let act = str("action")
            destructive = act == "clear"
            title = "VM snapshots: \(act)"
            detail = reasonArg.isEmpty ? act : reasonArg
            macReason = "let Fairy \(act) VM snapshots\(suffix)"
        case "switch_host":
            let h = str("host")
            destructive = false
            title = "Switch host to \(h)"
            detail = reasonArg
            macReason = "let Fairy switch Graviton host to \(h)"
        case "pull":
            let ref = str("reference")
            destructive = false
            title = "Pull image \(ref)"
            detail = reasonArg.isEmpty ? ref : "\(ref) — \(reasonArg)"
            macReason = "let Fairy pull \(ref)\(suffix)"
        case "tag_image":
            let src = str("source")
            let dst = str("target")
            destructive = false
            title = "Tag image \(src) → \(dst)"
            detail = reasonArg.isEmpty ? "\(src) → \(dst)" : "\(src) → \(dst)\n\(reasonArg)"
            macReason = "let Fairy tag image \(src) as \(dst)\(suffix)"
        case "push_image":
            let ref = str("reference")
            destructive = false
            title = "Push image \(ref)"
            detail = reasonArg.isEmpty ? ref : "\(ref) — \(reasonArg)"
            macReason = "let Fairy push image \(ref) to its registry\(suffix)"
        default:
            destructive = requiresTouchID(tool: tool, args: args)
            title = "Run \(tool)"
            detail = reasonArg.isEmpty ? tool : reasonArg
            macReason = "let Fairy run \(tool)\(suffix)"
        }

        return FairyActionSummary(title: title, detail: detail, touchIDReason: macReason, isDestructive: destructive)
    }
}

// MARK: - Wire Types

public struct FairyAskContext: Encodable, Hashable, Sendable {
    public var host: String
    public var connection: String
    public var section: String
    public var selectedContainer: String?
    public var summary: String?
    public var logsTail: String?

    public init(host: String, connection: String, section: String,
                selectedContainer: String? = nil, summary: String? = nil, logsTail: String? = nil) {
        self.host = host
        self.connection = connection
        self.section = section
        self.selectedContainer = selectedContainer
        self.summary = summary
        self.logsTail = logsTail
    }

    enum CodingKeys: String, CodingKey {
        case host, connection, section
        case selectedContainer = "selected_container"
        case summary
        case logsTail = "logs_tail"
    }
}

public enum FairyLinkEvent: Sendable, Equatable {
    case waking
    case hello(name: String)
    case text(String)
    case done(String)
    case spoken
}

public struct FairyToolGrant: Hashable, Sendable, Encodable {
    public var socket: String
    public var token: String

    public init(socket: String, token: String) {
        self.socket = socket
        self.token = token
    }
}

public struct FairyToolRequest: Codable, Sendable {
    public var id: Int?
    public var token: String?
    public var tool: String?
    public var args: [String: FairyJSON]?
}

public struct FairyToolResponse: Encodable, Sendable {
    public var id: FairyJSON
    public var result: FairyJSON?
    public var error: String?

    public init(id: FairyJSON, result: FairyJSON? = nil, error: String? = nil) {
        self.id = id
        self.result = result
        self.error = error
    }
}

#if os(macOS)

// MARK: - Socket Paths & POSIX Helpers (macOS)

public enum FairyPaths {
    public static let bundleID = "dev.fairy.companion"

    public static var fairySocketPath: String {
        if let override = ProcessInfo.processInfo.environment["GRAVITON_FAIRY_SOCKET"], !override.isEmpty {
            return override
        }
        return NSHomeDirectory() + "/Library/Application Support/lyra-companion/lyra/graviton.sock"
    }

    public static var toolsSocketPath: String {
        if let override = ProcessInfo.processInfo.environment["GRAVITON_FAIRY_TOOLS_SOCKET"], !override.isEmpty {
            return override
        }
        return NSHomeDirectory() + "/Library/Application Support/Graviton/fairy-tools.sock"
    }

    public static var historyFileURL: URL {
        if let override = ProcessInfo.processInfo.environment["GRAVITON_FAIRY_HISTORY"], !override.isEmpty {
            return URL(fileURLWithPath: override)
        }
        return URL(fileURLWithPath: NSHomeDirectory() + "/Library/Application Support/Graviton/fairy-history.json")
    }

    public static var isFairyInstalled: Bool {
        let userApp = NSHomeDirectory() + "/Applications/Fairy.app"
        return FileManager.default.fileExists(atPath: userApp)
            || FileManager.default.fileExists(atPath: "/Applications/Fairy.app")
    }
}

private enum UnixSocketIO {
    static func connect(path: String) throws -> Int32 {
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw EngineError.io(String(cString: strerror(errno))) }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let capacity = MemoryLayout.size(ofValue: addr.sun_path)
        guard path.utf8.count < capacity else {
            Darwin.close(fd)
            throw EngineError.pathTooLong
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { raw in
            raw.withMemoryRebound(to: CChar.self, capacity: capacity) { _ = strncpy($0, path, capacity - 1) }
        }
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard rc == 0 else {
            let err = String(cString: strerror(errno))
            Darwin.close(fd)
            throw EngineError.unreachable(err)
        }
        var noSigPipe: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
        return fd
    }

    static func bindAndListen(path: String) throws -> Int32 {
        let dir = (path as NSString).deletingLastPathComponent
        if !dir.isEmpty {
            try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        }
        unlink(path)
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw EngineError.io(String(cString: strerror(errno))) }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let capacity = MemoryLayout.size(ofValue: addr.sun_path)
        guard path.utf8.count < capacity else {
            Darwin.close(fd)
            throw EngineError.pathTooLong
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { raw in
            raw.withMemoryRebound(to: CChar.self, capacity: capacity) { _ = strncpy($0, path, capacity - 1) }
        }
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bound == 0 else {
            let err = String(cString: strerror(errno))
            Darwin.close(fd)
            throw EngineError.io(err)
        }
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: path)
        guard Darwin.listen(fd, 32) == 0 else {
            let err = String(cString: strerror(errno))
            Darwin.close(fd)
            unlink(path)
            throw EngineError.io(err)
        }
        return fd
    }

    static func writeAll(_ fd: Int32, _ data: Data) throws {
        try data.withUnsafeBytes { raw in
            var sent = 0
            while sent < raw.count {
                let n = Darwin.write(fd, raw.baseAddress! + sent, raw.count - sent)
                guard n > 0 else { throw EngineError.io(String(cString: strerror(errno))) }
                sent += n
            }
        }
    }

    static func readLine(_ fd: Int32, maxBytes: Int = 1 << 20) throws -> Data? {
        var line = Data()
        var byte: UInt8 = 0
        while line.count < maxBytes {
            let n = Darwin.read(fd, &byte, 1)
            if n == 0 {
                return line.isEmpty ? nil : line
            }
            if n < 0 {
                throw EngineError.io(String(cString: strerror(errno)))
            }
            if byte == UInt8(ascii: "\n") {
                return line
            }
            line.append(byte)
        }
        throw EngineError.daemon("line exceeded \(maxBytes) bytes")
    }
}

// MARK: - Graviton Tool Socket Server (for Fairy)

/// Listens on `fairy-tools.sock` (`0600`) and serves Fairy's `graviton_*` tool calls,
/// authenticating each request with a per-turn ephemeral hex token.
public final class FairyToolServer: @unchecked Sendable {
    public typealias ToolHandler = @Sendable (_ tool: String, _ args: [String: FairyJSON]) async throws -> FairyJSON

    public let socketPath: String
    private let lock = NSLock()
    private var tokens: Set<String> = []
    private var listenFD: Int32 = -1
    private var stopped = false
    private var handler: ToolHandler?

    public init(socketPath: String = FairyPaths.toolsSocketPath) {
        self.socketPath = socketPath
    }

    deinit {
        stop()
    }

    public func start(handler: @escaping ToolHandler) throws {
        lock.lock()
        defer { lock.unlock() }
        guard listenFD < 0 else {
            self.handler = handler
            return
        }
        let fd = try UnixSocketIO.bindAndListen(path: socketPath)
        self.handler = handler
        self.listenFD = fd
        self.stopped = false

        let thread = Thread { [weak self] in
            self?.acceptLoop(fd: fd)
        }
        thread.name = "graviton-fairy-tools"
        thread.start()
    }

    public func stop() {
        lock.lock()
        stopped = true
        let fd = listenFD
        listenFD = -1
        tokens.removeAll()
        lock.unlock()
        if fd >= 0 {
            Darwin.close(fd)
            unlink(socketPath)
        }
    }

    /// Issue a fresh 64-hex-char token for a single Fairy turn.
    public func grant() -> FairyToolGrant? {
        lock.lock()
        let active = listenFD >= 0 && !stopped
        lock.unlock()
        guard active else { return nil }

        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else {
            return nil
        }
        let token = bytes.map { String(format: "%02x", $0) }.joined()
        lock.lock()
        tokens.insert(token)
        lock.unlock()
        return FairyToolGrant(socket: socketPath, token: token)
    }

    /// Revoke a turn's token when the answer finishes or stops.
    public func revoke(_ token: String) {
        lock.lock()
        tokens.remove(token)
        lock.unlock()
    }

    private func isTokenValid(_ token: String) -> Bool {
        guard !token.isEmpty else { return false }
        lock.lock()
        defer { lock.unlock() }
        return tokens.contains(token)
    }

    private func currentHandler() -> ToolHandler? {
        lock.lock()
        defer { lock.unlock() }
        return handler
    }

    private func isStopped() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return stopped
    }

    private func acceptLoop(fd: Int32) {
        while !isStopped() {
            var pfd = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
            let ready = Darwin.poll(&pfd, 1, 250)
            if ready <= 0 { continue }
            let conn = Darwin.accept(fd, nil, nil)
            if conn < 0 { continue }
            Task.detached { [weak self] in
                await self?.handleConnection(conn)
            }
        }
    }

    private func handleConnection(_ conn: Int32) async {
        defer { Darwin.close(conn) }
        var noSigPipe: Int32 = 1
        setsockopt(conn, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
        var timeout = timeval(tv_sec: 30, tv_usec: 0)
        setsockopt(conn, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))

        guard let line = try? UnixSocketIO.readLine(conn) else { return }
        let rawObj = (try? JSONDecoder().decode([String: FairyJSON].self, from: line)) ?? [:]
        let reqId = rawObj["id"] ?? .null

        let response: FairyToolResponse
        let token = rawObj["token"]?.stringValue ?? ""
        let tool = rawObj["tool"]?.stringValue ?? ""
        let argsValue = rawObj["args"] ?? .object([:])

        if !isTokenValid(token) {
            response = FairyToolResponse(id: reqId, error: "That key isn't current: Graviton's tools work only while Fairy answers your request.")
        } else if !FairyApprovalPolicy.supportedTools.contains(tool) {
            response = FairyToolResponse(id: reqId, error: "Graviton has no tool called \"\(tool)\".")
        } else if case let .object(args) = argsValue {
            if let handler = currentHandler() {
                do {
                    let result = try await handler(tool, args)
                    response = FairyToolResponse(id: reqId, result: result)
                } catch {
                    response = FairyToolResponse(id: reqId, error: error.localizedDescription)
                }
            } else {
                response = FairyToolResponse(id: reqId, error: "Graviton's Fairy controller isn't ready.")
            }
        } else {
            response = FairyToolResponse(id: reqId, error: "Tool arguments must be a JSON object.")
        }

        if var out = try? JSONEncoder().encode(response) {
            out.append(UInt8(ascii: "\n"))
            try? UnixSocketIO.writeAll(conn, out)
        }
    }
}

// MARK: - Fairy Socket Client

private struct LinkEnvelope: Decodable {
    var type: String
    var name: String?
    var text: String?
    var message: String?
}

private struct AskPayload: Encodable {
    var type: String = "ask"
    var text: String
    var context: FairyAskContext
    var speak: Bool?
    var tools: FairyToolGrant?
}

/// Connects to Fairy over `graviton.sock` (on `chat-link.mjs` / `graviton-link.mjs`),
/// waking Fairy in the background if installed and not yet running.
public final class FairyLinkClient: @unchecked Sendable {
    public let socketPath: String
    private let lock = NSLock()
    private var activeFD: Int32 = -1

    public init(socketPath: String = FairyPaths.fairySocketPath) {
        self.socketPath = socketPath
    }

    public func probe() async -> (online: Bool, name: String?) {
        let path = socketPath
        return await Task.detached {
            guard let fd = try? UnixSocketIO.connect(path: path) else {
                return (false, nil)
            }
            defer { Darwin.close(fd) }
            var limit = timeval(tv_sec: 2, tv_usec: 0)
            setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &limit, socklen_t(MemoryLayout<timeval>.size))
            try? UnixSocketIO.writeAll(fd, Data("{\"type\":\"hello\"}\n".utf8))
            if let line = try? UnixSocketIO.readLine(fd),
               let env = try? JSONDecoder().decode(LinkEnvelope.self, from: line),
               env.type == "hello" {
                return (true, env.name)
            }
            return (true, nil)
        }.value
    }

    public func stop() {
        lock.lock()
        let fd = activeFD
        lock.unlock()
        guard fd >= 0 else { return }
        let payload = Data("{\"type\":\"stop\"}\n".utf8)
        Task.detached {
            try? UnixSocketIO.writeAll(fd, payload)
        }
    }

    public func reset() async throws {
        let path = socketPath
        try await Task.detached {
            let fd = try UnixSocketIO.connect(path: path)
            defer { Darwin.close(fd) }
            try UnixSocketIO.writeAll(fd, Data("{\"type\":\"new\"}\n".utf8))
            _ = try UnixSocketIO.readLine(fd)
        }.value
    }

    public func ask(
        text: String,
        context: FairyAskContext,
        fresh: Bool = false,
        speak: Bool = false,
        tools: FairyToolGrant? = nil,
        wakeIfNeeded: Bool = true
    ) -> AsyncThrowingStream<FairyLinkEvent, Error> {
        let path = socketPath
        return AsyncThrowingStream { continuation in
            let task = Task.detached { [weak self] in
                do {
                    let fd: Int32
                    if let existing = try? UnixSocketIO.connect(path: path) {
                        fd = existing
                    } else {
                        guard wakeIfNeeded, FairyPaths.isFairyInstalled else {
                            throw EngineError.unreachable("Fairy isn't installed or running on this Mac.")
                        }
                        continuation.yield(.waking)
                        let open = Process()
                        open.executableURL = URL(fileURLWithPath: "/usr/bin/open")
                        open.arguments = ["-g", "-b", FairyPaths.bundleID]
                        try? open.run()
                        open.waitUntilExit()

                        let deadline = Date().addingTimeInterval(20)
                        var openedFD: Int32?
                        while Date() < deadline {
                            try Task.checkCancellation()
                            if let s = try? UnixSocketIO.connect(path: path) {
                                openedFD = s
                                break
                            }
                            try await Task.sleep(for: .milliseconds(250))
                        }
                        guard let connected = openedFD else {
                            throw EngineError.unreachable("Fairy didn't start in time; open Fairy and try again.")
                        }
                        fd = connected
                    }

                    self?.setActiveFD(fd)
                    defer {
                        self?.clearActiveFD(fd)
                        Darwin.close(fd)
                    }

                    var limit = timeval(tv_sec: 600, tv_usec: 0)
                    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &limit, socklen_t(MemoryLayout<timeval>.size))

                    try UnixSocketIO.writeAll(fd, Data("{\"type\":\"hello\"}\n".utf8))
                    if fresh {
                        try UnixSocketIO.writeAll(fd, Data("{\"type\":\"new\"}\n".utf8))
                    }
                    var askData = try JSONEncoder().encode(AskPayload(
                        text: text,
                        context: context,
                        speak: speak ? true : nil,
                        tools: tools
                    ))
                    askData.append(UInt8(ascii: "\n"))
                    try UnixSocketIO.writeAll(fd, askData)

                    while let line = try UnixSocketIO.readLine(fd) {
                        try Task.checkCancellation()
                        guard let env = try? JSONDecoder().decode(LinkEnvelope.self, from: line) else { continue }
                        switch env.type {
                        case "hello":
                            continuation.yield(.hello(name: env.name ?? "Fairy"))
                        case "text":
                            continuation.yield(.text(env.text ?? ""))
                        case "done":
                            continuation.yield(.done(env.text ?? ""))
                            if !speak {
                                continuation.finish()
                                return
                            }
                        case "spoken":
                            continuation.yield(.spoken)
                            continuation.finish()
                            return
                        case "error":
                            throw EngineError.daemon(env.message ?? "Fairy couldn't answer.")
                        default:
                            break
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { [weak self] _ in
                self?.stop()
                task.cancel()
            }
        }
    }

    private func setActiveFD(_ fd: Int32) {
        lock.lock()
        activeFD = fd
        lock.unlock()
    }

    private func clearActiveFD(_ fd: Int32) {
        lock.lock()
        if activeFD == fd { activeFD = -1 }
        lock.unlock()
    }
}

#endif
