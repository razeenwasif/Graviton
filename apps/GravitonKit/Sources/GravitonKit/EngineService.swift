import Foundation

public enum EngineError: Error, LocalizedError, Equatable {
    case unreachable(String)
    case pathTooLong
    case io(String)
    case daemon(String)
    case unexpectedResponse

    public var errorDescription: String? {
        switch self {
        case let .unreachable(why): "Can't reach gravitond: \(why)"
        case .pathTooLong: "The gravitond socket path is too long."
        case let .io(why): "Connection error: \(why)"
        case let .daemon(message): message
        case .unexpectedResponse: "gravitond sent an unexpected response."
        }
    }
}

/// Everything the UI needs from an engine. `SocketEngine` talks to a real
/// gravitond; `DemoEngine` is an in-process stand-in for previews, iOS (until
/// remote hosts land) and when the daemon isn't running.
public protocol EngineService: Sendable {
    /// Cheap reachability check; returns the protocol version.
    func ping() async throws -> UInt32
    func engineInfo() async throws -> EngineInfo
    func containers() async throws -> [Container]
    func images() async throws -> [ImageInfo]
    func logs(id: String, tail: Int?) async throws -> [LogLine]
    func start(id: String) async throws -> Container
    func stop(id: String) async throws -> Container
    func restart(id: String) async throws -> Container
    /// Pull an image from its registry (may take minutes).
    func pull(reference: String) async throws -> ImageInfo
    /// Create a container and start it in its own VM; returns once it's running or has failed.
    func run(_ spec: RunSpec) async throws -> Container
    /// Remove a stopped container; returns its id.
    func remove(id: String) async throws -> String
    /// Where the disk space goes.
    func diskUsage() async throws -> DiskUsage
    /// Remove an image no container uses; returns the bytes freed.
    func removeImage(reference: String) async throws -> UInt64
    /// Clear the build cache and unreferenced layers, and more if asked.
    func prune(_ spec: PruneSpec) async throws -> PruneReport
    /// Recent CPU, memory and network samples and the processes of a running container.
    func stats(id: String) async throws -> ContainerStats
    func listFiles(id: String, path: String) async throws -> DirectoryListing
    /// The start of a file (up to 256 KiB).
    func readFile(id: String, path: String) async throws -> FileContents
    func inspect(id: String) async throws -> ContainerDetails
    /// Start a program on a terminal in a running container.
    func openTerminal(_ spec: ExecSpec) async throws -> any TerminalSession
    /// Recent events after `since` (oldest first), at most `limit`.
    func events(since: UInt64?, limit: Int?) async throws -> [EngineEvent]
}

// MARK: - Socket engine

/// Talks to gravitond over its Unix socket, one short-lived connection per call.
public final class SocketEngine: EngineService {
    public let path: String

    public init(path: String = Paths.socketPath) {
        self.path = path
    }

    public func ping() async throws -> UInt32 {
        guard case let .pong(version) = try await call(.ping) else { throw EngineError.unexpectedResponse }
        return version
    }

    public func engineInfo() async throws -> EngineInfo {
        guard case let .engine(info) = try await call(.engineInfo) else { throw EngineError.unexpectedResponse }
        return info
    }

    public func containers() async throws -> [Container] {
        guard case let .containers(list) = try await call(.listContainers) else { throw EngineError.unexpectedResponse }
        return list
    }

    public func images() async throws -> [ImageInfo] {
        guard case let .images(list) = try await call(.listImages) else { throw EngineError.unexpectedResponse }
        return list
    }

    public func logs(id: String, tail: Int?) async throws -> [LogLine] {
        guard case let .logs(lines) = try await call(.logs(id: id, tail: tail)) else { throw EngineError.unexpectedResponse }
        return lines
    }

    public func start(id: String) async throws -> Container { try await lifecycle(.start(id: id)) }
    public func stop(id: String) async throws -> Container { try await lifecycle(.stop(id: id)) }
    public func restart(id: String) async throws -> Container { try await lifecycle(.restart(id: id)) }

    public func pull(reference: String) async throws -> ImageInfo {
        guard case let .image(image) = try await call(.pull(reference: reference), timeout: 1800) else { throw EngineError.unexpectedResponse }
        return image
    }

    /// Pull an image, streaming per-layer progress before the final image.
    public func pullStream(reference: String) -> AsyncThrowingStream<PullEvent, Error> {
        let path = self.path
        return AsyncThrowingStream { continuation in
            let thread = Thread {
                do {
                    let fd = try Self.connect(path: path)
                    defer { close(fd) }
                    try Self.writeAll(fd, Array(try Wire.line(.pull(reference: reference, progress: true))))
                    var buffer = Data()
                    var chunk = [UInt8](repeating: 0, count: 64 * 1024)
                    while true {
                        while let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                            let line = buffer[buffer.startIndex..<nl]
                            buffer.removeSubrange(buffer.startIndex...nl)
                            switch try Wire.decoder.decode(EngineResponse.self, from: Data(line)) {
                            case let .pullProgress(progress): continuation.yield(.progress(progress))
                            case let .image(image):
                                continuation.yield(.pulled(image))
                                continuation.finish()
                                return
                            case let .error(message): throw EngineError.daemon(message)
                            default: throw EngineError.unexpectedResponse
                            }
                        }
                        let n = read(fd, &chunk, chunk.count)
                        guard n > 0 else { throw EngineError.io("gravitond closed the connection") }
                        buffer.append(contentsOf: chunk[0..<n])
                    }
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            thread.name = "graviton-pull"
            thread.start()
        }
    }

    public func run(_ spec: RunSpec) async throws -> Container {
        // Pulls the image first if it isn't local, then boots a VM.
        guard case let .container(c) = try await call(.run(spec), timeout: 1800) else { throw EngineError.unexpectedResponse }
        return c
    }

    public func remove(id: String) async throws -> String {
        guard case let .removed(removed) = try await call(.remove(id: id)) else { throw EngineError.unexpectedResponse }
        return removed
    }

    public func diskUsage() async throws -> DiskUsage {
        guard case let .diskUsage(u) = try await call(.diskUsage, timeout: 30) else { throw EngineError.unexpectedResponse }
        return u
    }

    public func removeImage(reference: String) async throws -> UInt64 {
        guard case let .imageRemoved(_, freed, _) = try await call(.removeImage(reference: reference), timeout: 60) else { throw EngineError.unexpectedResponse }
        return freed
    }

    public func prune(_ spec: PruneSpec) async throws -> PruneReport {
        guard case let .pruned(r) = try await call(.prune(spec), timeout: 300) else { throw EngineError.unexpectedResponse }
        return r
    }

    public func stats(id: String) async throws -> ContainerStats {
        guard case let .stats(stats) = try await call(.stats(id: id)) else { throw EngineError.unexpectedResponse }
        return stats
    }

    public func listFiles(id: String, path: String) async throws -> DirectoryListing {
        guard case let .files(listing) = try await call(.listFiles(id: id, path: path), timeout: 15) else { throw EngineError.unexpectedResponse }
        return listing
    }

    public func readFile(id: String, path: String) async throws -> FileContents {
        guard case let .file(contents) = try await call(.readFile(id: id, path: path), timeout: 15) else { throw EngineError.unexpectedResponse }
        return contents
    }

    /// Download a file or folder from a running container as a tar archive written to `archiveFile`.
    public func getArchive(id: String, path remotePath: String, to archiveFile: URL) async throws -> (isDir: Bool, name: String) {
        let socketPath = self.path
        return try await Task.detached {
            let fd = try Self.connect(path: socketPath)
            defer { close(fd) }
            try Self.writeAll(fd, Array(try Wire.line(.getArchive(id: id, path: remotePath))))

            var buffer = Data()
            var chunk = [UInt8](repeating: 0, count: 64 * 1024)
            var headerLine: Data?
            while headerLine == nil {
                if let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                    headerLine = Data(buffer[buffer.startIndex..<nl])
                    buffer.removeSubrange(buffer.startIndex...nl)
                    break
                }
                let n = read(fd, &chunk, chunk.count)
                guard n >= 0 else { throw EngineError.io(String(cString: strerror(errno))) }
                guard n > 0 else { throw EngineError.io("gravitond closed the connection") }
                buffer.append(contentsOf: chunk[0..<n])
            }

            let response = try Wire.decoder.decode(EngineResponse.self, from: headerLine!)
            let meta: (Bool, String)
            switch response {
            case let .archive(isDir, name): meta = (isDir, name)
            case let .error(message): throw EngineError.daemon(message)
            default: throw EngineError.unexpectedResponse
            }

            FileManager.default.createFile(atPath: archiveFile.path, contents: nil)
            let out = try FileHandle(forWritingTo: archiveFile)
            defer { try? out.close() }
            if !buffer.isEmpty {
                try out.write(contentsOf: buffer)
            }
            while true {
                let n = read(fd, &chunk, chunk.count)
                guard n >= 0 else { throw EngineError.io(String(cString: strerror(errno))) }
                if n == 0 { break }
                try out.write(contentsOf: chunk[0..<n])
            }
            return meta
        }.value
    }

    /// Upload a tar archive into directory `remotePath` inside a running container.
    public func putArchive(id: String, path remotePath: String, archiveFile: URL, size: UInt64) async throws -> UInt64 {
        let socketPath = self.path
        return try await Task.detached {
            let fd = try Self.connect(path: socketPath)
            defer { close(fd) }
            try Self.writeAll(fd, Array(try Wire.line(.putArchive(id: id, path: remotePath, size: size))))

            let source = try FileHandle(forReadingFrom: archiveFile)
            defer { try? source.close() }
            var sendError: Error?
            do {
                while let chunk = try source.read(upToCount: 1 << 20), !chunk.isEmpty {
                    try Self.writeAll(fd, Array(chunk))
                }
            } catch {
                sendError = error
            }

            var reply = Data()
            var buf = [UInt8](repeating: 0, count: 64 * 1024)
            while reply.last != UInt8(ascii: "\n") {
                let n = read(fd, &buf, buf.count)
                if n <= 0 { break }
                reply.append(contentsOf: buf[0..<n])
            }
            if !reply.isEmpty, let response = try? Wire.decoder.decode(EngineResponse.self, from: reply) {
                switch response {
                case let .copied(files):
                    if let sendError { throw sendError }
                    return files
                case let .error(message):
                    throw EngineError.daemon(message)
                default:
                    throw EngineError.unexpectedResponse
                }
            }
            if let sendError { throw sendError }
            throw EngineError.io("gravitond closed the connection")
        }.value
    }

    public func inspect(id: String) async throws -> ContainerDetails {
        guard case let .details(details) = try await call(.inspect(id: id)) else { throw EngineError.unexpectedResponse }
        return details
    }

    public func events(since: UInt64?, limit: Int?) async throws -> [EngineEvent] {
        guard case let .events(list) = try await call(.events(since: since, limit: limit)) else { throw EngineError.unexpectedResponse }
        return list
    }

    public func openTerminal(_ spec: ExecSpec) async throws -> any TerminalSession {
        let path = self.path
        return try await Task.detached { try SocketTerminal(path: path, spec: spec) }.value
    }

    /// Build an image from a Dockerfile; the stream carries the log, then the image.
    public func build(_ spec: BuildSpec) -> AsyncThrowingStream<BuildEvent, Error> {
        let path = self.path
        return AsyncThrowingStream { continuation in
            let thread = Thread {
                do {
                    let fd = try Self.connect(path: path)
                    defer { close(fd) }
                    try Self.writeAll(fd, Array(try Wire.line(.build(spec))))
                    var buffer = Data()
                    var chunk = [UInt8](repeating: 0, count: 64 * 1024)
                    while true {
                        while let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                            let line = buffer[buffer.startIndex..<nl]
                            buffer.removeSubrange(buffer.startIndex...nl)
                            switch try Wire.decoder.decode(EngineResponse.self, from: Data(line)) {
                            case let .buildLog(text): continuation.yield(.log(text))
                            case let .image(image):
                                continuation.yield(.built(image))
                                continuation.finish()
                                return
                            case let .error(message): throw EngineError.daemon(message)
                            default: throw EngineError.unexpectedResponse
                            }
                        }
                        let n = read(fd, &chunk, chunk.count)
                        guard n > 0 else { throw EngineError.io("gravitond closed the connection") }
                        buffer.append(contentsOf: chunk[0..<n])
                    }
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            thread.name = "graviton-build"
            thread.start()
        }
    }

    /// A one-time code for pairing a phone (gravitond must run with --remote).
    public func pairBegin() async throws -> PairingInfo {
        guard case let .pairing(info) = try await call(.pairBegin) else { throw EngineError.unexpectedResponse }
        return info
    }

    public func devices() async throws -> [PairedDevice] {
        guard case let .devices(list) = try await call(.listDevices) else { throw EngineError.unexpectedResponse }
        return list
    }

    public func revokeDevice(id: String) async throws {
        guard case .revoked = try await call(.revokeDevice(id: id)) else { throw EngineError.unexpectedResponse }
    }

    private func lifecycle(_ request: EngineRequest) async throws -> Container {
        // Stop waits up to ten seconds for SIGTERM before powering the VM off.
        guard case let .container(c) = try await call(request, timeout: 60) else { throw EngineError.unexpectedResponse }
        return c
    }

    func call(_ request: EngineRequest, timeout: Int = 5) async throws -> EngineResponse {
        let payload = try Wire.line(request)
        let path = self.path
        let reply = try await Task.detached { try Self.roundTrip(path: path, payload: payload, timeout: timeout) }.value
        let response = try Wire.decoder.decode(EngineResponse.self, from: reply)
        if case let .error(message) = response { throw EngineError.daemon(message) }
        return response
    }

    /// Blocking connect → write one line → read one line.
    static func roundTrip(path: String, payload: Data, timeout: Int) throws -> Data {
        let fd = try connect(path: path)
        defer { close(fd) }

        var limit = timeval(tv_sec: timeout, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &limit, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &limit, socklen_t(MemoryLayout<timeval>.size))
        try writeAll(fd, Array(payload))

        var reply = Data()
        var chunk = [UInt8](repeating: 0, count: 64 * 1024)
        while reply.last != UInt8(ascii: "\n") {
            let n = read(fd, &chunk, chunk.count)
            guard n >= 0 else { throw EngineError.io(String(cString: strerror(errno))) }
            if n == 0 { break }
            reply.append(contentsOf: chunk[0..<n])
        }
        guard !reply.isEmpty else { throw EngineError.io("gravitond closed the connection") }
        return reply
    }

    static func writeAll(_ fd: Int32, _ bytes: [UInt8]) throws {
        try bytes.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < buf.count {
                let n = write(fd, buf.baseAddress! + offset, buf.count - offset)
                guard n > 0 else { throw EngineError.io(String(cString: strerror(errno))) }
                offset += n
            }
        }
    }

    /// A connected Unix socket to gravitond (no timeouts; SIGPIPE off).
    static func connect(path: String) throws -> Int32 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw EngineError.io(String(cString: strerror(errno))) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let capacity = MemoryLayout.size(ofValue: addr.sun_path)
        guard path.utf8.count < capacity else {
            close(fd)
            throw EngineError.pathTooLong
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { raw in
            raw.withMemoryRebound(to: CChar.self, capacity: capacity) { _ = strncpy($0, path, capacity - 1) }
        }
        #if canImport(Darwin)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        #endif

        let connected = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connected == 0 else {
            let why = String(cString: strerror(errno))
            close(fd)
            throw EngineError.unreachable(why)
        }
        var noSigPipe: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
        return fd
    }
}

public enum Paths {
    /// Matches `graviton_core::paths::socket_path` in Rust.
    public static var socketPath: String {
        if let override = ProcessInfo.processInfo.environment["GRAVITON_SOCKET"], !override.isEmpty {
            return override
        }
        return NSHomeDirectory() + "/Library/Application Support/Graviton/gravitond.sock"
    }
}
