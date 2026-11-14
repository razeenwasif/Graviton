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

        let spec = RunSpec(image: "python:3.13-slim", name: "web", command: ["python3", "-m", "http.server"], env: ["A=1"], memoryMib: 2048,
                           ports: ["8080:8000"], volumes: ["~/code:/app:ro"])
        let run = String(decoding: try Wire.line(.run(spec)), as: UTF8.self)
        // Must parse as graviton_proto::Request::Run (see the Rust test `run_from_swift`).
        #expect(run == "{\"method\":\"run\",\"params\":{\"command\":[\"python3\",\"-m\",\"http.server\"],\"env\":[\"A=1\"],\"image\":\"python:3.13-slim\",\"memory_mib\":2048,\"name\":\"web\",\"ports\":[\"8080:8000\"],\"volumes\":[\"~/code:/app:ro\"]}}\n")

        let removed = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"removed","data":{"id":"abc"}}"#.utf8))
        guard case .removed("abc") = removed else { Issue.record("expected removed"); return }
    }

    @Test func splitsLists() {
        #expect(RunSpec.list("8080:80, 5432\n 3000 ") == ["8080:80", "5432", "3000"])
        #expect(RunSpec.list(" ") == [])
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

@Suite struct TerminalTests {
    @Test func framesMatchRust() throws {
        // Same bytes as graviton-proto's `terminal_frames` test.
        #expect(TerminalFrame.data(Array("ls\r".utf8)).encoded == [0, 0, 0, 0, 3, 0x6c, 0x73, 0x0d])
        #expect(TerminalFrame.resize(rows: 40, cols: 120).encoded == [1, 0, 0, 0, 4, 0, 40, 0, 120])
        let frames: [TerminalFrame] = [.data([1, 2]), .resize(rows: 50, cols: 160), .eof, .exit(-3)]
        var decoder = TerminalFrame.Decoder()
        var got: [TerminalFrame] = []
        for byte in frames.flatMap(\.encoded) {
            decoder.push([byte])
            while let f = try decoder.next() { got.append(f) }
        }
        #expect(got == frames)
        var bad = TerminalFrame.Decoder()
        bad.push([9, 0, 0, 0, 0])
        #expect(throws: EngineError.self) { try bad.next() }
    }

    @Test func encodesNewRequests() throws {
        let line = { (r: EngineRequest) in String(decoding: try Wire.line(r), as: UTF8.self) }
        #expect(try line(.listFiles(id: "web", path: "/etc")) == #"{"method":"list_files","params":{"id":"web","path":"/etc"}}"# + "\n")
        #expect(try line(.stats(id: "web")) == #"{"method":"stats","params":{"id":"web"}}"# + "\n")
        #expect(try line(.exec(ExecSpec(id: "web", command: ["sh"], rows: 30, cols: 100)))
            == #"{"method":"exec","params":{"cols":100,"command":["sh"],"env":[],"id":"web","rows":30,"tty":true}}"# + "\n")
    }

    @Test func decodesStatsAndFiles() throws {
        let stats = #"{"type":"stats","data":{"id":"a","cpus":2,"interval_secs":2,"samples":[{"timestamp_ms":1,"cpu_percent":12.5,"memory_bytes":3,"memory_limit_bytes":4,"net_rx_bytes":5,"net_tx_bytes":6}],"processes":[{"pid":7,"ppid":1,"uid":0,"state":"S","command":"sh","rss_bytes":8,"cpu_ms":9}]}}"#
        guard case let .stats(s) = try Wire.decoder.decode(EngineResponse.self, from: Data(stats.utf8)) else { Issue.record("not stats"); return }
        #expect(s.samples.first?.cpuPercent == 12.5)
        #expect(s.processes.first?.cpuMs == 9)
        let files = #"{"type":"files","data":{"path":"/","entries":[{"name":"bin","kind":"symlink","size":7,"mode":511,"uid":0,"gid":0,"modified":0,"link_target":"usr/bin"}]}}"#
        guard case let .files(l) = try Wire.decoder.decode(EngineResponse.self, from: Data(files.utf8)) else { Issue.record("not files"); return }
        #expect(l.entries.first?.linkTarget == "usr/bin")
        #expect(l.entries.first?.permissions == "lrwxrwxrwx")
    }

    @Test func demoShellEchoesAndExits() async throws {
        let engine = DemoEngine()
        let session = try await engine.openTerminal(ExecSpec(id: "web", command: ["sh"]))
        session.send(Array("whoami\rexit 4\r".utf8))
        var screen = ""
        var code: Int32?
        for await event in session.events {
            switch event {
            case let .output(bytes): screen += String(decoding: bytes, as: UTF8.self)
            case let .exited(c): code = c
            case .failed: break
            }
        }
        #expect(screen.contains("root"))
        #expect(code == 4)
    }
}

@Suite struct BuildTests {
    @Test func encodesBuildLikeRust() throws {
        let spec = BuildSpec(context: "/Users/me/app", tag: "app:1", buildArgs: ["PY=3.12"], noCache: true)
        let line = String(decoding: try Wire.line(.build(spec)), as: UTF8.self)
        #expect(line == #"{"method":"build","params":{"build_args":["PY=3.12"],"context":"/Users/me/app","no_cache":true,"tag":"app:1"}}"# + "\n")
        guard case let .buildLog(text) = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"build_log","data":{"line":"Step 1/3 : FROM alpine"}}"#.utf8)) else {
            Issue.record("not a build log"); return
        }
        #expect(text == "Step 1/3 : FROM alpine")
    }

    @MainActor @Test func demoDataCannotBuild() async {
        let store = EngineStore(useDaemon: false)
        await store.refresh()
        #expect(!store.canBuild)
        await #expect(throws: EngineError.self) { for try await _ in store.build(BuildSpec(context: "/tmp")) {} }
    }
}

@Suite struct RemoteHostTests {
    @Test func dockerignoreMatchesRust() {
        // The same cases as graviton-proto's `dockerignore` test.
        let ig = DockerIgnore(["# deps", "node_modules", "*.log", "**/__pycache__", ".git", "!keep.log"])
        #expect(ig.isIgnored("node_modules"))
        #expect(ig.isIgnored("node_modules/x/y.js"))
        #expect(ig.isIgnored("debug.log"))
        #expect(!ig.isIgnored("keep.log"))
        #expect(!ig.isIgnored("src/debug.log"))
        #expect(ig.isIgnored("src/pkg/__pycache__/a.pyc"))
        #expect(!ig.isIgnored("src/main.py"))
        #expect(DockerIgnore.glob("[a-c]x", "bx") && !DockerIgnore.glob("[!a-c]x", "bx") && DockerIgnore.glob("file?.txt", "file1.txt"))
    }

    @Test func packsAFolderTarCanRead() throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: "graviton-pack-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let deep = "src/" + String(repeating: "very-long-folder-name/", count: 6)
        try FileManager.default.createDirectory(at: dir.appending(path: deep), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: dir.appending(path: "node_modules/x"), withIntermediateDirectories: true)
        try "FROM alpine\n".write(to: dir.appending(path: "Dockerfile"), atomically: true, encoding: .utf8)
        try "node_modules\nDockerfile\n".write(to: dir.appending(path: ".dockerignore"), atomically: true, encoding: .utf8)
        try "deep\n".write(to: dir.appending(path: deep + "file.txt"), atomically: true, encoding: .utf8)
        try "junk".write(to: dir.appending(path: "node_modules/x/junk.js"), atomically: true, encoding: .utf8)
        try FileManager.default.createSymbolicLink(atPath: dir.appending(path: "link").path, withDestinationPath: "src")
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: dir.appending(path: "Dockerfile").path)

        let (archive, size) = try BuildUpload.pack(dir, dockerfile: nil)
        defer { try? FileManager.default.removeItem(at: archive) }
        #expect(size % 512 == 0)
        let tar = Process()
        tar.executableURL = URL(fileURLWithPath: "/usr/bin/tar")
        tar.arguments = ["-tvf", archive.path]
        let pipe = Pipe()
        tar.standardOutput = pipe
        try tar.run()
        tar.waitUntilExit()
        let listing = String(decoding: pipe.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        #expect(tar.terminationStatus == 0)
        #expect(listing.contains(deep + "file.txt"))       // long names survive
        #expect(listing.contains("link -> src"))
        #expect(listing.contains("-rwxr-xr-x") && listing.contains("Dockerfile")) // ignored, but always sent
        #expect(!listing.contains("node_modules"))
    }

    @MainActor @Test func sharesHostsWithGrav() throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: "graviton-hosts-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let url = dir.appending(path: "hosts.json")
        // Exactly what grav writes.
        try #"{"default":"mini","hosts":[{"name":"mini","address":"10.0.0.5","port":7843,"fingerprint":"ab","token":"t","device_id":"d1","host_name":"My Mac mini"}]}"#
            .write(to: url, atomically: true, encoding: .utf8)
        let defaults = UserDefaults(suiteName: "graviton-tests-\(UUID().uuidString)")!
        let hosts = SharedHosts(url: url, defaults: defaults)
        #expect(hosts.hosts.map(\.name) == ["mini"])
        #expect(hosts.hosts.first?.remoteHost.address == "10.0.0.5")
        #expect(hosts.selected == nil)  // the app's choice is its own

        let result = PairingResult(host: RemoteHost(deviceID: "d2", name: "Razeen's MacBook", serviceName: nil, address: "10.0.0.9", port: 7843, fingerprint: "cd"), token: "t2")
        try hosts.add(result)
        #expect(hosts.hosts.map(\.name).sorted() == ["mini", "razeens-macbook"])
        let raw = try String(contentsOf: url, encoding: .utf8)
        #expect(raw.contains(#""default" : "mini""#) && raw.contains(#""device_id" : "d2""#))
        let perms = try FileManager.default.attributesOfItem(atPath: url.path)[.posixPermissions] as? NSNumber
        #expect(perms?.intValue == 0o600)
        try hosts.forget("mini")
        #expect(hosts.hosts.map(\.name) == ["razeens-macbook"])
        #expect(!(try String(contentsOf: url, encoding: .utf8)).contains(#""default" : "mini""#))
    }
}
