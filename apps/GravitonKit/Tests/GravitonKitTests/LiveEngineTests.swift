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
        #expect(try await remote.ping() == 1)
        async let containers = remote.containers()
        async let images = remote.images()
        async let engine = remote.engineInfo()
        let (c, i, e) = try await (containers, images, engine)
        #expect(e.protocol == 1)
        _ = (c, i)

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
