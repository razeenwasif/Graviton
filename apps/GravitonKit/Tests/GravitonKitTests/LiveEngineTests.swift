import Foundation
import Testing
@testable import GravitonKit

/// Talks to a real gravitond. Opt in with `GRAVITON_LIVE=1 swift test` while
/// `scripts/dev-gravitond.sh` is running.
@Suite(.enabled(if: ProcessInfo.processInfo.environment["GRAVITON_LIVE"] == "1"))
struct LiveEngineTests {
    @Test func pullRunLogsRemove() async throws {
        let engine = SocketEngine()
        let image = try await engine.pull(reference: "alpine")
        #expect(image.repository == "alpine")

        let name = "swift-live-\(Int.random(in: 1000...9999))"
        let spec = RunSpec(image: "alpine", name: name, command: ["sh", "-c", "echo hello-from-swift; echo \"$GREETING\" >&2; exit 3"], env: ["GREETING=hi-env"], memoryMib: 512)
        var container = try await engine.run(spec)
        #expect(container.name == name)

        let deadline = Date.now.addingTimeInterval(30)
        while container.status != .exited, Date.now < deadline {
            try await Task.sleep(for: .milliseconds(100))
            container = try await engine.containers().first { $0.id == container.id } ?? container
        }
        #expect(container.status == .exited)
        #expect(container.exitCode == 3)

        let messages = try await engine.logs(id: container.id, tail: nil).map(\.message)
        #expect(messages.contains("hello-from-swift"))
        #expect(messages.contains("hi-env"))

        #expect(try await engine.remove(id: container.id) == container.id)
        #expect(try await engine.containers().contains { $0.id == container.id } == false)
    }

    @Test func terminalStatsFilesInspect() async throws {
        let engine = SocketEngine()
        let name = "swift-term-\(Int.random(in: 1000...9999))"
        let container = try await engine.run(RunSpec(image: "alpine", name: name, command: ["sleep", "300"], memoryMib: 256))

        let session = try await engine.openTerminal(ExecSpec(id: container.id, command: ["sh"], rows: 30, cols: 100))
        session.resize(rows: 33, cols: 101)
        try await Task.sleep(for: .milliseconds(300))
        session.send(Array("stty size; exit 6\r".utf8))
        var screen = ""
        var code: Int32?
        for await event in session.events {
            if case let .output(b) = event { screen += String(decoding: b, as: UTF8.self) }
            if case let .exited(c) = event { code = c }
        }
        #expect(screen.contains("33 101"))
        #expect(code == 6)

        let listing = try await engine.listFiles(id: container.id, path: "/etc")
        #expect(listing.entries.contains { $0.name == "hostname" })
        let hostname = try await engine.readFile(id: container.id, path: "/etc/hostname")
        #expect(hostname.text == name + "\n")

        // Upload + download archive over the Unix socket.
        let scratch = FileManager.default.temporaryDirectory.appending(path: "graviton-live-cp-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: scratch) }
        let upFile = scratch.appending(path: "upload.txt")
        try "swift-socket-upload\n".write(to: upFile, atomically: true, encoding: .utf8)
        let (upTar, upSize) = try BuildUpload.packItems([upFile])
        defer { try? FileManager.default.removeItem(at: upTar) }
        #expect(try await engine.putArchive(id: container.id, path: "/tmp", archiveFile: upTar, size: upSize) >= 1)
        #expect(try await engine.readFile(id: container.id, path: "/tmp/upload.txt").text == "swift-socket-upload\n")
        let dlTar = scratch.appending(path: "download.tar")
        let (isDir, archiveName) = try await engine.getArchive(id: container.id, path: "/tmp/upload.txt", to: dlTar)
        #expect(!isDir && archiveName == "upload.txt")
        #if os(macOS)
        let saved = scratch.appending(path: "saved.txt")
        try BuildUpload.extractArchive(dlTar, isDir: isDir, archiveName: archiveName, to: saved)
        #expect(try String(contentsOf: saved, encoding: .utf8) == "swift-socket-upload\n")
        #endif

        let details = try await engine.inspect(id: container.id)
        #expect(details.command == ["sleep", "300"])

        try await Task.sleep(for: .seconds(5))
        let stats = try await engine.stats(id: container.id)
        #expect(!stats.samples.isEmpty)
        #expect(stats.processes.contains { $0.command == "sleep 300" })

        _ = try await engine.stop(id: container.id)
        _ = try await engine.remove(id: container.id)
    }

    @Test func buildsAndRunsAnImage() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: "graviton-live-build-\(Int.random(in: 1000...9999))")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try "FROM alpine\nARG WHO=nobody\nCOPY hello.txt /greeting\nRUN echo \"$WHO\" >> /greeting\nCMD [\"cat\", \"/greeting\"]\n".write(to: dir.appending(path: "Dockerfile"), atomically: true, encoding: .utf8)
        try "hello from a swift build\n".write(to: dir.appending(path: "hello.txt"), atomically: true, encoding: .utf8)

        let engine = SocketEngine()
        var log: [String] = []
        var built: ImageInfo?
        for try await event in engine.build(BuildSpec(context: dir.path, tag: "swift-live-build:1", buildArgs: ["WHO=swift"])) {
            switch event {
            case let .log(line): log.append(line)
            case let .built(image): built = image
            }
        }
        #expect(log.contains { $0.hasPrefix("Step 4/5 : RUN") })
        #expect(built?.repository == "swift-live-build")

        var container = try await engine.run(RunSpec(image: "swift-live-build:1", memoryMib: 256))
        let deadline = Date.now.addingTimeInterval(30)
        while container.status != .exited, Date.now < deadline {
            try await Task.sleep(for: .milliseconds(100))
            container = try await engine.containers().first { $0.id == container.id } ?? container
        }
        let messages = try await engine.logs(id: container.id, tail: nil).map(\.message)
        #expect(messages.contains("hello from a swift build"))
        #expect(messages.contains("swift"))
        _ = try await engine.remove(id: container.id)

        // A bad Dockerfile line comes back as the daemon's error.
        try "FROM alpine\nRUN false\n".write(to: dir.appending(path: "Dockerfile"), atomically: true, encoding: .utf8)
        await #expect(throws: EngineError.self) { for try await _ in engine.build(BuildSpec(context: dir.path, tag: "swift-live-build:bad")) {} }
    }

    @Test func reportsDaemonErrors() async throws {
        await #expect(throws: EngineError.daemon("no such container: definitely-not-here")) {
            try await SocketEngine().remove(id: "definitely-not-here")
        }
    }
}

/// Pairs with a real gravitond running `--remote`, then uses the remote engine.
@Suite(.enabled(if: ProcessInfo.processInfo.environment["GRAVITON_LIVE"] == "1"))
struct LiveRemoteTests {
    @Test func pairAndListOverTLS() async throws {
        let local = SocketEngine()
        let info = try await local.pairBegin()
        #expect(info.code.count == 8)

        // A wrong fingerprint must be refused before anything is sent.
        let wrong = PairingTarget(name: info.host, address: "127.0.0.1", port: info.port, fingerprint: String(repeating: "0", count: 64))
        await #expect(throws: EngineError.self) { try await Pairing.pair(wrong, code: info.code, deviceName: "Swift live test") }

        let target = PairingTarget(name: info.host, address: "127.0.0.1", port: info.port, fingerprint: info.fingerprint)
        let paired = try await Pairing.pair(target, code: info.code, deviceName: "Swift live test")
        #expect(paired.host.fingerprint == info.fingerprint)
        #expect(paired.host.name == info.host)

        let remote = RemoteEngine(host: paired.host, token: paired.token)
        #expect(try await remote.ping() == 2)
        async let containers = remote.containers()
        async let images = remote.images()
        async let engine = remote.engineInfo()
        let (c, i, e) = try await (containers, images, engine)
        #expect(e.protocol == 2)
        _ = i

        // A shell over TLS, if anything is running to open one in.
        if let running = c.first(where: \.isRunning) {
            let session = try await remote.openTerminal(ExecSpec(id: running.id, command: ["sh", "-c", "echo over-tls; exit 9"]))
            var screen = ""
            var code: Int32?
            for await event in session.events {
                if case let .output(b) = event { screen += String(decoding: b, as: UTF8.self) }
                if case let .exited(x) = event { code = x }
            }
            #expect(screen.contains("over-tls"))
            #expect(code == 9)

            // Upload + download archive over TLS.
            let scratch = FileManager.default.temporaryDirectory.appending(path: "graviton-tls-cp-\(UUID().uuidString)")
            try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
            defer { try? FileManager.default.removeItem(at: scratch) }
            let upFile = scratch.appending(path: "tls-upload.txt")
            try "swift-tls-upload\n".write(to: upFile, atomically: true, encoding: .utf8)
            let (upTar, upSize) = try BuildUpload.packItems([upFile])
            defer { try? FileManager.default.removeItem(at: upTar) }
            #expect(try await remote.putArchive(id: running.id, path: "/tmp", archiveFile: upTar, size: upSize) >= 1)
            let dlTar = scratch.appending(path: "tls-download.tar")
            let (isDir, archiveName) = try await remote.getArchive(id: running.id, path: "/tmp/tls-upload.txt", to: dlTar)
            #expect(!isDir && archiveName == "tls-upload.txt")
            #if os(macOS)
            let saved = scratch.appending(path: "tls-saved.txt")
            try BuildUpload.extractArchive(dlTar, isDir: isDir, archiveName: archiveName, to: saved)
            #expect(try String(contentsOf: saved, encoding: .utf8) == "swift-tls-upload\n")
            #endif

            // The request connection is unaffected.
            #expect(try await remote.ping() == 2)
        }

        try await local.revokeDevice(id: paired.host.deviceID)
        let revoked = RemoteEngine(host: paired.host, token: paired.token)
        await #expect(throws: EngineError.self) { try await revoked.ping() }
    }
}

@Suite struct PairingLinkTests {
    @Test func parsesLinks() {
        let fp = String(repeating: "ab", count: 32)
        let t = PairingTarget(link: "graviton://pair?host=192.0.2.52&port=7843&fp=\(fp)&code=Z5VRBPZ3&name=Workstation")
        #expect(t?.address == "192.0.2.52")
        #expect(t?.port == 7843)
        #expect(t?.fingerprint == fp)
        #expect(t?.code == "Z5VRBPZ3")
        #expect(t?.name == "Workstation")
        #expect(PairingTarget(link: "https://example.com") == nil)
        #expect(PairingTarget(link: "graviton://pair?host=x&fp=short") == nil)
        #expect(RemoteHost.short(fp) == "ABAB ABAB ABAB ABAB")
    }
}
