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

        // Streaming logs over Unix socket replays historical lines then ends when container exited.
        var streamed: [String] = []
        for try await line in engine.logsStream(id: container.id, tail: nil) {
            streamed.append(line.message)
        }
        #expect(streamed.contains("hello-from-swift"))

        #expect(try await engine.remove(id: container.id) == container.id)
        #expect(try await engine.containers().contains { $0.id == container.id } == false)
    }

    @Test func streamingLogsCancellationAndCommitSaveLoad() async throws {
        let engine = SocketEngine()
        _ = try await engine.pull(reference: "alpine")
        let name = "swift-commit-\(Int.random(in: 1000...9999))"
        let committedRef = "swift-committed-\(Int.random(in: 1000...9999)):v1"
        let container = try await engine.run(RunSpec(
            image: "alpine",
            name: name,
            command: ["sh", "-c", "echo live-line-1; sleep 1; echo live-line-2; sleep 30"],
            memoryMib: 256
        ))
        let tarURL = FileManager.default.temporaryDirectory.appending(path: "swift-save-\(UUID().uuidString).tar")
        defer { try? FileManager.default.removeItem(at: tarURL) }

        do {
            // Stream live logs until second line, then break to test mid-stream cancellation without blocking.
            var got: [String] = []
            for try await line in engine.logsStream(id: container.id, tail: nil) {
                got.append(line.message)
                if got.contains("live-line-2") { break }
            }
            #expect(got.contains("live-line-1") && got.contains("live-line-2"))

            // Stop, commit with message + ENV override, export archive, delete image, re-import archive.
            _ = try await engine.stop(id: container.id)
            let committed = try await engine.commit(CommitSpec(
                container: container.id,
                reference: committedRef,
                author: "Swift Test",
                message: "live commit test",
                changes: ["ENV SWIFT_COMMIT=1"]
            ))
            #expect("\(committed.repository):\(committed.tag)" == committedRef)

            try await engine.saveImage(references: [committedRef], to: tarURL)
            let attrs = try FileManager.default.attributesOfItem(atPath: tarURL.path)
            let tarSize = try #require((attrs[.size] as? NSNumber)?.uint64Value)
            #expect(tarSize > 0)

            _ = try await engine.remove(id: container.id)
            _ = try await engine.removeImage(reference: committedRef)
            let loaded = try await engine.loadImage(from: tarURL, size: tarSize)
            #expect(loaded.contains { "\($0.repository):\($0.tag)" == committedRef })
            _ = try await engine.removeImage(reference: committedRef)
        } catch {
            _ = try? await engine.stop(id: container.id)
            _ = try? await engine.remove(id: container.id)
            _ = try? await engine.removeImage(reference: committedRef)
            throw error
        }
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
        _ = try? await engine.removeImage(reference: "swift-live-build:1")
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
            let remoteNet = try await remote.networkInfo()
            #expect(remoteNet.endpoints.contains { $0.containerId == running.id && $0.isRunning })

            // TLS log streaming + immediate cancellation test.
            let logStreamTask = Task {
                for try await _ in remote.logsStream(id: running.id, tail: 5) {
                    break
                }
            }
            try await Task.sleep(for: .milliseconds(150))
            logStreamTask.cancel()
            _ = try? await logStreamTask.value
        }

        // Volumes over TLS: create, list, remove.
        let volName = "swift-tls-vol-\(Int.random(in: 1000...9999))"
        let created = try await remote.createVolume(name: volName)
        #expect(created.name == volName)
        #expect(try await remote.volumes().contains { $0.name == volName })
        _ = try await remote.removeVolume(name: volName)
        #expect(try await remote.volumes().contains { $0.name == volName } == false)

        // Unconditional TLS live streaming logs (+ cancellation), commit, saveImage & loadImage.
        let tlsName = "swift-tls-stream-\(Int.random(in: 1000...9999))"
        let tlsRef = "swift-tls-committed-\(Int.random(in: 1000...9999)):v1"
        let tlsContainer = try await remote.run(RunSpec(
            image: "alpine",
            name: tlsName,
            command: ["sh", "-c", "echo tls-live-1; sleep 1; echo tls-live-2; sleep 30"],
            memoryMib: 256
        ))
        let tlsTar = FileManager.default.temporaryDirectory.appending(path: "swift-tls-save-\(UUID().uuidString).tar")
        defer { try? FileManager.default.removeItem(at: tlsTar) }
        do {
            var seen: [String] = []
            for try await line in remote.logsStream(id: tlsContainer.id, tail: nil) {
                seen.append(line.message)
                if seen.contains("tls-live-2") { break }
            }
            #expect(seen.contains("tls-live-1") && seen.contains("tls-live-2"))

            _ = try await remote.stop(id: tlsContainer.id)
            let committed = try await remote.commit(CommitSpec(container: tlsContainer.id, reference: tlsRef, message: "tls commit"))
            #expect("\(committed.repository):\(committed.tag)" == tlsRef)

            try await remote.saveImage(references: [tlsRef], to: tlsTar)
            let attrs = try FileManager.default.attributesOfItem(atPath: tlsTar.path)
            let tarSize = try #require((attrs[.size] as? NSNumber)?.uint64Value)
            #expect(tarSize > 0)

            _ = try await remote.remove(id: tlsContainer.id)
            _ = try await remote.removeImage(reference: tlsRef)
            let reloaded = try await remote.loadImage(from: tlsTar, size: tarSize)
            #expect(reloaded.contains { "\($0.repository):\($0.tag)" == tlsRef })
            _ = try await remote.removeImage(reference: tlsRef)
        } catch {
            _ = try? await local.stop(id: tlsContainer.id)
            _ = try? await local.remove(id: tlsContainer.id)
            _ = try? await local.removeImage(reference: tlsRef)
            _ = try? await local.revokeDevice(id: paired.host.deviceID)
            throw error
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
