import CryptoKit
import Foundation
import Network

// MARK: - Hosts

/// A Mac running gravitond with `--remote`, as this device knows it.
public struct RemoteHost: Codable, Hashable, Sendable, Identifiable {
    /// The id gravitond gave this device when it paired.
    public var deviceID: String
    /// The Mac's name.
    public var name: String
    /// Bonjour service name, when it was found on the network. Preferred: survives address changes.
    public var serviceName: String?
    /// Address and port, from a pairing link or typed in.
    public var address: String?
    public var port: UInt16
    /// SHA-256 of the endpoint's certificate (lowercase hex). Every connection checks it.
    public var fingerprint: String

    public var id: String { deviceID }

    public init(deviceID: String, name: String, serviceName: String?, address: String?, port: UInt16, fingerprint: String) {
        self.deviceID = deviceID
        self.name = name
        self.serviceName = serviceName
        self.address = address
        self.port = port
        self.fingerprint = fingerprint
    }

    /// "F004 18F9 00F8 BA56", as `grav pair` prints it.
    public var shortFingerprint: String { Self.short(fingerprint) }

    public static func short(_ fingerprint: String) -> String {
        stride(from: 0, to: min(16, fingerprint.count), by: 4).map { i -> String in
            let start = fingerprint.index(fingerprint.startIndex, offsetBy: i)
            return String(fingerprint[start..<fingerprint.index(start, offsetBy: 4)]).uppercased()
        }.joined(separator: " ")
    }

    var endpoints: [NWEndpoint] {
        var list: [NWEndpoint] = []
        if let serviceName {
            list.append(.service(name: serviceName, type: RemoteHost.bonjourType, domain: "local.", interface: nil))
        }
        if let address, let port = NWEndpoint.Port(rawValue: port) {
            list.append(.hostPort(host: NWEndpoint.Host(address), port: port))
        }
        return list
    }

    public static let bonjourType = "_graviton._tcp"
    public static let defaultPort: UInt16 = 7843
}

/// Where to pair: from Bonjour, a `graviton://pair` link, or typed in.
public struct PairingTarget: Hashable, Sendable {
    public var name: String
    public var serviceName: String?
    public var address: String?
    public var port: UInt16
    /// From a link: the fingerprint to insist on. Otherwise the phone shows what it saw.
    public var fingerprint: String?
    /// From a link.
    public var code: String?

    public init(name: String, serviceName: String? = nil, address: String? = nil, port: UInt16 = RemoteHost.defaultPort,
                fingerprint: String? = nil, code: String? = nil) {
        self.name = name
        self.serviceName = serviceName
        self.address = address
        self.port = port
        self.fingerprint = fingerprint
        self.code = code
    }

    /// Parse `graviton://pair?host=…&port=…&fp=…&code=…&name=…` (what `grav pair` prints and its QR code holds).
    public init?(link: String) {
        guard let url = URL(string: link.trimmingCharacters(in: .whitespacesAndNewlines)),
              url.scheme == "graviton", url.host == "pair",
              let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems else { return nil }
        let value = { (key: String) in items.first { $0.name == key }?.value }
        guard let host = value("host"), !host.isEmpty else { return nil }
        let fp = value("fp")?.lowercased()
        if let fp, fp.count != 64 || !fp.allSatisfy(\.isHexDigit) { return nil }
        self.init(name: value("name") ?? host, address: host, port: value("port").flatMap(UInt16.init) ?? RemoteHost.defaultPort,
                  fingerprint: fp, code: value("code"))
    }

    var endpoints: [NWEndpoint] {
        RemoteHost(deviceID: "", name: name, serviceName: serviceName, address: address, port: port, fingerprint: "").endpoints
    }
}

// MARK: - Connection

/// One TLS connection carrying JSON lines, with the server certificate pinned.
final class LineConnection: @unchecked Sendable {
    private let connection: NWConnection
    private let queue = DispatchQueue(label: "dev.graviton.remote")
    private var buffer = Data()
    private var eof = false
    /// The certificate fingerprint seen during the handshake.
    private(set) var observedFingerprint: String?

    /// - Parameter pin: required fingerprint; `nil` accepts any certificate and records it (first pairing).
    init(endpoint: NWEndpoint, pin: String?) {
        let tls = NWProtocolTLS.Options()
        let observed = Observed()
        sec_protocol_options_set_verify_block(tls.securityProtocolOptions, { _, trust, complete in
            let secTrust = sec_trust_copy_ref(trust).takeRetainedValue()
            guard let chain = SecTrustCopyCertificateChain(secTrust) as? [SecCertificate], let leaf = chain.first else {
                complete(false)
                return
            }
            let der = SecCertificateCopyData(leaf) as Data
            let fingerprint = SHA256.hash(data: der).map { String(format: "%02x", $0) }.joined()
            observed.value = fingerprint
            complete(pin == nil || pin == fingerprint)
        }, queue)
        let tcp = NWProtocolTCP.Options()
        tcp.connectionTimeout = 6
        connection = NWConnection(to: endpoint, using: NWParameters(tls: tls, tcp: tcp))
        self.observed = observed
    }

    private let observed: Observed
    private final class Observed: @unchecked Sendable { var value: String? }

    func start() async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let once = Once()
            connection.stateUpdateHandler = { [observed = self.observed] state in
                switch state {
                case .ready:
                    once.run { continuation.resume() }
                case let .failed(error):
                    let pinned = observed.value != nil
                    once.run { continuation.resume(throwing: pinned ? EngineError.unreachable("the Mac's certificate didn't match the one paired (\(error))") : EngineError.unreachable(error.localizedDescription)) }
                case let .waiting(error):
                    once.run { continuation.resume(throwing: EngineError.unreachable(error.localizedDescription)) }
                case .cancelled:
                    once.run { continuation.resume(throwing: EngineError.unreachable("cancelled")) }
                default:
                    break
                }
            }
            connection.start(queue: queue)
        }
        observedFingerprint = observed.value
    }

    func send(_ data: Data) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            connection.send(content: data, completion: .contentProcessed { error in
                if let error { continuation.resume(throwing: EngineError.io(error.localizedDescription)) } else { continuation.resume() }
            })
        }
    }

    func readLine() async throws -> Data {
        while true {
            if let newline = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                let line = buffer[buffer.startIndex..<newline]
                buffer.removeSubrange(buffer.startIndex...newline)
                return Data(line)
            }
            if eof { throw EngineError.io("the Mac closed the connection") }
            let (data, isComplete): (Data?, Bool) = try await withCheckedThrowingContinuation { continuation in
                connection.receive(minimumIncompleteLength: 1, maximumLength: 256 * 1024) { data, _, isComplete, error in
                    if let error { continuation.resume(throwing: EngineError.io(error.localizedDescription)) }
                    else { continuation.resume(returning: (data, isComplete)) }
                }
            }
            if isComplete { eof = true }
            if let data, !data.isEmpty {
                buffer.append(data)
            } else if isComplete {
                throw EngineError.io("the Mac closed the connection")
            }
        }
    }

    /// The next bytes from the Mac, starting with anything `readLine` read past.
    func receive() async throws -> Data {
        guard let chunk = try await receiveOrEOF() else {
            throw EngineError.io("the Mac closed the connection")
        }
        return chunk
    }

    /// The next bytes from the Mac, starting with anything `readLine` read past, or `nil` on clean EOF.
    func receiveOrEOF() async throws -> Data? {
        if !buffer.isEmpty {
            defer { buffer = Data() }
            return buffer
        }
        if eof { return nil }
        while true {
            let (data, isComplete): (Data?, Bool) = try await withCheckedThrowingContinuation { continuation in
                connection.receive(minimumIncompleteLength: 1, maximumLength: 256 * 1024) { data, _, isComplete, error in
                    if let error { continuation.resume(throwing: EngineError.io(error.localizedDescription)) }
                    else { continuation.resume(returning: (data, isComplete)) }
                }
            }
            if isComplete { eof = true }
            if let data, !data.isEmpty { return data }
            if isComplete { return nil }
        }
    }

    func call(_ request: EngineRequest) async throws -> EngineResponse {
        try await send(Wire.line(request))
        let response = try Wire.decoder.decode(EngineResponse.self, from: try await readLine())
        if case let .error(message) = response { throw EngineError.daemon(message) }
        return response
    }

    func cancel() {
        connection.cancel()
    }

    /// Connect to the first endpoint that answers.
    static func open(_ endpoints: [NWEndpoint], pin: String?) async throws -> LineConnection {
        var lastError: Error = EngineError.unreachable("no address for this Mac")
        for endpoint in endpoints {
            let c = LineConnection(endpoint: endpoint, pin: pin)
            do {
                try await c.start()
                return c
            } catch {
                c.cancel()
                lastError = error
            }
        }
        throw lastError
    }
}

private final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false
    func run(_ body: () -> Void) {
        lock.lock()
        defer { lock.unlock() }
        guard !done else { return }
        done = true
        body()
    }
}

// MARK: - Pairing

public struct PairingResult: Sendable {
    public var host: RemoteHost
    public var token: String
}

public enum Pairing {
    /// Trade a one-time code for a token. With no fingerprint in `target`, the
    /// certificate seen is trusted and returned so the user can compare it.
    public static func pair(_ target: PairingTarget, code: String, deviceName: String) async throws -> PairingResult {
        let connection = try await LineConnection.open(target.endpoints, pin: target.fingerprint)
        defer { connection.cancel() }
        guard case let .paired(token, deviceID, hostName) = try await connection.call(.pair(code: code, device: deviceName)) else {
            throw EngineError.unexpectedResponse
        }
        guard let fingerprint = connection.observedFingerprint else { throw EngineError.unexpectedResponse }
        let host = RemoteHost(deviceID: deviceID, name: hostName, serviceName: target.serviceName, address: target.address,
                              port: target.port, fingerprint: fingerprint)
        return PairingResult(host: host, token: token)
    }
}

// MARK: - Engine

/// Talks to a paired Mac's gravitond over TLS. Requests are sent one at a
/// time on one connection, which reconnects (and re-authenticates) as needed.
public actor RemoteEngine: EngineService {
    public nonisolated let host: RemoteHost
    private let token: String
    private var connection: LineConnection?
    private var busy = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    public init(host: RemoteHost, token: String) {
        self.host = host
        self.token = token
    }

    deinit {
        connection?.cancel()
    }

    private func acquire() async {
        if !busy {
            busy = true
            return
        }
        await withCheckedContinuation { waiters.append($0) }
    }

    private func release() {
        if waiters.isEmpty { busy = false } else { waiters.removeFirst().resume() }
    }

    private func connected() async throws -> LineConnection {
        if let connection { return connection }
        let c = try await LineConnection.open(host.endpoints, pin: host.fingerprint)
        do {
            guard case .authenticated = try await c.call(.auth(token: token)) else { throw EngineError.unexpectedResponse }
        } catch {
            c.cancel()
            throw error
        }
        connection = c
        return c
    }

    func call(_ request: EngineRequest) async throws -> EngineResponse {
        await acquire()
        defer { release() }
        do {
            return try await connected().call(request)
        } catch EngineError.daemon(let message) {
            throw EngineError.daemon(message) // the connection is fine
        } catch {
            connection?.cancel()
            connection = nil
            throw error
        }
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
        guard case let .volumes(list) = try await call(.listVolumes) else { throw EngineError.unexpectedResponse }
        return list
    }

    public func createVolume(name: String) async throws -> VolumeInfo {
        guard case let .volume(vol) = try await call(.createVolume(name: name)) else { throw EngineError.unexpectedResponse }
        return vol
    }

    public func removeVolume(name: String) async throws -> UInt64 {
        guard case let .volumeRemoved(_, freed) = try await call(.removeVolume(name: name)) else { throw EngineError.unexpectedResponse }
        return freed
    }

    public func networkInfo() async throws -> NetworkInfo {
        guard case let .network(info) = try await call(.networkInfo) else { throw EngineError.unexpectedResponse }
        return info
    }

    public func logs(id: String, tail: Int?) async throws -> [LogLine] {
        guard case let .logs(lines) = try await call(.logs(id: id, tail: tail)) else { throw EngineError.unexpectedResponse }
        return lines
    }

    public func start(id: String) async throws -> Container { try await container(.start(id: id)) }
    public func stop(id: String) async throws -> Container { try await container(.stop(id: id)) }
    public func restart(id: String) async throws -> Container { try await container(.restart(id: id)) }
    /// Runs and pulls can take minutes (a pull, a VM boot): they get a connection
    /// of their own so the shared one keeps answering the list and logs meanwhile.
    public func run(_ spec: RunSpec) async throws -> Container {
        guard case let .container(c) = try await separately(.run(spec)) else { throw EngineError.unexpectedResponse }
        return c
    }

    public func pull(reference: String) async throws -> ImageInfo {
        guard case let .image(image) = try await separately(.pull(reference: reference)) else { throw EngineError.unexpectedResponse }
        return image
    }

    /// Pull an image on the paired Mac, streaming per-layer progress before the final image.
    public nonisolated func pullStream(reference: String, platform: String? = nil) -> AsyncThrowingStream<PullEvent, Error> {
        let host = self.host, token = self.token
        return AsyncThrowingStream { continuation in
            let task = Task.detached {
                do {
                    let c = try await LineConnection.open(host.endpoints, pin: host.fingerprint)
                    defer { c.cancel() }
                    guard case .authenticated = try await c.call(.auth(token: token)) else { throw EngineError.unexpectedResponse }
                    try await c.send(Wire.line(.pull(reference: reference, progress: true, platform: platform)))
                    while true {
                        try Task.checkCancellation()
                        switch try Wire.decoder.decode(EngineResponse.self, from: try await c.readLine()) {
                        case let .pullProgress(progress): continuation.yield(.progress(progress))
                        case let .image(image):
                            continuation.yield(.pulled(image))
                            continuation.finish()
                            return
                        case let .error(message): throw EngineError.daemon(message)
                        default: throw EngineError.unexpectedResponse
                        }
                    }
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    private nonisolated func separately(_ request: EngineRequest) async throws -> EngineResponse {
        let c = try await LineConnection.open(host.endpoints, pin: host.fingerprint)
        defer { c.cancel() }
        guard case .authenticated = try await c.call(.auth(token: token)) else { throw EngineError.unexpectedResponse }
        return try await c.call(request)
    }

    public func remove(id: String) async throws -> String {
        guard case let .removed(removed) = try await call(.remove(id: id)) else { throw EngineError.unexpectedResponse }
        return removed
    }

    public func diskUsage() async throws -> DiskUsage {
        guard case let .diskUsage(u) = try await call(.diskUsage) else { throw EngineError.unexpectedResponse }
        return u
    }

    public func removeImage(reference: String) async throws -> UInt64 {
        guard case let .imageRemoved(_, freed, _) = try await call(.removeImage(reference: reference)) else { throw EngineError.unexpectedResponse }
        return freed
    }

    public func prune(_ spec: PruneSpec) async throws -> PruneReport {
        guard case let .pruned(r) = try await call(.prune(spec)) else { throw EngineError.unexpectedResponse }
        return r
    }

    public func stats(id: String) async throws -> ContainerStats {
        guard case let .stats(stats) = try await call(.stats(id: id)) else { throw EngineError.unexpectedResponse }
        return stats
    }

    public func listFiles(id: String, path: String) async throws -> DirectoryListing {
        guard case let .files(listing) = try await call(.listFiles(id: id, path: path)) else { throw EngineError.unexpectedResponse }
        return listing
    }

    public func readFile(id: String, path: String) async throws -> FileContents {
        guard case let .file(contents) = try await call(.readFile(id: id, path: path)) else { throw EngineError.unexpectedResponse }
        return contents
    }

    /// Download a file or folder from a container on the paired Mac as a tar archive.
    public nonisolated func getArchive(id: String, path: String, to archiveFile: URL) async throws -> (isDir: Bool, name: String) {
        let c = try await LineConnection.open(host.endpoints, pin: host.fingerprint)
        defer { c.cancel() }
        guard case .authenticated = try await c.call(.auth(token: token)) else { throw EngineError.unexpectedResponse }
        try await c.send(Wire.line(.getArchive(id: id, path: path)))
        let response = try Wire.decoder.decode(EngineResponse.self, from: try await c.readLine())
        let meta: (Bool, String)
        switch response {
        case let .archive(isDir, name): meta = (isDir, name)
        case let .error(message): throw EngineError.daemon(message)
        default: throw EngineError.unexpectedResponse
        }
        FileManager.default.createFile(atPath: archiveFile.path, contents: nil)
        let out = try FileHandle(forWritingTo: archiveFile)
        defer { try? out.close() }
        while let chunk = try await c.receiveOrEOF() {
            try Task.checkCancellation()
            try out.write(contentsOf: chunk)
        }
        return meta
    }

    /// Upload a tar archive into a container directory on the paired Mac.
    public nonisolated func putArchive(id: String, path: String, archiveFile: URL, size: UInt64) async throws -> UInt64 {
        let c = try await LineConnection.open(host.endpoints, pin: host.fingerprint)
        defer { c.cancel() }
        guard case .authenticated = try await c.call(.auth(token: token)) else { throw EngineError.unexpectedResponse }
        try await c.send(Wire.line(.putArchive(id: id, path: path, size: size)))

        let source = try FileHandle(forReadingFrom: archiveFile)
        defer { try? source.close() }
        var sendError: Error?
        do {
            while let chunk = try source.read(upToCount: 1 << 20), !chunk.isEmpty {
                try Task.checkCancellation()
                try await c.send(chunk)
            }
        } catch {
            sendError = error
        }

        if let line = try? await c.readLine(),
           let response = try? Wire.decoder.decode(EngineResponse.self, from: line) {
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
        throw EngineError.io("the Mac closed the connection")
    }

    public func inspect(id: String) async throws -> ContainerDetails {
        guard case let .details(details) = try await call(.inspect(id: id)) else { throw EngineError.unexpectedResponse }
        return details
    }

    public func events(since: UInt64?, limit: Int?) async throws -> [EngineEvent] {
        guard case let .events(list) = try await call(.events(since: since, limit: limit)) else { throw EngineError.unexpectedResponse }
        return list
    }

    /// Ask this Mac to send push alerts to this device.
    public func registerPush(token: String, sandbox: Bool) async throws -> PushInfo {
        guard case let .push(info) = try await call(.registerPush(token: token, sandbox: sandbox)) else { throw EngineError.unexpectedResponse }
        return info
    }

    /// Builds send the folder over a connection of their own: the log streams back.
    public nonisolated func build(_ spec: BuildSpec) -> AsyncThrowingStream<BuildEvent, Error> {
        let host = self.host, token = self.token
        return AsyncThrowingStream { continuation in
            let task = Task.detached {
                var archive: URL?
                defer { if let archive { try? FileManager.default.removeItem(at: archive) } }
                do {
                    var spec = spec
                    let folder = URL(fileURLWithPath: spec.context)
                    let (file, size) = try BuildUpload.pack(folder, dockerfile: spec.dockerfile)
                    archive = file
                    if spec.tag == nil { spec.tag = BuildSpec.defaultName(for: spec.context) }
                    spec.upload = size
                    continuation.yield(.log("Sending \(folder.lastPathComponent) to \(host.name) (\(Format.bytes(size)))…"))

                    let c = try await LineConnection.open(host.endpoints, pin: host.fingerprint)
                    defer { c.cancel() }
                    guard case .authenticated = try await c.call(.auth(token: token)) else { throw EngineError.unexpectedResponse }
                    try await c.send(Wire.line(.build(spec)))
                    let source = try FileHandle(forReadingFrom: file)
                    defer { try? source.close() }
                    while let chunk = try source.read(upToCount: 1 << 20), !chunk.isEmpty {
                        try Task.checkCancellation()
                        try await c.send(chunk)
                    }
                    while true {
                        switch try Wire.decoder.decode(EngineResponse.self, from: try await c.readLine()) {
                        case let .buildLog(line): continuation.yield(.log(line))
                        case let .image(image):
                            continuation.yield(.built(image))
                            continuation.finish()
                            return
                        case let .error(message): throw EngineError.daemon(message)
                        default: throw EngineError.unexpectedResponse
                        }
                    }
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// Terminals get a connection of their own, so they don't hold up other requests.
    public func openTerminal(_ spec: ExecSpec) async throws -> any TerminalSession {
        try await RemoteTerminal.open(host: host, token: token, spec: spec)
    }

    public nonisolated func execCommand(id: String, command: [String], env: [String] = [], timeout: Int = 120) async throws -> ExecResult {
        let spec = ExecSpec(id: id, command: command, tty: false, rows: 0, cols: 0, env: env)
        let host = self.host, token = self.token
        return try await Task.detached {
            let c = try await LineConnection.open(host.endpoints, pin: host.fingerprint)
            defer { c.cancel() }
            guard case .authenticated = try await c.call(.auth(token: token)) else { throw EngineError.unexpectedResponse }
            try await c.send(Wire.line(.exec(spec)))
            var out = Data()
            let maxBytes = 128 * 1024
            var truncated = false
            while true {
                try Task.checkCancellation()
                switch try Wire.decoder.decode(EngineResponse.self, from: try await c.readLine()) {
                case let .execOutput(_, bytes):
                    if out.count < maxBytes {
                        let take = min(bytes.count, maxBytes - out.count)
                        out.append(contentsOf: bytes[..<take])
                        if take < bytes.count { truncated = true }
                    } else {
                        truncated = true
                    }
                case let .execExited(code):
                    return ExecResult(exitCode: code, output: String(decoding: out, as: UTF8.self), truncated: truncated)
                case let .error(message):
                    throw EngineError.daemon(message)
                default:
                    throw EngineError.unexpectedResponse
                }
            }
        }.value
    }

    public func snapshotStatus() async throws -> SnapshotInfo {
        guard case let .snapshots(info) = try await call(.snapshotStatus) else { throw EngineError.unexpectedResponse }
        return info
    }

    public func snapshotWarm(cpus: UInt32? = nil, memoryMib: UInt64? = nil, rosetta: Bool = false, slots: UInt32? = nil) async throws -> SnapshotInfo {
        guard case let .snapshots(info) = try await separately(.snapshotWarm(cpus: cpus, memoryMib: memoryMib, rosetta: rosetta, slots: slots)) else {
            throw EngineError.unexpectedResponse
        }
        return info
    }

    public func snapshotClear() async throws -> SnapshotInfo {
        guard case let .snapshots(info) = try await call(.snapshotClear) else { throw EngineError.unexpectedResponse }
        return info
    }

    private func container(_ request: EngineRequest) async throws -> Container {
        guard case let .container(c) = try await call(request) else { throw EngineError.unexpectedResponse }
        return c
    }
}
