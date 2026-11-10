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
