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

    public func run(_ spec: RunSpec) async throws -> Container {
        // Pulls the image first if it isn't local, then boots a VM.
        guard case let .container(c) = try await call(.run(spec), timeout: 1800) else { throw EngineError.unexpectedResponse }
        return c
    }

    public func remove(id: String) async throws -> String {
        guard case let .removed(removed) = try await call(.remove(id: id)) else { throw EngineError.unexpectedResponse }
        return removed
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
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw EngineError.io(String(cString: strerror(errno))) }
        defer { close(fd) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let capacity = MemoryLayout.size(ofValue: addr.sun_path)
        guard path.utf8.count < capacity else { throw EngineError.pathTooLong }
        withUnsafeMutablePointer(to: &addr.sun_path) { raw in
            raw.withMemoryRebound(to: CChar.self, capacity: capacity) { _ = strncpy($0, path, capacity - 1) }
        }
        #if canImport(Darwin)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        #endif

        let connected = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connected == 0 else { throw EngineError.unreachable(String(cString: strerror(errno))) }

        var limit = timeval(tv_sec: timeout, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &limit, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &limit, socklen_t(MemoryLayout<timeval>.size))
        #if canImport(Darwin)
        var noSigPipe: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
        #endif

        try payload.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < buf.count {
                let n = write(fd, buf.baseAddress! + offset, buf.count - offset)
                guard n > 0 else { throw EngineError.io(String(cString: strerror(errno))) }
                offset += n
            }
        }

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
