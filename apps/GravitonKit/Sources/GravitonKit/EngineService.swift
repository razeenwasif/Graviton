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
    func volumes() async throws -> [VolumeInfo]
    func createVolume(name: String) async throws -> VolumeInfo
    func removeVolume(name: String) async throws -> UInt64
    func networkInfo() async throws -> NetworkInfo
    func networks() async throws -> [UserNetwork]
    func createNetwork(name: String, internalOnly: Bool, labels: [String: String]) async throws -> UserNetwork
    func removeNetwork(name: String) async throws -> String
    func connectNetwork(network: String, container: String) async throws -> Bool
    func disconnectNetwork(network: String, container: String) async throws -> Bool
    func logs(id: String, tail: Int?, sinceMs: UInt64?, untilMs: UInt64?) async throws -> [LogLine]
    func start(id: String) async throws -> Container
    func stop(id: String) async throws -> Container
    func pause(id: String) async throws -> Container
    func unpause(id: String) async throws -> Container
    func restart(id: String) async throws -> Container
    /// Pull an image from its registry (may take minutes).
    func pull(reference: String) async throws -> ImageInfo
    /// Tag a local image with an additional name/reference.
    func tagImage(source: String, target: String) async throws -> ImageInfo
    /// Push a local image to its registry using Keychain credentials.
    func pushImage(reference: String) async throws -> ImagePushResult
    /// Saved OCI registry logins (usernames only; passwords stay in the Mac's Keychain).
    func listLogins() async throws -> [RegistryLoginInfo]
    /// Verify and store OCI registry credentials in the Mac's Keychain.
    func registryLogin(registry: String, username: String, password: String) async throws -> RegistryLoginInfo
    /// Forget saved OCI registry credentials.
    func registryLogout(registry: String) async throws -> String
    /// Snapshot a container's filesystem and configuration into a local image.
    func commit(_ spec: CommitSpec) async throws -> ImageInfo
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
    /// Run a non-interactive command inside a running container and capture its combined output.
    func execCommand(id: String, command: [String], env: [String], timeout: Int) async throws -> ExecResult
    /// Inspect cached VM snapshot slots.
    func snapshotStatus() async throws -> SnapshotInfo
    /// Pre-warm one or more VM snapshot slots.
    func snapshotWarm(cpus: UInt32?, memoryMib: UInt64?, rosetta: Bool, slots: UInt32?) async throws -> SnapshotInfo
    /// Remove idle cached VM snapshots from disk.
    func snapshotClear() async throws -> SnapshotInfo
    /// Recent events after `since` (oldest first), at most `limit`.
    func events(since: UInt64?, limit: Int?) async throws -> [EngineEvent]
}

public extension EngineService {
    func logs(id: String, tail: Int?) async throws -> [LogLine] {
        try await logs(id: id, tail: tail, sinceMs: nil, untilMs: nil)
    }
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

    public func volumes() async throws -> [VolumeInfo] {
        guard case let .volumes(list) = try await call(.listVolumes, timeout: 30) else { throw EngineError.unexpectedResponse }
        return list
    }

    public func createVolume(name: String) async throws -> VolumeInfo {
        guard case let .volume(vol) = try await call(.createVolume(name: name)) else { throw EngineError.unexpectedResponse }
        return vol
    }

    public func removeVolume(name: String) async throws -> UInt64 {
        guard case let .volumeRemoved(_, freed) = try await call(.removeVolume(name: name), timeout: 60) else { throw EngineError.unexpectedResponse }
        return freed
    }

    public func networkInfo() async throws -> NetworkInfo {
        guard case let .network(info) = try await call(.networkInfo) else { throw EngineError.unexpectedResponse }
        return info
    }

    public func networks() async throws -> [UserNetwork] {
        guard case let .networks(list) = try await call(.listNetworks) else { throw EngineError.unexpectedResponse }
        return list
    }

    public func createNetwork(name: String, internalOnly: Bool = false, labels: [String: String] = [:]) async throws -> UserNetwork {
        guard case let .networkCreated(net) = try await call(.createNetwork(name: name, internal: internalOnly, labels: labels)) else {
            throw EngineError.unexpectedResponse
        }
        return net
    }

    public func removeNetwork(name: String) async throws -> String {
        guard case let .networkRemoved(removed) = try await call(.removeNetwork(name: name)) else {
            throw EngineError.unexpectedResponse
        }
        return removed
    }

    public func connectNetwork(network: String, container: String) async throws -> Bool {
        guard case let .networkUpdated(_, _, connected) = try await call(.connectNetwork(network: network, container: container)) else {
            throw EngineError.unexpectedResponse
        }
        return connected
    }

    public func disconnectNetwork(network: String, container: String) async throws -> Bool {
        guard case let .networkUpdated(_, _, connected) = try await call(.disconnectNetwork(network: network, container: container)) else {
            throw EngineError.unexpectedResponse
        }
        return connected
    }

    public func logs(id: String, tail: Int?, sinceMs: UInt64? = nil, untilMs: UInt64? = nil) async throws -> [LogLine] {
        guard case let .logs(lines) = try await call(.logs(id: id, tail: tail, follow: false, sinceMs: sinceMs, untilMs: untilMs)) else {
            throw EngineError.unexpectedResponse
        }
        return lines
    }

    private final class CancelableFD: @unchecked Sendable {
        private let lock = NSLock()
        private var fd: Int32?
        private(set) var isCancelled = false

        func attach(_ fd: Int32) -> Bool {
            lock.lock()
            defer { lock.unlock() }
            if isCancelled { return false }
            self.fd = fd
            return true
        }

        func detach() -> Int32? {
            lock.lock()
            defer { lock.unlock() }
            let current = fd
            fd = nil
            return current
        }

        func cancel() {
            lock.lock()
            isCancelled = true
            let current = fd
            lock.unlock()
            if let current {
                _ = Darwin.shutdown(current, SHUT_RDWR)
            }
        }
    }

    /// Stream container logs in real time (`follow: true`), emitting matching history then live lines until exit or cancellation.
    public func logsStream(id: String, tail: Int? = nil, sinceMs: UInt64? = nil, untilMs: UInt64? = nil) -> AsyncThrowingStream<LogLine, Error> {
        let path = self.path
        let box = CancelableFD()
        return AsyncThrowingStream { continuation in
            continuation.onTermination = { _ in box.cancel() }
            let thread = Thread {
                var ownedFD: Int32?
                defer {
                    _ = box.detach()
                    if let fd = ownedFD { close(fd) }
                }
                do {
                    let fd = try Self.connect(path: path)
                    ownedFD = fd
                    guard box.attach(fd) else {
                        continuation.finish()
                        return
                    }
                    try Self.writeAll(fd, Array(try Wire.line(.logs(id: id, tail: tail, follow: true, sinceMs: sinceMs, untilMs: untilMs))))
                    var buffer = Data()
                    var chunk = [UInt8](repeating: 0, count: 64 * 1024)
                    while true {
                        if box.isCancelled {
                            continuation.finish()
                            return
                        }
                        var pfd = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
                        let pr = Darwin.poll(&pfd, 1, 250)
                        if pr == 0 { continue }
                        if pr < 0 {
                            if errno == EINTR { continue }
                            if box.isCancelled { continuation.finish(); return }
                            throw EngineError.io(String(cString: strerror(errno)))
                        }
                        let n = read(fd, &chunk, chunk.count)
                        if n < 0 {
                            if errno == EINTR || errno == EAGAIN { continue }
                            if box.isCancelled { continuation.finish(); return }
                            throw EngineError.io(String(cString: strerror(errno)))
                        }
                        if n == 0 {
                            continuation.finish()
                            return
                        }
                        buffer.append(contentsOf: chunk[0..<n])
                        while let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                            let line = Data(buffer[buffer.startIndex..<nl])
                            buffer.removeSubrange(buffer.startIndex...nl)
                            if line.isEmpty { continue }
                            switch try Wire.decoder.decode(EngineResponse.self, from: line) {
                            case let .log(entry):
                                continuation.yield(entry)
                            case .logEnd:
                                continuation.finish()
                                return
                            case let .error(message):
                                throw EngineError.daemon(message)
                            default:
                                throw EngineError.unexpectedResponse
                            }
                        }
                    }
                } catch {
                    if box.isCancelled {
                        continuation.finish()
                    } else {
                        continuation.finish(throwing: error)
                    }
                }
            }
            thread.name = "graviton-logs"
            thread.start()
        }
    }

    public func start(id: String) async throws -> Container { try await lifecycle(.start(id: id)) }
    public func stop(id: String) async throws -> Container { try await lifecycle(.stop(id: id)) }
    public func pause(id: String) async throws -> Container { try await lifecycle(.pause(id: id)) }
    public func unpause(id: String) async throws -> Container { try await lifecycle(.unpause(id: id)) }
    public func restart(id: String) async throws -> Container { try await lifecycle(.restart(id: id)) }

    public func pull(reference: String) async throws -> ImageInfo {
        guard case let .image(image) = try await call(.pull(reference: reference), timeout: 1800) else { throw EngineError.unexpectedResponse }
        return image
    }

    public func tagImage(source: String, target: String) async throws -> ImageInfo {
        guard case let .image(image) = try await call(.tagImage(source: source, target: target)) else { throw EngineError.unexpectedResponse }
        return image
    }

    public func pushImage(reference: String) async throws -> ImagePushResult {
        guard case let .imagePushed(res) = try await call(.pushImage(reference: reference), timeout: 1800) else { throw EngineError.unexpectedResponse }
        return res
    }

    public func commit(_ spec: CommitSpec) async throws -> ImageInfo {
        guard case let .image(image) = try await call(.commit(spec), timeout: 600) else { throw EngineError.unexpectedResponse }
        return image
    }

    /// Export one or more local images into a Docker/OCI `.tar` archive at `archiveFile`.
    public func saveImage(references: [String], to archiveFile: URL) async throws {
        _ = try await receiveArchive(request: .saveImage(references: references), to: archiveFile) { response in
            guard case .imageArchive = response else { throw EngineError.unexpectedResponse }
        }
    }

    /// Import one or more images from a `.tar` or `.tar.gz` archive at `archiveFile`.
    public func loadImage(from archiveFile: URL, size: UInt64) async throws -> [ImageInfo] {
        try await sendArchive(request: .loadImage(size: size), from: archiveFile) { response in
            guard case let .images(list) = response else { throw EngineError.unexpectedResponse }
            return list
        }
    }

    /// Pull an image, streaming per-layer progress before the final image.
    public func pullStream(reference: String, platform: String? = nil) -> AsyncThrowingStream<PullEvent, Error> {
        let path = self.path
        return AsyncThrowingStream { continuation in
            let thread = Thread {
                do {
                    let fd = try Self.connect(path: path)
                    defer { close(fd) }
                    try Self.writeAll(fd, Array(try Wire.line(.pull(reference: reference, progress: true, platform: platform))))
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
        try await receiveArchive(request: .getArchive(id: id, path: remotePath), to: archiveFile) { response in
            guard case let .archive(isDir, name) = response else { throw EngineError.unexpectedResponse }
            return (isDir, name)
        }
    }

    /// Upload a tar archive into directory `remotePath` inside a running container.
    public func putArchive(id: String, path remotePath: String, archiveFile: URL, size: UInt64) async throws -> UInt64 {
        try await sendArchive(request: .putArchive(id: id, path: remotePath, size: size), from: archiveFile) { response in
            guard case let .copied(files) = response else { throw EngineError.unexpectedResponse }
            return files
        }
    }

    private func receiveArchive<T: Sendable>(
        request: EngineRequest,
        to archiveFile: URL,
        parseHeader: @escaping @Sendable (EngineResponse) throws -> T
    ) async throws -> T {
        let socketPath = self.path
        return try await Task.detached {
            let fd = try Self.connect(path: socketPath)
            defer { close(fd) }
            try Self.writeAll(fd, Array(try Wire.line(request)))

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
            if case let .error(message) = response { throw EngineError.daemon(message) }
            let meta = try parseHeader(response)

            FileManager.default.createFile(atPath: archiveFile.path, contents: nil)
            do {
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
            } catch {
                try? FileManager.default.removeItem(at: archiveFile)
                throw error
            }
            return meta
        }.value
    }

    private func sendArchive<T: Sendable>(
        request: EngineRequest,
        from archiveFile: URL,
        parseResponse: @escaping @Sendable (EngineResponse) throws -> T
    ) async throws -> T {
        let socketPath = self.path
        return try await Task.detached {
            let fd = try Self.connect(path: socketPath)
            defer { close(fd) }
            try Self.writeAll(fd, Array(try Wire.line(request)))

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
                if case let .error(message) = response {
                    throw EngineError.daemon(message)
                }
                if let sendError { throw sendError }
                return try parseResponse(response)
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

    public func execCommand(id: String, command: [String], env: [String] = [], timeout: Int = 120) async throws -> ExecResult {
        let path = self.path
        let spec = ExecSpec(id: id, command: command, tty: false, rows: 0, cols: 0, env: env)
        return try await Task.detached {
            let fd = try Self.connect(path: path)
            defer { close(fd) }
            var limit = timeval(tv_sec: timeout, tv_usec: 0)
            setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &limit, socklen_t(MemoryLayout<timeval>.size))
            setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &limit, socklen_t(MemoryLayout<timeval>.size))
            try Self.writeAll(fd, Array(try Wire.line(.exec(spec))))

            var buffer = Data()
            var chunk = [UInt8](repeating: 0, count: 64 * 1024)
            var out = Data()
            let maxBytes = 128 * 1024
            var truncated = false
            while true {
                while let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                    let line = Data(buffer[buffer.startIndex..<nl])
                    buffer.removeSubrange(buffer.startIndex...nl)
                    switch try Wire.decoder.decode(EngineResponse.self, from: line) {
                    case let .execOutput(_, bytes):
                        if out.count < maxBytes {
                            let take = min(bytes.count, maxBytes - out.count)
                            out.append(contentsOf: bytes[..<take])
                            if take < bytes.count { truncated = true }
                        } else {
                            truncated = true
                        }
                    case let .execExited(code):
                        let text = String(decoding: out, as: UTF8.self)
                        return ExecResult(exitCode: code, output: text, truncated: truncated)
                    case let .error(message):
                        throw EngineError.daemon(message)
                    default:
                        throw EngineError.unexpectedResponse
                    }
                }
                let n = read(fd, &chunk, chunk.count)
                if n < 0 { throw EngineError.io(String(cString: strerror(errno))) }
                if n == 0 { throw EngineError.io("gravitond closed the connection") }
                buffer.append(contentsOf: chunk[0..<n])
            }
        }.value
    }

    public func snapshotStatus() async throws -> SnapshotInfo {
        guard case let .snapshots(info) = try await call(.snapshotStatus, timeout: 15) else { throw EngineError.unexpectedResponse }
        return info
    }

    public func snapshotWarm(cpus: UInt32? = nil, memoryMib: UInt64? = nil, rosetta: Bool = false, slots: UInt32? = nil) async throws -> SnapshotInfo {
        guard case let .snapshots(info) = try await call(.snapshotWarm(cpus: cpus, memoryMib: memoryMib, rosetta: rosetta, slots: slots), timeout: 120) else {
            throw EngineError.unexpectedResponse
        }
        return info
    }

    public func snapshotClear() async throws -> SnapshotInfo {
        guard case let .snapshots(info) = try await call(.snapshotClear, timeout: 30) else { throw EngineError.unexpectedResponse }
        return info
    }

    public func listLogins() async throws -> [RegistryLoginInfo] {
        guard case let .logins(list) = try await call(.listLogins, timeout: 10) else { throw EngineError.unexpectedResponse }
        return list
    }

    public func registryLogin(registry: String, username: String, password: String) async throws -> RegistryLoginInfo {
        guard case let .loggedIn(canonical, user) = try await call(
            .registryLogin(registry: registry, username: username, password: password),
            timeout: 30
        ) else { throw EngineError.unexpectedResponse }
        return RegistryLoginInfo(registry: canonical, username: user)
    }

    public func registryLogout(registry: String) async throws -> String {
        guard case let .loggedOut(canonical) = try await call(.registryLogout(registry: registry), timeout: 15) else {
            throw EngineError.unexpectedResponse
        }
        return canonical
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
