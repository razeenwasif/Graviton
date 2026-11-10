import Foundation

/// What a terminal session reports.
public enum TerminalEvent: Sendable, Equatable {
    /// Bytes for the screen.
    case output([UInt8])
    /// The program exited with this code; nothing follows.
    case exited(Int32)
    /// The connection broke or the session couldn't start.
    case failed(String)
}

/// An interactive program in a container, on a pseudo-terminal.
public protocol TerminalSession: AnyObject, Sendable {
    var events: AsyncStream<TerminalEvent> { get }
    /// Keystrokes (or pasted text) for the program.
    func send(_ bytes: [UInt8])
    func resize(rows: UInt16, cols: UInt16)
    /// Hang up: the program gets SIGHUP.
    func close()
}

/// Terminal frames, as in `graviton_proto::frame`: one kind byte, a
/// big-endian u32 length, then the payload.
public enum TerminalFrame: Equatable, Sendable {
    case data([UInt8])
    case resize(rows: UInt16, cols: UInt16)
    case eof
    case exit(Int32)

    static let maxPayload = 1 << 20

    public var encoded: [UInt8] {
        let (kind, payload): (UInt8, [UInt8]) = switch self {
        case let .data(bytes): (0, bytes)
        case let .resize(rows, cols): (1, Self.be(rows) + Self.be(cols))
        case .eof: (2, [])
        case let .exit(code): (3, Self.be(UInt32(bitPattern: code)))
        }
        return [kind] + Self.be(UInt32(payload.count)) + payload
    }

    private static func be<T: FixedWidthInteger>(_ v: T) -> [UInt8] {
        withUnsafeBytes(of: v.bigEndian) { Array($0) }
    }

    /// Splits a byte stream into frames, however it arrives.
    public struct Decoder: Sendable {
        private var buffer: [UInt8] = []
        public init() {}

        public mutating func push(_ bytes: some Sequence<UInt8>) { buffer.append(contentsOf: bytes) }

        /// The next complete frame, or `nil` until more bytes arrive.
        public mutating func next() throws -> TerminalFrame? {
            guard buffer.count >= 5 else { return nil }
            let len = Int(buffer[1]) << 24 | Int(buffer[2]) << 16 | Int(buffer[3]) << 8 | Int(buffer[4])
            guard len <= TerminalFrame.maxPayload else { throw EngineError.daemon("terminal frame too long") }
            guard buffer.count >= 5 + len else { return nil }
            let kind = buffer[0]
            let p = Array(buffer[5..<(5 + len)])
            buffer.removeFirst(5 + len)
            switch (kind, p.count) {
            case (0, _): return .data(p)
            case (1, 4): return .resize(rows: UInt16(p[0]) << 8 | UInt16(p[1]), cols: UInt16(p[2]) << 8 | UInt16(p[3]))
            case (2, 0): return .eof
            case (3, 4): return .exit(Int32(bitPattern: UInt32(p[0]) << 24 | UInt32(p[1]) << 16 | UInt32(p[2]) << 8 | UInt32(p[3])))
            default: throw EngineError.daemon("bad terminal frame (kind \(kind))")
            }
        }
    }
}

/// Feeds bytes from a connection through a frame decoder into terminal events.
struct FrameReader {
    private var decoder = TerminalFrame.Decoder()

    /// Events for these bytes; `done` once the program has exited.
    mutating func events(for bytes: some Sequence<UInt8>) -> (events: [TerminalEvent], done: Bool) {
        decoder.push(bytes)
        var out: [TerminalEvent] = []
        do {
            while let frame = try decoder.next() {
                switch frame {
                case let .data(b): out.append(.output(b))
                case let .exit(code): out.append(.exited(code)); return (out, true)
                case .resize, .eof: break
                }
            }
        } catch {
            out.append(.failed(error.localizedDescription))
            return (out, true)
        }
        return (out, false)
    }
}

// MARK: - Local socket

/// A terminal over gravitond's Unix socket.
final class SocketTerminal: TerminalSession, @unchecked Sendable {
    let events: AsyncStream<TerminalEvent>
    private let continuation: AsyncStream<TerminalEvent>.Continuation
    private let fd: Int32
    private let lock = NSLock()
    private var closed = false

    /// Connects and starts `spec`; throws if gravitond refuses.
    init(path: String, spec: ExecSpec) throws {
        fd = try SocketEngine.connect(path: path)
        (events, continuation) = AsyncStream.makeStream(bufferingPolicy: .unbounded)
        do {
            try SocketEngine.writeAll(fd, Array(try Wire.line(.exec(spec))))
            // Byte by byte: what follows the answer is terminal frames.
            var line: [UInt8] = []
            var byte: UInt8 = 0
            while true {
                let n = read(fd, &byte, 1)
                guard n == 1 else { throw EngineError.io("gravitond closed the connection") }
                if byte == UInt8(ascii: "\n") { break }
                line.append(byte)
            }
            switch try Wire.decoder.decode(EngineResponse.self, from: Data(line)) {
            case .attached: break
            case let .error(message): throw EngineError.daemon(message)
            default: throw EngineError.unexpectedResponse
            }
        } catch {
            Darwin.close(fd)
            throw error
        }
        let thread = Thread { [self] in readLoop() }
        thread.name = "graviton-terminal"
        thread.start()
    }

    private func readLoop() {
        var reader = FrameReader()
        var chunk = [UInt8](repeating: 0, count: 64 * 1024)
        while true {
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 {
                lock.withLock {
                    if !closed { continuation.yield(.failed("The connection to gravitond closed.")) }
                }
                break
            }
            let (events, done) = reader.events(for: chunk[0..<n])
            for e in events { continuation.yield(e) }
            if done { break }
        }
        continuation.finish()
        close()
    }

    func send(_ bytes: [UInt8]) { write(.data(bytes)) }
    func resize(rows: UInt16, cols: UInt16) { write(.resize(rows: rows, cols: cols)) }

    private func write(_ frame: TerminalFrame) {
        lock.withLock {
            guard !closed else { return }
            try? SocketEngine.writeAll(fd, frame.encoded)
        }
    }

    func close() {
        lock.withLock {
            guard !closed else { return }
            closed = true
            shutdown(fd, SHUT_RDWR)
            Darwin.close(fd)
        }
    }

    deinit { close() }
}

// MARK: - Paired Mac

/// A terminal over the TLS link to a paired Mac, on a connection of its own.
final class RemoteTerminal: TerminalSession, @unchecked Sendable {
    let events: AsyncStream<TerminalEvent>
    private let continuation: AsyncStream<TerminalEvent>.Continuation
    private let connection: LineConnection
    private let lock = NSLock()
    private var closed = false
    /// Frames go out one at a time, in order.
    private var outbox: Task<Void, Never>?

    private init(connection: LineConnection) {
        self.connection = connection
        (events, continuation) = AsyncStream.makeStream(bufferingPolicy: .unbounded)
    }

    static func open(host: RemoteHost, token: String, spec: ExecSpec) async throws -> RemoteTerminal {
        let c = try await LineConnection.open(host.endpoints, pin: host.fingerprint)
        do {
            guard case .authenticated = try await c.call(.auth(token: token)) else { throw EngineError.unexpectedResponse }
            guard case .attached = try await c.call(.exec(spec)) else { throw EngineError.unexpectedResponse }
        } catch {
            c.cancel()
            throw error
        }
        let session = RemoteTerminal(connection: c)
        Task.detached { await session.readLoop() }
        return session
    }

    private func readLoop() async {
        var reader = FrameReader()
        while true {
            do {
                let (events, done) = reader.events(for: try await connection.receive())
                for e in events { continuation.yield(e) }
                if done { break }
            } catch {
                if !lock.withLock({ closed }) { continuation.yield(.failed(error.localizedDescription)) }
                break
            }
        }
        continuation.finish()
        close()
    }

    func send(_ bytes: [UInt8]) { enqueue(.data(bytes)) }
    func resize(rows: UInt16, cols: UInt16) { enqueue(.resize(rows: rows, cols: cols)) }

    private func enqueue(_ frame: TerminalFrame) {
        lock.withLock {
            guard !closed else { return }
            let previous = outbox
            let connection = self.connection
            outbox = Task {
                await previous?.value
                try? await connection.send(Data(frame.encoded))
            }
        }
    }

    func close() {
        lock.withLock {
            guard !closed else { return }
            closed = true
            connection.cancel()
        }
    }

    deinit { close() }
}

// MARK: - Demo

/// A pretend shell for demo data: echoes what's typed and answers a few commands.
final class DemoTerminal: TerminalSession, @unchecked Sendable {
    let events: AsyncStream<TerminalEvent>
    private let continuation: AsyncStream<TerminalEvent>.Continuation
    private let lock = NSLock()
    private var line: [UInt8] = []
    private var cwd = "/app"
    private let name: String

    init(container: String) {
        name = container
        (events, continuation) = AsyncStream.makeStream(bufferingPolicy: .unbounded)
        emit("\u{1b}[2mDemo shell: Graviton isn't connected to gravitond, so this only pretends.\u{1b}[0m\r\n")
        prompt()
    }

    private var promptText: String { "\u{1b}[38;5;111m\(name)\u{1b}[0m:\(cwd)# " }
    private func prompt() { emit(promptText) }
    private func emit(_ s: String) { continuation.yield(.output(Array(s.utf8))) }

    func send(_ bytes: [UInt8]) {
        lock.withLock {
            for b in bytes {
                switch b {
                case 0x0d, 0x0a:
                    emit("\r\n")
                    let command = String(decoding: line, as: UTF8.self).trimmingCharacters(in: .whitespaces)
                    line = []
                    if run(command) { return }
                    prompt()
                case 0x7f, 0x08:
                    if !line.isEmpty { line.removeLast(); emit("\u{8} \u{8}") }
                case 0x03:
                    line = []
                    emit("^C\r\n"); prompt()
                case 0x04 where line.isEmpty:
                    _ = run("exit")
                    return
                case 0x20...0x7e:
                    line.append(b); emit(String(UnicodeScalar(b)))
                default:
                    break
                }
            }
        }
    }

    /// Returns true when the shell exited.
    private func run(_ command: String) -> Bool {
        let words = command.split(separator: " ").map(String.init)
        switch words.first {
        case nil: break
        case "exit":
            continuation.yield(.exited(Int32(words.dropFirst().first ?? "0") ?? 0))
            continuation.finish()
            return true
        case "pwd": emit(cwd + "\r\n")
        case "whoami": emit("root\r\n")
        case "hostname": emit(name + "\r\n")
        case "uname": emit(words.contains("-a") ? "Linux \(name) 6.12.115-0-virt #1-Alpine SMP aarch64 Linux\r\n" : "Linux\r\n")
        case "echo": emit(words.dropFirst().joined(separator: " ") + "\r\n")
        case "cd": cwd = words.count > 1 ? (words[1].hasPrefix("/") ? words[1] : cwd + "/" + words[1]) : "/root"
        case "ls": emit("\u{1b}[1;34mnode_modules\u{1b}[0m  package.json  \u{1b}[1;34msrc\u{1b}[0m  tsconfig.json\r\n")
        case "clear": emit("\u{1b}[H\u{1b}[2J")
        default: emit("sh: \(words[0]): not available in the demo shell\r\n")
        }
        return false
    }

    func resize(rows: UInt16, cols: UInt16) {}
    func close() { continuation.finish() }
}
