import Foundation
import Testing
@testable import GravitonKit

/// These JSON shapes must match `crates/graviton-proto` (see its tests).
@Suite struct WireTests {
    @Test func encodesRequestsLikeRust() throws {
        let logs = String(decoding: try Wire.line(.logs(id: "web", tail: 10)), as: UTF8.self)
        #expect(logs == "{\"method\":\"logs\",\"params\":{\"id\":\"web\",\"tail\":10}}\n")

        let ping = String(decoding: try Wire.line(.ping), as: UTF8.self)
        #expect(ping == "{\"method\":\"ping\"}\n")

        let list = String(decoding: try Wire.line(.listContainers), as: UTF8.self)
        #expect(list == "{\"method\":\"list_containers\"}\n")
    }

    @Test func decodesResponses() throws {
        let pong = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"pong","data":{"protocol":1}}"#.utf8))
        guard case .pong(1) = pong else { Issue.record("expected pong, got \(pong)"); return }

        let json = #"""
        {"type":"containers","data":[{"id":"a3f9c2e81b07","name":"api-gateway","image":"storefront/api:0.4.2",
        "status":"running","exit_code":null,"ports":[{"host_port":8080,"container_port":80,"protocol":"tcp"}],
        "mounts":[],"ip_address":"192.168.64.4","cpu_percent":12.4,"memory_bytes":432013312,
        "memory_limit_bytes":1073741824,"created_at":1700000000,"started_at":1700000100}]}
        """#
        guard case let .containers(list) = try Wire.decoder.decode(EngineResponse.self, from: Data(json.utf8)) else {
            Issue.record("expected containers"); return
        }
        #expect(list.first?.name == "api-gateway")
        #expect(list.first?.ports.first?.hostPort == 8080)
        #expect(list.first?.isRunning == true)

        let err = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"error","data":{"message":"no such container: x"}}"#.utf8))
        guard case .error("no such container: x") = err else { Issue.record("expected error"); return }
    }
}

@Suite struct DemoEngineTests {
    @Test func lifecycle() async throws {
        let engine = DemoEngine()
        let stopped = try await engine.stop(id: "web")
        #expect(stopped.status == .exited)
        await #expect(throws: EngineError.self) { try await engine.stop(id: "web") }
        let started = try await engine.start(id: "worker")
        #expect(started.isRunning)
        let tail = try await engine.logs(id: "api-gateway", tail: 3)
        #expect(tail.count == 3)
    }

    @Test func filters() async {
        let all = await DemoEngine().containers()
        #expect(ContainerFilter.running.apply(all).count == 4)
        #expect(ContainerFilter.stopped.apply(all).count == 2)
        #expect(ContainerFilter.all.apply(all, search: "redis").map(\.name) == ["redis-cache"])
    }
}

@Suite struct FormatTests {
    @Test func formats() {
        #expect(Format.bytes(512) == "512 B")
        #expect(Format.bytes(412 * 1024 * 1024) == "412 MB")
        #expect(Format.uptime(11_520) == "3h 12m")
        #expect(Format.percent(nil) == "—")
    }
}

@Suite struct RunTests {
    @Test func encodesPullRunRemove() throws {
        let pull = String(decoding: try Wire.line(.pull(reference: "alpine")), as: UTF8.self)
        #expect(pull == "{\"method\":\"pull\",\"params\":{\"reference\":\"alpine\"}}\n")

        let spec = RunSpec(image: "python:3.13-slim", name: "web", command: ["python3", "-m", "http.server"], env: ["A=1"], memoryMib: 2048)
        let run = String(decoding: try Wire.line(.run(spec)), as: UTF8.self)
        // Must parse as graviton_proto::Request::Run (see the Rust test `run_from_swift`).
        #expect(run == "{\"method\":\"run\",\"params\":{\"command\":[\"python3\",\"-m\",\"http.server\"],\"env\":[\"A=1\"],\"image\":\"python:3.13-slim\",\"memory_mib\":2048,\"name\":\"web\"}}\n")

        let removed = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"removed","data":{"id":"abc"}}"#.utf8))
        guard case .removed("abc") = removed else { Issue.record("expected removed"); return }
    }

    @Test func splitsCommandLines() {
        #expect(RunSpec.words("python3 -c 'print(1 + 1)'") == ["python3", "-c", "print(1 + 1)"])
        #expect(RunSpec.words(#"sh -c "echo \"hi there\"""#) == ["sh", "-c", #"echo "hi there""#])
        #expect(RunSpec.words("  echo   a\\ b  ") == ["echo", "a b"])
        #expect(RunSpec.words("") == [])
    }

    @Test func demoCanRemoveButNotPull() async throws {
        let engine = DemoEngine()
        await #expect(throws: EngineError.self) { try await engine.pull(reference: "alpine") }
        await #expect(throws: EngineError.self) { try await engine.remove(id: "web") }
        #expect(try await engine.remove(id: "worker") == "e4c5d6f7a8b9")
    }
}
