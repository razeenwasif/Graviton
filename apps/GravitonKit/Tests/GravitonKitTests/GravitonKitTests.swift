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
        "status":"running","exit_code":null,"ports":[{"host_port":8080,"container_port":80,"protocol":"tcp"},{"host_ip":"0.0.0.0","host_port":5353,"container_port":53,"protocol":"udp"}],
        "mounts":[],"ip_address":"192.168.64.4","cpu_percent":12.4,"memory_bytes":432013312,
        "memory_limit_bytes":1073741824,"created_at":1700000000,"started_at":1700000100}]}
        """#
        guard case let .containers(list) = try Wire.decoder.decode(EngineResponse.self, from: Data(json.utf8)) else {
            Issue.record("expected containers"); return
        }
        #expect(list.first?.name == "api-gateway")
        #expect(list.first?.ports.first?.hostPort == 8080)
        #expect(list.first?.ports.first?.bindIp == "127.0.0.1")
        #expect(list.first?.ports.last?.hostIp == "0.0.0.0")
        #expect(list.first?.ports.last?.protocol == .udp)
        #expect(list.first?.ports.last?.fullLabel == "0.0.0.0:5353 → 53/udp")
        #expect(list.first?.ports.last?.compactLabel == "0.0.0.0:5353→53/udp")
        #expect(list.first?.isRunning == true)

        let err = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"error","data":{"message":"no such container: x"}}"#.utf8))
        guard case .error("no such container: x") = err else { Issue.record("expected error"); return }
    }

    @Test func engineInfoDecodesHostMemoryAndComputesSegments() throws {
        let withHost = #"""
        {"type":"engine","data":{"version":"0.1.0","protocol":1,"kernel":"Linux 6.12.118-graviton",
        "arch":"arm64","cpus":12,"memory_bytes":17179869184,"memory_used_bytes":2147483648,
        "disk_bytes":0,"disk_used_bytes":2000,"rosetta":true,"host_memory_used_bytes":10737418240}}
        """#
        guard case let .engine(info) = try Wire.decoder.decode(EngineResponse.self, from: Data(withHost.utf8)) else {
            Issue.record("expected engine"); return
        }
        #expect(info.hasHostMemory)
        #expect(info.vmMemoryUsedBytes == 2_147_483_648)
        #expect(info.totalMemoryUsedBytes == 10_737_418_240)
        #expect(info.otherHostMemoryUsedBytes == 8_589_934_592)
        #expect(abs(info.vmMemoryFraction - 0.125) < 0.001)
        #expect(abs(info.otherHostMemoryFraction - 0.5) < 0.001)
        #expect(abs(info.hostMemoryFraction - 0.625) < 0.001)

        // Backwards compatibility when daemon omits host_memory_used_bytes
        let legacy = #"""
        {"type":"engine","data":{"version":"0.1.0","protocol":1,"kernel":"Linux 6.12",
        "arch":"arm64","cpus":10,"memory_bytes":17179869184,"memory_used_bytes":1073741824,
        "disk_bytes":0,"disk_used_bytes":1000}}
        """#
        guard case let .engine(oldInfo) = try Wire.decoder.decode(EngineResponse.self, from: Data(legacy.utf8)) else {
            Issue.record("expected legacy engine"); return
        }
        #expect(!oldInfo.hasHostMemory)
        #expect(oldInfo.totalMemoryUsedBytes == 1_073_741_824)
        #expect(oldInfo.otherHostMemoryUsedBytes == 0)
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

        let forked = try await engine.fork(ForkSpec(source: "postgres", names: ["pg-pr-42"], env: ["POSTGRES_DB=staging"]))
        #expect(forked.count == 1)
        #expect(forked.first?.name == "pg-pr-42")
        #expect(forked.first?.forkedFrom == "5d10e7b3c4aa")
        let entries = try await engine.diff(id: "pg-pr-42")
        #expect(entries.count == 2)
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

        let pullStream = String(decoding: try Wire.line(.pull(reference: "alpine", progress: true)), as: UTF8.self)
        #expect(pullStream == "{\"method\":\"pull\",\"params\":{\"progress\":true,\"reference\":\"alpine\"}}\n")

        let pullPlatform = String(decoding: try Wire.line(.pull(reference: "alpine", progress: true, platform: "linux/amd64")), as: UTF8.self)
        #expect(pullPlatform == "{\"method\":\"pull\",\"params\":{\"platform\":\"linux/amd64\",\"progress\":true,\"reference\":\"alpine\"}}\n")

        let engineJSON = #"{"type":"engine","data":{"version":"0.1.0","protocol":2,"kernel":"Linux 6.12.118-graviton","arch":"arm64","cpus":10,"memory_bytes":16,"memory_used_bytes":2,"disk_bytes":0,"disk_used_bytes":1,"rosetta":true}}"#
        guard case let .engine(info) = try Wire.decoder.decode(EngineResponse.self, from: Data(engineJSON.utf8)) else {
            Issue.record("expected engine info"); return
        }
        #expect(info.rosetta == true && info.kernel == "Linux 6.12.118-graviton")

        let dfJSON = #"{"type":"disk_usage","data":{"images":1,"unused_images":0,"blobs_bytes":100,"unused_images_bytes":0,"containers":1,"stopped_containers":0,"container_disks_bytes":200,"stopped_disks_bytes":0,"build_cache_steps":2,"build_cache_bytes":300,"unreferenced_bytes":50,"snapshots":2,"snapshots_bytes":400}}"#
        guard case let .diskUsage(usage) = try Wire.decoder.decode(EngineResponse.self, from: Data(dfJSON.utf8)) else {
            Issue.record("expected disk usage"); return
        }
        #expect(usage.snapshots == 2 && usage.snapshotsBytes == 400 && usage.alwaysReclaimable == 750)

        let resolvedJSON = #"{"type":"pull_progress","data":{"stage":"resolved","layers":2,"total_bytes":1000}}"#
        guard case let .pullProgress(.resolved(layers, totalBytes)) = try Wire.decoder.decode(EngineResponse.self, from: Data(resolvedJSON.utf8)) else {
            Issue.record("expected resolved pull_progress"); return
        }
        #expect(layers == 2 && totalBytes == 1000)

        let layerJSON = #"{"type":"pull_progress","data":{"stage":"layer","index":0,"total_layers":2,"digest":"sha256:abcdef0123456789","done":400,"total":800,"cached":false}}"#
        guard case let .pullProgress(.layer(layer)) = try Wire.decoder.decode(EngineResponse.self, from: Data(layerJSON.utf8)) else {
            Issue.record("expected layer pull_progress"); return
        }
        #expect(layer.shortDigest == "abcdef012345" && !layer.isComplete && layer.fraction == 0.5)

        var state = PullState()
        state.apply(.resolved(layers: 2, totalBytes: 1000))
        state.apply(.layer(layer))
        #expect(state.downloadedBytes == 400 && state.fraction == 0.4)
        state.apply(.layer(LayerPullProgress(index: 1, totalLayers: 2, digest: "sha256:1111222233334444", done: 200, total: 200, cached: true)))
        #expect(state.downloadedBytes == 600 && state.completedLayers == 1 && state.fraction == 0.6)

        let spec = RunSpec(image: "python:3.13-slim", name: "web", command: ["python3", "-m", "http.server"], env: ["A=1"], memoryMib: 2048,
                           ports: ["8080:8000"], volumes: ["~/code:/app:ro"])
        let run = String(decoding: try Wire.line(.run(spec)), as: UTF8.self)
        // Must parse as graviton_proto::Request::Run (see the Rust test `run_from_swift`).
        #expect(run == "{\"method\":\"run\",\"params\":{\"command\":[\"python3\",\"-m\",\"http.server\"],\"env\":[\"A=1\"],\"image\":\"python:3.13-slim\",\"memory_mib\":2048,\"name\":\"web\",\"ports\":[\"8080:8000\"],\"volumes\":[\"~/code:/app:ro\"]}}\n")

        let withRestart = String(decoding: try Wire.line(.run(RunSpec(image: "redis", restart: .onFailure))), as: UTF8.self)
        #expect(withRestart == "{\"method\":\"run\",\"params\":{\"env\":[],\"image\":\"redis\",\"ports\":[],\"restart\":\"on-failure\",\"volumes\":[]}}\n")

        let removed = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"removed","data":{"id":"abc"}}"#.utf8))
        guard case .removed("abc") = removed else { Issue.record("expected removed"); return }

        let forkLine = String(decoding: try Wire.line(.fork(ForkSpec(source: "pg", names: ["pg-test"], count: 1, env: ["A=1"]))), as: UTF8.self)
        #expect(forkLine == "{\"method\":\"fork\",\"params\":{\"count\":1,\"env\":[\"A=1\"],\"names\":[\"pg-test\"],\"source\":\"pg\",\"start\":true}}\n")

        let diffLine = String(decoding: try Wire.line(.diff(id: "pg-test")), as: UTF8.self)
        #expect(diffLine == "{\"method\":\"diff\",\"params\":{\"id\":\"pg-test\"}}\n")

        let diffRespJSON = #"{"type":"diff","data":{"entries":[{"kind":"added","path":"/tmp/x","size_bytes":12},{"kind":"deleted","path":"/etc/old"}]}}"#
        guard case let .diff(diffEntries) = try Wire.decoder.decode(EngineResponse.self, from: Data(diffRespJSON.utf8)) else {
            Issue.record("expected diff response"); return
        }
        #expect(diffEntries.count == 2 && diffEntries[0].kind == .added && diffEntries[1].kind == .deleted)
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
        #expect(try line(.getArchive(id: "web", path: "/etc/hosts")) == #"{"method":"get_archive","params":{"id":"web","path":"/etc/hosts"}}"# + "\n")
        #expect(try line(.putArchive(id: "web", path: "/tmp", size: 2048)) == #"{"method":"put_archive","params":{"id":"web","path":"/tmp","size":2048}}"# + "\n")
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
        guard case let .archive(isDir, name) = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"archive","data":{"is_dir":true,"name":"etc"}}"#.utf8)) else {
            Issue.record("not archive"); return
        }
        #expect(isDir && name == "etc")
        guard case let .copied(count) = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"copied","data":{"files":4}}"#.utf8)) else {
            Issue.record("not copied"); return
        }
        #expect(count == 4)
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

    @Test func packsAndExtractsArchiveItems() throws {
        let fm = FileManager.default
        let root = fm.temporaryDirectory.appending(path: "graviton-items-\(UUID().uuidString)")
        defer { try? fm.removeItem(at: root) }
        let nested = root.appending(path: "bundle/sub")
        try fm.createDirectory(at: nested, withIntermediateDirectories: true)
        let single = root.appending(path: "hello.txt")
        try "hello world\n".write(to: single, atomically: true, encoding: .utf8)
        try "inside\n".write(to: nested.appending(path: "data.txt"), atomically: true, encoding: .utf8)
        try fm.createSymbolicLink(atPath: root.appending(path: "bundle/ref").path, withDestinationPath: "sub/data.txt")

        // Round-trip single file into a renamed destination.
        let (fileTar, fileSize) = try BuildUpload.packItems([single])
        defer { try? fm.removeItem(at: fileTar) }
        #expect(fileSize > 0 && fileSize % 512 == 0)
        #if os(macOS)
        let savedFile = root.appending(path: "saved.txt")
        try BuildUpload.extractArchive(fileTar, isDir: false, archiveName: "hello.txt", to: savedFile)
        #expect(try String(contentsOf: savedFile, encoding: .utf8) == "hello world\n")

        // Round-trip folder with nested file + symlink.
        let (dirTar, dirSize) = try BuildUpload.packItems([root.appending(path: "bundle")])
        defer { try? fm.removeItem(at: dirTar) }
        #expect(dirSize > 0 && dirSize % 512 == 0)
        let savedDir = root.appending(path: "extracted-bundle")
        try BuildUpload.extractArchive(dirTar, isDir: true, archiveName: "bundle", to: savedDir)
        #expect(try String(contentsOf: savedDir.appending(path: "sub/data.txt"), encoding: .utf8) == "inside\n")
        #expect(try fm.destinationOfSymbolicLink(atPath: savedDir.appending(path: "ref").path) == "sub/data.txt")
        #endif
    }

    @MainActor @Test func sharesHostsWithGrav() throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: "graviton-hosts-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let url = dir.appending(path: "hosts.json")
        // Exactly what grav writes.
        try #"{"default":"mini","hosts":[{"name":"mini","address":"10.0.0.5","port":7843,"fingerprint":"ab","token":"t","device_id":"d1","host_name":"My Mac mini"}]}"#
            .write(to: url, atomically: true, encoding: .utf8)
        let hosts = SharedHosts(url: url)
        #expect(hosts.hosts.map(\.name) == ["mini"])
        #expect(hosts.hosts.first?.remoteHost.address == "10.0.0.5")
        #expect(hosts.selected == "mini")  // grav's default is the Mac the app shows
        hosts.select(nil)
        #expect((try String(contentsOf: url, encoding: .utf8)).contains(#""hosts""#))
        #expect(!(try String(contentsOf: url, encoding: .utf8)).contains(#""default" : "mini""#))
        #expect(SharedHosts(url: url).selected == nil)
        hosts.select("mini")
        #expect(SharedHosts(url: url).selected == "mini")

        let result = PairingResult(host: RemoteHost(deviceID: "d2", name: "My MacBook", serviceName: nil, address: "10.0.0.9", port: 7843, fingerprint: "cd"), token: "t2")
        try hosts.add(result)
        #expect(hosts.hosts.map(\.name).sorted() == ["mini", "my-macbook"])
        let raw = try String(contentsOf: url, encoding: .utf8)
        #expect(raw.contains(#""default" : "mini""#) && raw.contains(#""device_id" : "d2""#))
        let perms = try FileManager.default.attributesOfItem(atPath: url.path)[.posixPermissions] as? NSNumber
        #expect(perms?.intValue == 0o600)
        try hosts.forget("mini")
        #expect(hosts.hosts.map(\.name) == ["my-macbook"])
        #expect(!(try String(contentsOf: url, encoding: .utf8)).contains(#""default" : "mini""#))
    }

    @MainActor @Test func multiAddressHostsBonjourSyncAndUpdate() throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: "graviton-multi-hosts-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let url = dir.appending(path: "hosts.json")
        let fp = String(repeating: "ab", count: 32)
        try #"{"default":"mini","hosts":[{"name":"mini","address":"100.64.0.10","port":7843,"fingerprint":"\#(fp)","token":"t","device_id":"d1","host_name":"BuildHost"}]}"#
            .write(to: url, atomically: true, encoding: .utf8)

        let hosts = SharedHosts(url: url)
        #expect(hosts.hosts.first?.allAddresses == ["100.64.0.10"])

        // Multi-address update writes primary `address` + ordered `addresses` + `service_name`.
        let updated = try hosts.update("mini", addresses: ["192.0.2.61", "100.64.0.10", "192.0.2.61"], port: 7843, serviceName: "BuildHost")
        #expect(updated.address == "192.0.2.61")
        #expect(updated.allAddresses == ["192.0.2.61", "100.64.0.10"])
        #expect(updated.remoteHost.endpoints.count == 3) // 1 Bonjour + 2 hostPort

        // Re-read verifies persistence + compatibility.
        let reloaded = SharedHosts(url: url)
        #expect(reloaded.hosts.first?.allAddresses == ["192.0.2.61", "100.64.0.10"])
        #expect(reloaded.hosts.first?.serviceName == "BuildHost")

        // Bonjour TXT fingerprint sync updates serviceName if renamed on the LAN.
        reloaded.syncBonjour([HostBrowser.Found(name: "BuildHost-2", fingerprint: fp.uppercased())])
        #expect(reloaded.hosts.first?.serviceName == "BuildHost-2")

        // Legacy RemoteHost JSON without `addresses` still decodes cleanly.
        let legacyJSON = #"{"deviceID":"d9","name":"Mini","address":"10.0.0.5","port":7843,"fingerprint":"\#(fp)"}"#
        let decodedLegacy = try JSONDecoder().decode(RemoteHost.self, from: Data(legacyJSON.utf8))
        #expect(decodedLegacy.allAddresses == ["10.0.0.5"])

        // Multi-address PairingTarget parsing.
        let link = "graviton://pair?host=192.0.2.61&addrs=192.0.2.61,100.64.0.10&service=BuildHost&port=7843&fp=\(fp)&code=Z5VRBPZ3&name=mini"
        let target = PairingTarget(link: link)
        #expect(target?.address == "192.0.2.61")
        #expect(target?.addresses == ["192.0.2.61", "100.64.0.10"])
        #expect(target?.serviceName == "BuildHost")

        // HostStore.updateAddresses and syncBonjour on isolated UserDefaults.
        let suiteName = "graviton-hoststore-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }
        defaults.set(try JSONEncoder().encode([decodedLegacy]), forKey: "graviton.hosts")
        let hostStore = HostStore(defaults: defaults)
        hostStore.updateAddresses(for: "d9", addresses: ["192.0.2.61", "100.64.0.10"], port: 7843, serviceName: "BuildHost")
        #expect(hostStore.hosts.first?.allAddresses == ["192.0.2.61", "100.64.0.10"])
        hostStore.syncBonjour([HostBrowser.Found(name: "BuildHost-Lab", fingerprint: fp)])
        #expect(hostStore.hosts.first?.serviceName == "BuildHost-Lab")
    }
}

@Suite struct CleanupTests {
    @Test func requestsMatchRust() throws {
        let line = { (r: EngineRequest) in String(decoding: try Wire.line(r), as: UTF8.self) }
        #expect(try line(.diskUsage) == #"{"method":"disk_usage"}"# + "\n")
        #expect(try line(.removeImage(reference: "alpine:3.20")) == #"{"method":"remove_image","params":{"reference":"alpine:3.20"}}"# + "\n")
        #expect(try line(.prune(PruneSpec(images: true, dryRun: true))) == #"{"method":"prune","params":{"containers":false,"dry_run":true,"images":true}}"# + "\n")
        let json = #"{"type":"pruned","data":{"containers":["web"],"images":["docker.io/library/busybox:latest"],"build_cache_steps":3,"blobs":7,"freed_bytes":1234,"dry_run":false}}"#
        guard case let .pruned(r) = try Wire.decoder.decode(EngineResponse.self, from: Data(json.utf8)) else { Issue.record("not pruned"); return }
        #expect(r.containers == ["web"] && r.blobs == 7 && r.freedBytes == 1234)
        let removed = #"{"type":"image_removed","data":{"reference":"busybox","freed_bytes":10,"kept_bytes":2}}"#
        guard case let .imageRemoved(_, freed, kept) = try Wire.decoder.decode(EngineResponse.self, from: Data(removed.utf8)) else { Issue.record("not removed"); return }
        #expect(freed == 10 && kept == 2)
    }

    @Test func demoCleansUp() async throws {
        let demo = DemoEngine()
        let before = await demo.diskUsage()
        #expect(before.unusedImages > 0)
        let dry = await demo.prune(PruneSpec(images: true, dryRun: true))
        #expect(await demo.diskUsage() == before) // a dry run changes nothing
        let real = await demo.prune(PruneSpec(images: true))
        #expect(real.freedBytes == dry.freedBytes)
        #expect(await demo.diskUsage().unusedImages == 0)
        await #expect(throws: EngineError.self) { _ = try await demo.removeImage(reference: "docker.io/library/postgres:17-alpine") }
    }
}

@Suite struct VolumesAndNetworkTests {
    @Test func wireMatchesRust() throws {
        let line = { (r: EngineRequest) in String(decoding: try Wire.line(r), as: UTF8.self) }
        #expect(try line(.listVolumes) == #"{"method":"list_volumes"}"# + "\n")
        #expect(try line(.createVolume(name: "pgdata")) == #"{"method":"create_volume","params":{"name":"pgdata"}}"# + "\n")
        #expect(try line(.removeVolume(name: "pgdata")) == #"{"method":"remove_volume","params":{"name":"pgdata"}}"# + "\n")
        #expect(try line(.networkInfo) == #"{"method":"network_info"}"# + "\n")

        let volsJSON = #"{"type":"volumes","data":[{"name":"pgdata","path":"/tmp/volumes/pgdata","size_bytes":4096,"file_count":12,"created_at":1700000000,"used_by":[{"container_id":"abc123def456","container_name":"postgres","target":"/var/lib/postgresql/data","read_only":false,"running":true}]}]}"#
        guard case let .volumes(vols) = try Wire.decoder.decode(EngineResponse.self, from: Data(volsJSON.utf8)) else {
            Issue.record("expected volumes"); return
        }
        #expect(vols.count == 1 && vols[0].name == "pgdata" && vols[0].isInUse && vols[0].usedBy[0].containerName == "postgres")

        let removedJSON = #"{"type":"volume_removed","data":{"name":"pgdata","freed_bytes":4096}}"#
        guard case let .volumeRemoved(name, freed) = try Wire.decoder.decode(EngineResponse.self, from: Data(removedJSON.utf8)) else {
            Issue.record("expected volume_removed"); return
        }
        #expect(name == "pgdata" && freed == 4096)

        let netJSON = #"{"type":"network","data":{"name":"graviton","driver":"L2 switch + NAT","subnet":"192.168.64.0/24","gateway":"192.168.64.1","dns_server":"127.0.0.11:53","dns_domain":"graviton","connected_ports":2,"learned_macs":2,"frames_forwarded":128,"bytes_forwarded":65536,"endpoints":[{"container_id":"abc123def456","container_name":"postgres","image":"postgres:17-alpine","status":"running","ip_address":"192.168.64.2","dns_names":["postgres","postgres.graviton","abc123def456"],"dns_enabled":true,"ports":[{"host_port":5432,"container_port":5432,"protocol":"tcp"}]}]}}"#
        guard case let .network(net) = try Wire.decoder.decode(EngineResponse.self, from: Data(netJSON.utf8)) else {
            Issue.record("expected network"); return
        }
        #expect(net.subnet == "192.168.64.0/24" && net.connectedPorts == 2 && net.endpoints.first?.dnsNames.contains("postgres.graviton") == true)
    }

    @Test func demoVolumesAndNetwork() async throws {
        let demo = DemoEngine()
        let initial = await demo.volumes()
        #expect(initial.contains { $0.name == "pgdata" && $0.isInUse })
        #expect(initial.contains { $0.name == "redis-snapshots" && !$0.isInUse })

        // Can't remove an in-use volume or create a bad/duplicate name.
        await #expect(throws: EngineError.self) { _ = try await demo.removeVolume(name: "pgdata") }
        await #expect(throws: EngineError.self) { _ = try await demo.createVolume(name: "../escape") }
        await #expect(throws: EngineError.self) { _ = try await demo.createVolume(name: "pgdata") }

        let created = try await demo.createVolume(name: "scratch-vol")
        #expect(created.name == "scratch-vol" && !created.isInUse)
        #expect(try await demo.removeVolume(name: "scratch-vol") == 0)

        let net = await demo.networkInfo()
        #expect(net.subnet == "192.168.64.0/24" && net.connectedPorts == 4)
        #expect(net.endpoints.first?.isRunning == true)
    }
}

@Suite struct ComposeStacksTests {
    @Test func wireDecodesComposeFieldsAndEncodesRunSpec() throws {
        let json = #"""
        {"type":"containers","data":[{"id":"a3f9c2e81b07","name":"shop-api-1","image":"shop/api:1",
        "status":"running","exit_code":null,"ports":[],"mounts":[],"ip_address":"192.168.64.4",
        "cpu_percent":4.0,"memory_bytes":104857600,"memory_limit_bytes":1073741824,
        "created_at":1700000000,"started_at":1700000100,"project":"shop","service":"api"}]}
        """#
        guard case let .containers(list) = try Wire.decoder.decode(EngineResponse.self, from: Data(json.utf8)) else {
            Issue.record("expected containers"); return
        }
        #expect(list.first?.project == "shop" && list.first?.service == "api")

        let detailsJSON = #"""
        {"type":"details","data":{"container":{"id":"a3f9c2e81b07","name":"shop-api-1","image":"shop/api:1",
        "status":"running","exit_code":null,"ports":[],"mounts":[],"ip_address":"192.168.64.4",
        "cpu_percent":4.0,"memory_bytes":104857600,"memory_limit_bytes":1073741824,
        "created_at":1700000000,"started_at":1700000100,"project":"shop","service":"api"},
        "image_reference":"docker.io/shop/api:1","image_digest":"sha256:abc","command":["api"],
        "env":[],"working_dir":"/app","user":"app","hostname":"api-host","cpus":2,"memory_mib":1024,
        "kernel":"Linux 6.12","vm_dir":"/tmp/vm","restart":"on-failure",
        "project":"shop","service":"api","labels":{"env":"dev"}}}
        """#
        guard case let .details(d) = try Wire.decoder.decode(EngineResponse.self, from: Data(detailsJSON.utf8)) else {
            Issue.record("expected details"); return
        }
        #expect(d.project == "shop" && d.service == "api" && d.labels?["env"] == "dev")

        let spec = RunSpec(
            image: "alpine:3.22",
            name: "shop-api-1",
            entrypoint: ["/bin/sh"],
            workingDir: "/app",
            user: "1000:1000",
            hostname: "api-host",
            project: "shop",
            service: "api",
            labels: ["tier": "web"],
            dnsAliases: ["backend"]
        )
        let encoded = String(decoding: try Wire.line(.run(spec)), as: UTF8.self)
        #expect(encoded.contains(#""project":"shop""#))
        #expect(encoded.contains(#""service":"api""#))
        #expect(encoded.contains(#""working_dir":"/app""#))
        #expect(encoded.contains(#""dns_aliases":["backend"]"#))
        #expect(encoded.contains(#""labels":{"tier":"web"}"#))
    }

    @MainActor @Test func demoGroupsStacksAndControlsLifecycle() async {
        let store = EngineStore(useDaemon: false)
        await store.refresh()
        let grouped = ContainerStack.group(store.containers)
        #expect(grouped.stacks.map(\.name) == ["storefront"])
        #expect(grouped.stacks.first?.containers.count == 5)
        #expect(grouped.stacks.first?.runningCount == 4)
        #expect(grouped.standalone.map(\.name) == ["minio"])

        // Search matches project and service names too.
        #expect(ContainerFilter.all.apply(store.containers, search: "storefront").count == 5)

        // Start stack starts the stopped worker too.
        await store.startStack("storefront")
        #expect(store.stacks.first?.runningCount == 5)
        #expect(store.stacks.first?.allRunning == true)

        // Stop stack stops all 5 services without touching standalone minio.
        await store.stopStack("storefront")
        #expect(store.stacks.first?.runningCount == 0)
        #expect(store.stacks.first?.anyRunning == false)

        // Restart stack when none running brings them all up.
        await store.restartStack("storefront")
        #expect(store.stacks.first?.runningCount == 5)
    }
}

@Suite struct FairyBridgeTests {
    @Test func wireEncodesSnapshotsAndDecodesExec() throws {
        let line = { (r: EngineRequest) in String(decoding: try Wire.line(r), as: UTF8.self) }
        #expect(try line(.snapshotStatus) == #"{"method":"snapshot_status"}"# + "\n")
        #expect(try line(.snapshotClear) == #"{"method":"snapshot_clear"}"# + "\n")
        #expect(try line(.snapshotWarm(cpus: 4, memoryMib: 2048, rosetta: true, slots: 2))
            == #"{"method":"snapshot_warm","params":{"cpus":4,"memory_mib":2048,"rosetta":true,"slots":2}}"# + "\n")
        #expect(try line(.snapshotWarm()) == #"{"method":"snapshot_warm","params":{}}"# + "\n")

        let snapJSON = #"{"type":"snapshots","data":{"enabled":true,"total_slots":2,"in_use":1,"total_bytes":27000000,"max_slots_per_shape":8,"groups":[{"cpus":2,"memory_mib":1024,"rosetta":false,"slots":2,"in_use":1,"size_bytes":27000000}]}}"#
        guard case let .snapshots(info) = try Wire.decoder.decode(EngineResponse.self, from: Data(snapJSON.utf8)) else {
            Issue.record("expected snapshots"); return
        }
        #expect(info.enabled && info.totalSlots == 2 && info.inUse == 1 && info.groups.count == 1)

        let outJSON = #"{"type":"exec_output","data":{"stream":"stdout","data":[104,105,10]}}"#
        guard case let .execOutput(stream, bytes) = try Wire.decoder.decode(EngineResponse.self, from: Data(outJSON.utf8)) else {
            Issue.record("expected exec_output"); return
        }
        #expect(stream == "stdout" && String(decoding: bytes, as: UTF8.self) == "hi\n")

        let exitJSON = #"{"type":"exec_exited","data":{"code":7}}"#
        guard case let .execExited(code) = try Wire.decoder.decode(EngineResponse.self, from: Data(exitJSON.utf8)) else {
            Issue.record("expected exec_exited"); return
        }
        #expect(code == 7)

        #expect(try line(.tagImage(source: "app:1", target: "ghcr.io/me/app:1"))
            == #"{"method":"tag_image","params":{"source":"app:1","target":"ghcr.io/me/app:1"}}"# + "\n")
        #expect(try line(.pushImage(reference: "ghcr.io/me/app:1"))
            == #"{"method":"push_image","params":{"reference":"ghcr.io/me/app:1"}}"# + "\n")
        #expect(try line(.pushImage(reference: "ghcr.io/me/app:1", progress: true))
            == #"{"method":"push_image","params":{"progress":true,"reference":"ghcr.io/me/app:1"}}"# + "\n")

        let pushedJSON = #"{"type":"image_pushed","data":{"reference":"ghcr.io/me/app:1","digest":"sha256:ef01","size_bytes":2048,"blobs_pushed":1,"blobs_skipped":2}}"#
        guard case let .imagePushed(pushed) = try Wire.decoder.decode(EngineResponse.self, from: Data(pushedJSON.utf8)) else {
            Issue.record("expected image_pushed"); return
        }
        #expect(pushed.reference == "ghcr.io/me/app:1" && pushed.digest == "sha256:ef01" && pushed.blobsPushed == 1 && pushed.blobsSkipped == 2)
    }

    @Test func approvalPolicyMatchesTieredRules() {
        #expect(FairyApprovalPolicy.supportedTools.count == 33)

        // Auto-run reads & reversible operations
        for readTool in ["status", "events", "containers", "inspect", "logs", "stats",
                         "list_files", "read_file", "images", "volumes", "network", "list_logins",
                         "navigate", "switch_host", "pull", "tag_image"] {
            #expect(!FairyApprovalPolicy.requiresTouchID(tool: readTool, args: [:]), "\(readTool) should not need Touch ID")
        }
        #expect(!FairyApprovalPolicy.requiresTouchID(tool: "container_action", args: ["action": .string("start")]))
        #expect(!FairyApprovalPolicy.requiresTouchID(tool: "container_action", args: ["action": .string("restart")]))
        #expect(!FairyApprovalPolicy.requiresTouchID(tool: "container_action", args: ["action": .string("unpause")]))
        #expect(!FairyApprovalPolicy.requiresTouchID(tool: "stack_action", args: ["action": .string("start")]))
        #expect(!FairyApprovalPolicy.requiresTouchID(tool: "volume_action", args: ["action": .string("create")]))
        #expect(!FairyApprovalPolicy.requiresTouchID(tool: "disk_and_prune", args: ["action": .string("usage")]))
        #expect(!FairyApprovalPolicy.requiresTouchID(tool: "disk_and_prune", args: ["action": .string("prune_dry_run")]))
        #expect(!FairyApprovalPolicy.requiresTouchID(tool: "snapshots", args: ["action": .string("status")]))
        for safeCompose in ["config", "ps", "logs", "start", "restart"] {
            #expect(!FairyApprovalPolicy.requiresTouchID(tool: "compose", args: ["action": .string(safeCompose)]))
        }

        // Touch ID required operations
        #expect(FairyApprovalPolicy.requiresTouchID(tool: "container_action", args: ["action": .string("pause")]))
        #expect(FairyApprovalPolicy.requiresTouchID(tool: "container_action", args: ["action": .string("stop")]))
        #expect(FairyApprovalPolicy.requiresTouchID(tool: "container_action", args: ["action": .string("remove")]))
        #expect(FairyApprovalPolicy.requiresTouchID(tool: "stack_action", args: ["action": .string("stop")]))
        #expect(FairyApprovalPolicy.requiresTouchID(tool: "volume_action", args: ["action": .string("remove")]))
        #expect(FairyApprovalPolicy.requiresTouchID(tool: "disk_and_prune", args: ["action": .string("prune")]))
        #expect(FairyApprovalPolicy.requiresTouchID(tool: "snapshots", args: ["action": .string("warm")]))
        #expect(FairyApprovalPolicy.requiresTouchID(tool: "snapshots", args: ["action": .string("clear")]))
        for gatedCompose in ["up", "down", "stop", "build"] {
            #expect(FairyApprovalPolicy.requiresTouchID(tool: "compose", args: ["action": .string(gatedCompose)]))
        }
        for gatedTool in ["run", "exec", "fork_container", "commit_container", "build", "push_image", "save_image", "load_image", "remove_image", "copy_files", "unknown_future_tool"] {
            #expect(FairyApprovalPolicy.requiresTouchID(tool: gatedTool, args: [:]), "\(gatedTool) must require Touch ID")
        }
        #expect(!FairyApprovalPolicy.requiresTouchID(tool: "diff_container", args: ["container": .string("api")]))

        let commitSummary = FairyApprovalPolicy.summarize(
            tool: "commit_container",
            args: ["container": .string("api"), "reference": .string("api:v2"), "reason": .string("snapshot config")],
            host: "mini"
        )
        #expect(commitSummary.title == "Commit api → api:v2")
        #expect(!commitSummary.isDestructive)
        #expect(commitSummary.touchIDReason.contains("mini"))

        let saveSummary = FairyApprovalPolicy.summarize(
            tool: "save_image",
            args: ["references": .array([.string("alpine:3.22")]), "output_path": .string("/tmp/alpine.tar"), "reason": .string("backup")],
            host: "mini"
        )
        #expect(saveSummary.title == "Export image archive")
        #expect(saveSummary.detail.contains("/tmp/alpine.tar"))

        let loadSummary = FairyApprovalPolicy.summarize(
            tool: "load_image",
            args: ["input_path": .string("/tmp/alpine.tar"), "reason": .string("restore")],
            host: "mini"
        )
        #expect(loadSummary.title == "Import image archive")
        #expect(loadSummary.detail.contains("/tmp/alpine.tar"))

        let tagSummary = FairyApprovalPolicy.summarize(
            tool: "tag_image",
            args: ["source": .string("app:1"), "target": .string("ghcr.io/me/app:1"), "reason": .string("prepare push")],
            host: "mini"
        )
        #expect(tagSummary.title == "Tag image app:1 → ghcr.io/me/app:1")
        #expect(!tagSummary.isDestructive)
        #expect(tagSummary.touchIDReason.contains("mini"))

        let pushSummary = FairyApprovalPolicy.summarize(
            tool: "push_image",
            args: ["reference": .string("ghcr.io/me/app:1"), "reason": .string("publish build")],
            host: "mini"
        )
        #expect(pushSummary.title == "Push image ghcr.io/me/app:1")
        #expect(!pushSummary.isDestructive)
        #expect(pushSummary.touchIDReason.contains("ghcr.io/me/app:1"))
        #expect(pushSummary.touchIDReason.contains("mini"))

        let pauseSummary = FairyApprovalPolicy.summarize(
            tool: "container_action",
            args: ["action": .string("pause"), "container": .string("api"), "reason": .string("freeze load")],
            host: "mini"
        )
        #expect(pauseSummary.title == "Pause container api")
        #expect(!pauseSummary.isDestructive)
        #expect(pauseSummary.touchIDReason.contains("pause"))
        #expect(pauseSummary.touchIDReason.contains("mini"))

        let multiArchBuild = FairyApprovalPolicy.summarize(
            tool: "build",
            args: [
                "context": .string("/tmp/app"),
                "tag": .string("app:1"),
                "platform": .string("linux/arm64,linux/amd64"),
                "reason": .string("multi-arch"),
            ],
            host: "local"
        )
        #expect(multiArchBuild.detail.contains("Platform: linux/arm64,linux/amd64"))
        #expect(multiArchBuild.touchIDReason.contains("linux/arm64,linux/amd64"))

        let secRun = FairyApprovalPolicy.summarize(
            tool: "run",
            args: [
                "image": .string("alpine:3.22"),
                "read_only": .bool(true),
                "init": .bool(true),
                "cap_drop": .array([.string("ALL")]),
                "cap_add": .array([.string("NET_BIND_SERVICE")]),
                "tmpfs": .array([.string("/tmp:rw,noexec,size=64m")]),
                "reason": .string("locked down service"),
            ],
            host: "local"
        )
        #expect(secRun.detail.contains("Read-only rootfs"))
        #expect(secRun.detail.contains("Init: docker-init"))
        #expect(secRun.detail.contains("Cap drop: ALL"))
        #expect(secRun.detail.contains("Cap add: NET_BIND_SERVICE"))
        #expect(secRun.detail.contains("Tmpfs: /tmp:rw,noexec,size=64m"))
    }

    @Test func redactsSecretsByDefaultAndPreservesWhenAsked() {
        let env = ["APP_ENV=dev", "API_TOKEN=supersecret123", "DB_PASSWORD=pw"]
        let masked = FairyApprovalPolicy.redactEnv(env, includeValues: false)
        #expect(masked.allSatisfy { $0.contains("[redacted") })
        let revealed = FairyApprovalPolicy.redactEnv(env, includeValues: true)
        #expect(revealed[0] == "APP_ENV=dev")
        #expect(revealed[1].contains("[redacted"))
        #expect(revealed[2].contains("[redacted"))
    }

    #if os(macOS)
    @Test func toolServerAndLinkClientRoundTripOverUnixSocket() async throws {
        let dir = "/tmp/graviton-fairy-test-\(UUID().uuidString.prefix(8))"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }

        let toolsSock = "\(dir)/tools.sock"
        let server = FairyToolServer(socketPath: toolsSock)
        try server.start { tool, args in
            return .object(["echo": .string(tool), "filter": args["filter"] ?? .null])
        }
        defer { server.stop() }

        let grant = try #require(server.grant())
        #expect(grant.token.count == 64)

        // Helper speaking single-line JSON over Unix domain socket (matches Node GravitonBridge)
        func callRaw(_ payload: String) throws -> [String: Any] {
            let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
            defer { Darwin.close(fd) }
            var addr = sockaddr_un()
            addr.sun_family = sa_family_t(AF_UNIX)
            withUnsafeMutablePointer(to: &addr.sun_path) { raw in
                raw.withMemoryRebound(to: CChar.self, capacity: 104) { _ = strncpy($0, toolsSock, 103) }
            }
            addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
            let rc = withUnsafePointer(to: &addr) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
                }
            }
            #expect(rc == 0)
            let bytes = Array((payload + "\n").utf8)
            _ = bytes.withUnsafeBytes { Darwin.write(fd, $0.baseAddress!, $0.count) }
            var buf = [UInt8](repeating: 0, count: 4096)
            let n = Darwin.read(fd, &buf, buf.count)
            let data = Data(buf[0..<max(0, n)])
            return try #require(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        }

        // Valid call
        let ok = try callRaw(#"{"id":1,"token":"\#(grant.token)","tool":"containers","args":{"filter":"running"}}"#)
        #expect(ok["id"] as? Int == 1)
        let res = ok["result"] as? [String: Any]
        #expect(res?["echo"] as? String == "containers" && res?["filter"] as? String == "running")

        // Unknown tool rejected
        let badTool = try callRaw(#"{"id":2,"token":"\#(grant.token)","tool":"shell_exec","args":{}}"#)
        #expect((badTool["error"] as? String)?.contains("no tool") == true)

        // Revoked token rejected
        server.revoke(grant.token)
        let badToken = try callRaw(#"{"id":3,"token":"\#(grant.token)","tool":"status","args":{}}"#)
        #expect((badToken["error"] as? String)?.contains("key isn't current") == true)

        // Mock Fairy link server on graviton.sock
        let fairySock = "\(dir)/graviton.sock"
        let listenFD = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        var sAddr = sockaddr_un()
        sAddr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutablePointer(to: &sAddr.sun_path) { raw in
            raw.withMemoryRebound(to: CChar.self, capacity: 104) { _ = strncpy($0, fairySock, 103) }
        }
        sAddr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let brc = withUnsafePointer(to: &sAddr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(listenFD, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        #expect(brc == 0 && Darwin.listen(listenFD, 4) == 0)
        let mockThread = Thread {
            defer { Darwin.close(listenFD) }
            let conn = Darwin.accept(listenFD, nil, nil)
            guard conn >= 0 else { return }
            defer { Darwin.close(conn) }
            var data = Data()
            var buf = [UInt8](repeating: 0, count: 4096)
            while !String(decoding: data, as: UTF8.self).contains("\"type\":\"ask\"") {
                let n = Darwin.read(conn, &buf, buf.count)
                if n <= 0 { break }
                data.append(contentsOf: buf[0..<n])
            }
            let replies = [
                #"{"type":"hello","name":"Fairy"}"#,
                #"{"type":"text","text":"Working"}"#,
                #"{"type":"done","text":"All good."}"#,
            ].joined(separator: "\n") + "\n"
            let out = Array(replies.utf8)
            _ = out.withUnsafeBytes { Darwin.write(conn, $0.baseAddress!, $0.count) }
        }
        mockThread.start()

        let client = FairyLinkClient(socketPath: fairySock)
        var events: [FairyLinkEvent] = []
        for try await ev in client.ask(
            text: "status?",
            context: FairyAskContext(host: "This Mac", connection: "live", section: "Containers"),
            fresh: true,
            tools: grant,
            wakeIfNeeded: false
        ) {
            events.append(ev)
        }
        #expect(events == [.hello(name: "Fairy"), .text("Working"), .done("All good.")])
    }

    @Test(.enabled(if: ProcessInfo.processInfo.environment["GRAVITON_FAIRY_E2E_SOCKET"] != nil))
    func crossProcessNodeSwiftE2E() async throws {
        let fairySock = try #require(ProcessInfo.processInfo.environment["GRAVITON_FAIRY_E2E_SOCKET"])
        let dir = "/tmp/graviton-fairy-e2e-\(UUID().uuidString.prefix(8))"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }

        final class GateState: @unchecked Sendable {
            private let lock = NSLock()
            var allowTouchID = false
            var called: [(tool: String, needsAuth: Bool)] = []

            func record(tool: String, needsAuth: Bool) -> Bool {
                lock.lock()
                defer { lock.unlock() }
                called.append((tool, needsAuth))
                return allowTouchID
            }

            func setAllow(_ allowed: Bool) {
                lock.lock()
                allowTouchID = allowed
                lock.unlock()
            }

            func snapshot() -> [(tool: String, needsAuth: Bool)] {
                lock.lock()
                defer { lock.unlock() }
                return called
            }
        }

        let gate = GateState()
        let toolsSock = "\(dir)/tools.sock"
        let server = FairyToolServer(socketPath: toolsSock)
        try server.start { tool, args in
            let needsAuth = FairyApprovalPolicy.requiresTouchID(tool: tool, args: args)
            let allowed = gate.record(tool: tool, needsAuth: needsAuth)
            if needsAuth && !allowed {
                throw EngineError.daemon("Touch ID approval denied by user.")
            }
            switch tool {
            case "status":
                return .object(["host": .string("e2e-mac"), "running": .int(4)])
            case "container_action":
                return .object(["removed": args["container"] ?? .null])
            default:
                throw EngineError.daemon("Unexpected E2E tool: \(tool)")
            }
        }
        defer { server.stop() }

        let client = FairyLinkClient(socketPath: fairySock)

        // Turn 1: read auto-runs while Touch ID gated mutation denies cleanly
        gate.setAllow(false)
        let grant1 = try #require(server.grant())
        var events1: [FairyLinkEvent] = []
        for try await ev in client.ask(
            text: "check status and remove api-gateway",
            context: FairyAskContext(host: "e2e-mac", connection: "live", section: "Containers"),
            fresh: true,
            tools: grant1,
            wakeIfNeeded: false
        ) {
            events1.append(ev)
        }
        server.revoke(grant1.token)
        #expect(events1.contains(.hello(name: "Fairy")))
        let done1 = try #require(events1.compactMap { if case let .done(t) = $0 { return t }; return nil }.first)
        #expect(done1.contains("e2e-mac"))
        #expect(done1.contains("Touch ID approval denied by user."))

        // Turn 2: fresh token + approved Touch ID allows remove to execute
        gate.setAllow(true)
        let grant2 = try #require(server.grant())
        var events2: [FairyLinkEvent] = []
        for try await ev in client.ask(
            text: "now remove api-gateway",
            context: FairyAskContext(host: "e2e-mac", connection: "live", section: "Containers"),
            fresh: false,
            tools: grant2,
            wakeIfNeeded: false
        ) {
            events2.append(ev)
        }
        server.revoke(grant2.token)
        let done2 = try #require(events2.compactMap { if case let .done(t) = $0 { return t }; return nil }.first)
        #expect(done2.contains("e2e-mac"))
        #expect(done2.contains("api-gateway"))

        let calls = gate.snapshot()
        #expect(calls.count == 4)
        #expect(calls[0].tool == "status" && !calls[0].needsAuth)
        #expect(calls[1].tool == "container_action" && calls[1].needsAuth)
        #expect(calls[2].tool == "status" && !calls[2].needsAuth)
        #expect(calls[3].tool == "container_action" && calls[3].needsAuth)

        // Revoked token is immediately rejected by FairyToolServer
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        defer { Darwin.close(fd) }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutablePointer(to: &addr.sun_path) { raw in
            raw.withMemoryRebound(to: CChar.self, capacity: 104) { _ = strncpy($0, toolsSock, 103) }
        }
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        #expect(rc == 0)
        let payload = Array((#"{"id":99,"token":"\#(grant2.token)","tool":"status","args":{}}"# + "\n").utf8)
        _ = payload.withUnsafeBytes { Darwin.write(fd, $0.baseAddress!, $0.count) }
        var buf = [UInt8](repeating: 0, count: 4096)
        let n = Darwin.read(fd, &buf, buf.count)
        let revokedResp = try #require(try JSONSerialization.jsonObject(with: Data(buf[0..<max(0, n)])) as? [String: Any])
        #expect((revokedResp["error"] as? String)?.contains("key isn't current") == true)
    }
    #endif
}

@Suite struct FairyHistoryTests {
    @Test func saveLoadSanitizesInterruptedAndEnforcesPermissions() throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: "graviton-fairy-hist-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let fileURL = dir.appending(path: "fairy-history.json")
        let store = FairyHistoryStore(url: fileURL)

        let t1 = Date(timeIntervalSince1970: 1_700_000_100)
        let t2 = Date(timeIntervalSince1970: 1_700_000_200)
        let stepDone = FairyToolStep(tool: "container_action", title: "Remove old-api", status: .done, touchIDVerified: true)
        let stepRunning = FairyToolStep(tool: "logs", title: "Read logs", status: .running)
        let stepWaiting = FairyToolStep(tool: "prune", title: "Prune storage", status: .waitingTouchID)

        let oldConv = FairyConversation(
            title: "Older thread",
            hostName: "This Mac",
            createdAt: t1,
            updatedAt: t1,
            messages: [
                FairyChatMessage(role: .user, text: "Check logs"),
                FairyChatMessage(role: .assistant, text: "Working on it", steps: [stepRunning, stepWaiting]),
            ]
        )
        let newConv = FairyConversation(
            title: "Clean up old-api on mini",
            hostName: "mini",
            createdAt: t2,
            updatedAt: t2,
            messages: [
                FairyChatMessage(role: .user, text: "Remove old-api"),
                FairyChatMessage(role: .assistant, text: "Removed cleanly.", steps: [stepDone]),
            ]
        )

        try store.save(FairyHistorySnapshot(activeConversationID: oldConv.id, conversations: [oldConv, newConv]))
        let attrs = try FileManager.default.attributesOfItem(atPath: fileURL.path)
        let perms = attrs[.posixPermissions] as? NSNumber
        #expect(perms?.intValue == 0o600)

        let loaded = store.load()
        #expect(loaded.activeConversationID == oldConv.id)
        #expect(loaded.conversations.map(\.id) == [newConv.id, oldConv.id])
        #expect(loaded.conversations[0].messages[1].steps[0].touchIDVerified == true)
        #expect(loaded.conversations[0].messages[1].steps[0].status == .done)

        let restoredSteps = loaded.conversations[1].messages[1].steps
        #expect(restoredSteps[0].status == .failed && restoredSteps[0].note == "Interrupted")
        #expect(restoredSteps[1].status == .failed && restoredSteps[1].note == "Interrupted")

        // Corrupt JSON recovers cleanly without crashing
        let corruptURL = dir.appending(path: "corrupt.json")
        try Data("{not json".utf8).write(to: corruptURL)
        let empty = FairyHistoryStore(url: corruptURL).load()
        #expect(empty.conversations.isEmpty)
        #expect(empty.activeConversationID == nil)
    }

    @Test func helpersDeriveTitleFilterAndBuildContinuationSummary() {
        #expect(FairyHistoryStore.deriveTitle(from: "   \n  ") == "New conversation")
        #expect(FairyHistoryStore.deriveTitle(from: "  Check   running\n containers  ") == "Check running containers")
        let longTitle = FairyHistoryStore.deriveTitle(from: String(repeating: "a", count: 90))
        #expect(longTitle.count == 64 && longTitle.hasSuffix("…"))

        let convA = FairyConversation(
            title: "Restart redis on mini",
            hostName: "mini",
            messages: [
                FairyChatMessage(role: .user, text: "Please restart redis"),
                FairyChatMessage(role: .assistant, text: "Done.", steps: [
                    FairyToolStep(tool: "container_action", title: "Restart redis", status: .done),
                ]),
            ]
        )
        let convB = FairyConversation(
            title: "Disk usage check",
            hostName: "This Mac",
            messages: [
                FairyChatMessage(role: .user, text: "How much space is reclaimable?"),
                FairyChatMessage(role: .assistant, text: "420 MB build cache."),
            ]
        )

        #expect(FairyHistoryStore.filter([convA, convB], query: "").count == 2)
        #expect(FairyHistoryStore.filter([convA, convB], query: "mini").map(\.id) == [convA.id])
        #expect(FairyHistoryStore.filter([convA, convB], query: "420 MB").map(\.id) == [convB.id])
        #expect(FairyHistoryStore.filter([convA, convB], query: "container_action").map(\.id) == [convA.id])

        #expect(FairyHistoryStore.continuationSummary(for: []) == nil)
        let recap = FairyHistoryStore.continuationSummary(for: [
            FairyChatMessage(role: .user, text: "First prompt"),
            FairyChatMessage(role: .error, text: "Transient error should be excluded"),
            FairyChatMessage(role: .assistant, text: "First reply", steps: [
                FairyToolStep(tool: "status", title: "Check status", status: .done),
            ]),
        ], maxChars: 500)
        #expect(recap?.contains("User: First prompt") == true)
        #expect(recap?.contains("Fairy [1 tool steps]: First reply") == true)
        #expect(recap?.contains("Transient error") == false)
    }

    @Test func networkRateSeriesHandlesVmCounterReset() {
        let samples = [
            StatSample(timestampMs: 1_000, cpuPercent: 1, memoryBytes: 100, memoryLimitBytes: 1000, netRxBytes: 1_000, netTxBytes: 500),
            StatSample(timestampMs: 3_000, cpuPercent: 2, memoryBytes: 100, memoryLimitBytes: 1000, netRxBytes: 1_600, netTxBytes: 800),
            // Non-increasing timestamp should be skipped cleanly.
            StatSample(timestampMs: 3_000, cpuPercent: 2, memoryBytes: 100, memoryLimitBytes: 1000, netRxBytes: 1_700, netTxBytes: 900),
            // Simulated VM restart resetting RX/TX counters below previous total.
            StatSample(timestampMs: 5_000, cpuPercent: 3, memoryBytes: 100, memoryLimitBytes: 1000, netRxBytes: 200, netTxBytes: 100),
        ]
        let rates = NetworkRate.series(samples)
        #expect(rates.count == 2)
        #expect(rates[0].received == 300)
        #expect(rates[0].sent == 150)
        #expect(rates[1].received == 100)
        #expect(rates[1].sent == 50)
    }
}

@Suite struct HealthcheckTests {
    @Test func wireDecodesHealthcheckDetailsAndEvents() throws {
        let json = #"""
        {"type":"containers","data":[{"id":"abc123def456","name":"db","image":"postgres:17",
        "status":"running","exit_code":null,"ports":[],"mounts":[],"ip_address":"192.168.64.2",
        "cpu_percent":1.5,"memory_bytes":67108864,"memory_limit_bytes":1073741824,
        "created_at":1700000000,"started_at":1700000005,"health":"healthy"}]}
        """#
        guard case let .containers(list) = try Wire.decoder.decode(EngineResponse.self, from: Data(json.utf8)) else {
            Issue.record("expected containers"); return
        }
        #expect(list.first?.health == .healthy)

        let detailsJSON = #"""
        {"type":"details","data":{"container":{"id":"abc123def456","name":"db","image":"postgres:17",
        "status":"running","exit_code":null,"ports":[],"mounts":[],"ip_address":"192.168.64.2",
        "cpu_percent":1.5,"memory_bytes":67108864,"memory_limit_bytes":1073741824,
        "created_at":1700000000,"started_at":1700000005,"health":"unhealthy"},
        "image_reference":"docker.io/library/postgres:17","image_digest":"sha256:abc","command":["postgres"],
        "env":[],"working_dir":"/","user":"postgres","hostname":"db","cpus":2,"memory_mib":1024,
        "kernel":"Linux 6.12","vm_dir":"/tmp/vm","restart":"no",
        "healthcheck":{"test":["CMD-SHELL","pg_isready"],"interval_secs":5,"timeout_secs":2,"start_period_secs":10,"retries":3},
        "health":{"status":"unhealthy","failing_streak":3,"log":[{"start":10,"end":11,"exit_code":1,"output":"refused"}]}}}
        """#
        guard case let .details(d) = try Wire.decoder.decode(EngineResponse.self, from: Data(detailsJSON.utf8)) else {
            Issue.record("expected details"); return
        }
        #expect(d.healthcheck?.commandDisplay == "pg_isready")
        #expect(d.healthcheck?.intervalSecs == 5)
        #expect(d.health?.status == .unhealthy)
        #expect(d.health?.failingStreak == 3)
        #expect(d.health?.log.first?.output == "refused")

        let evJSON = #"""
        {"type":"events","data":[
            {"seq":1,"timestamp_ms":1000,"kind":"healthy","container_id":"abc","container":"db","message":"db is healthy"},
            {"seq":2,"timestamp_ms":2000,"kind":"unhealthy","container_id":"abc","container":"db","message":"db is unhealthy"}
        ]}
        """#
        guard case let .events(evs) = try Wire.decoder.decode(EngineResponse.self, from: Data(evJSON.utf8)) else {
            Issue.record("expected events"); return
        }
        #expect(evs.map(\.kind) == [.healthy, .unhealthy])
        #expect(evs[0].isProblem == false && evs[1].isProblem == true)

        let spec = RunSpec(
            image: "alpine:3.22",
            healthcheck: Healthcheck(test: ["CMD-SHELL", "test -f /tmp/ok"], intervalSecs: 2, timeoutSecs: 1, startPeriodSecs: 0, retries: 2)
        )
        let encoded = String(decoding: try Wire.line(.run(spec)), as: UTF8.self)
        #expect(encoded.contains(#""interval_secs":2"#))
        #expect(encoded.contains(#""CMD-SHELL""#))
    }

    @Test func pauseAndUnpauseWireEventsAndDemoTransitions() async throws {
        let pauseWire = String(decoding: try Wire.line(.pause(id: "web")), as: UTF8.self)
        let unpauseWire = String(decoding: try Wire.line(.unpause(id: "web")), as: UTF8.self)
        #expect(pauseWire.contains(#""method":"pause""#) && pauseWire.contains(#""id":"web""#))
        #expect(unpauseWire.contains(#""method":"unpause""#) && unpauseWire.contains(#""id":"web""#))

        let evJSON = #"""
        {"type":"events","data":[
            {"seq":1,"timestamp_ms":1000,"kind":"paused","container_id":"abc","container":"web","message":"web paused"},
            {"seq":2,"timestamp_ms":2000,"kind":"unpaused","container_id":"abc","container":"web","message":"web unpaused"}
        ]}
        """#
        guard case let .events(evs) = try Wire.decoder.decode(EngineResponse.self, from: Data(evJSON.utf8)) else {
            Issue.record("expected events"); return
        }
        #expect(evs.map(\.kind) == [.paused, .unpaused])

        let engine = DemoEngine()
        let paused = try await engine.pause(id: "web")
        #expect(paused.isPaused && paused.isActive && !paused.isRunning)
        #expect(paused.statusText() == "Paused")
        await #expect(throws: EngineError.self) { try await engine.start(id: "web") }
        await #expect(throws: EngineError.self) { try await engine.remove(id: "web") }

        let allWhilePaused = await engine.containers()
        #expect(ContainerFilter.running.apply(allWhilePaused).contains { $0.name == "web" })
        #expect(!ContainerFilter.stopped.apply(allWhilePaused).contains { $0.name == "web" })

        let unpaused = try await engine.unpause(id: "web")
        #expect(unpaused.isRunning)
        _ = try await engine.pause(id: "web")
        let stopped = try await engine.stop(id: "web")
        #expect(stopped.status == .exited)
    }

    @Test func snapshotInfoDecodesPausedReasonBackCompat() throws {
        let plain = #"{"enabled":true,"total_slots":1,"in_use":0,"total_bytes":14159872,"max_slots_per_shape":8,"groups":[]}"#
        let decodedPlain = try Wire.decoder.decode(SnapshotInfo.self, from: Data(plain.utf8))
        #expect(decodedPlain.pausedReason == nil)

        let paused = #"{"enabled":true,"total_slots":1,"in_use":0,"total_bytes":14159872,"max_slots_per_shape":8,"groups":[],"paused_reason":"macOS console session is locked"}"#
        let decodedPaused = try Wire.decoder.decode(SnapshotInfo.self, from: Data(paused.utf8))
        #expect(decodedPaused.pausedReason == "macOS console session is locked")
    }

    @Test func streamingLogsCommitAndArchivesWireAndHelpers() async throws {
        let line = { (r: EngineRequest) in String(decoding: try Wire.line(r), as: UTF8.self) }

        // Legacy logs request shape must remain untouched.
        #expect(try line(.logs(id: "web", tail: 10)) == #"{"method":"logs","params":{"id":"web","tail":10}}"# + "\n")

        // Streaming logs with follow & time window.
        #expect(
            try line(.logs(id: "api", tail: 25, follow: true, sinceMs: 1_700_000_000_000, untilMs: 1_700_000_060_000))
                == #"{"method":"logs","params":{"follow":true,"id":"api","since_ms":1700000000000,"tail":25,"until_ms":1700000060000}}"# + "\n"
        )

        // Commit, SaveImage, LoadImage request wire format.
        let commitSpec = CommitSpec(
            container: "web",
            reference: "myrepo/web:v2",
            author: "Jane",
            message: "snapshot",
            changes: ["ENV FOO=bar", "CMD [\"sh\"]"]
        )
        #expect(
            try line(.commit(commitSpec))
                == #"{"method":"commit","params":{"author":"Jane","changes":["ENV FOO=bar","CMD [\"sh\"]"],"container":"web","message":"snapshot","reference":"myrepo/web:v2"}}"# + "\n"
        )
        let minCommit = CommitSpec(container: "c1")
        #expect(try line(.commit(minCommit)) == #"{"method":"commit","params":{"container":"c1","reference":""}}"# + "\n")
        #expect(try line(.saveImage(references: ["alpine:3.22", "app:1"])) == #"{"method":"save_image","params":{"references":["alpine:3.22","app:1"]}}"# + "\n")
        #expect(try line(.loadImage(size: 42_000)) == #"{"method":"load_image","params":{"size":42000}}"# + "\n")

        // Responses: log, log_end (with exit code and empty), image_archive.
        let logJSON = #"{"type":"log","data":{"seq":7,"timestamp_ms":1700000000123,"level":"info","message":"ready"}}"#
        guard case let .log(entry) = try Wire.decoder.decode(EngineResponse.self, from: Data(logJSON.utf8)) else {
            Issue.record("expected .log"); return
        }
        #expect(entry.seq == 7 && entry.timestampMs == 1_700_000_000_123 && entry.message == "ready")

        guard case let .logEnd(code) = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"log_end","data":{"exit_code":42}}"#.utf8)) else {
            Issue.record("expected .logEnd with code"); return
        }
        #expect(code == 42)

        guard case let .logEnd(nilCode) = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"log_end","data":{}}"#.utf8)) else {
            Issue.record("expected .logEnd without code"); return
        }
        #expect(nilCode == nil)

        guard case .imageArchive = try Wire.decoder.decode(EngineResponse.self, from: Data(#"{"type":"image_archive"}"#.utf8)) else {
            Issue.record("expected .imageArchive"); return
        }

        // Format.parseLogTimestamp & LogTimeWindow helpers.
        let fixedNow = Date(timeIntervalSince1970: 1_700_000_600)
        #expect(Format.parseLogTimestamp("5m", now: fixedNow) == 1_700_000_300_000)
        #expect(Format.parseLogTimestamp("30s", now: fixedNow) == 1_700_000_570_000)
        #expect(Format.parseLogTimestamp("1700000100") == 1_700_000_100_000)
        #expect(Format.parseLogTimestamp("1700000100123") == 1_700_000_100_123)
        #expect(Format.parseLogTimestamp("2026-11-22T09:00:00Z") != nil)
        #expect(Format.parseLogTimestamp("not-a-timestamp") == nil)
        #expect(LogTimeWindow.all.sinceMs(now: fixedNow) == nil)
        #expect(LogTimeWindow.fiveMinutes.sinceMs(now: fixedNow) == 1_700_000_300_000)
        #expect(LogTimeWindow.oneHour.sinceMs(now: fixedNow) == 1_699_997_000_000)

        // DemoEngine time window filter & commit rejection.
        let demo = DemoEngine()
        let allLogs = try await demo.logs(id: "api-gateway", tail: nil)
        #expect(allLogs.count > 2)
        let cutoff = allLogs[allLogs.count - 2].timestampMs
        let filtered = try await demo.logs(id: "api-gateway", tail: nil, sinceMs: cutoff, untilMs: cutoff)
        #expect(filtered.allSatisfy { $0.timestampMs == cutoff })
        await #expect(throws: EngineError.self) { _ = try await demo.commit(CommitSpec(container: "web", reference: "web:snap")) }
    }

    @Test func registryLoginsAndSecurityDetailsWireAndDemo() async throws {
        let line = { (r: EngineRequest) in String(decoding: try Wire.line(r), as: UTF8.self) }
        #expect(try line(.listLogins) == #"{"method":"list_logins"}"# + "\n")
        #expect(
            try line(.registryLogin(registry: "ghcr.io", username: "octocat", password: "secret"))
                == #"{"method":"registry_login","params":{"password":"secret","registry":"ghcr.io","username":"octocat"}}"# + "\n"
        )
        #expect(
            try line(.registryLogout(registry: "ghcr.io"))
                == #"{"method":"registry_logout","params":{"registry":"ghcr.io"}}"# + "\n"
        )

        guard case let .logins(list) = try Wire.decoder.decode(
            EngineResponse.self,
            from: Data(#"{"type":"logins","data":[{"registry":"docker.io","username":"alice"}]}"#.utf8)
        ) else {
            Issue.record("expected .logins"); return
        }
        #expect(list == [RegistryLoginInfo(registry: "docker.io", username: "alice")])

        guard case let .loggedIn(reg, user) = try Wire.decoder.decode(
            EngineResponse.self,
            from: Data(#"{"type":"logged_in","data":{"registry":"docker.io","username":"alice"}}"#.utf8)
        ) else {
            Issue.record("expected .loggedIn"); return
        }
        #expect(reg == "docker.io" && user == "alice")

        guard case let .loggedOut(outReg) = try Wire.decoder.decode(
            EngineResponse.self,
            from: Data(#"{"type":"logged_out","data":{"registry":"docker.io"}}"#.utf8)
        ) else {
            Issue.record("expected .loggedOut"); return
        }
        #expect(outReg == "docker.io")

        // ContainerDetails with security fields + backwards compatibility when omitted.
        let secJSON = #"{"container":{"id":"abc","name":"web","image":"alpine","status":"running","exit_code":null,"ports":[],"mounts":[],"ip_address":null,"cpu_percent":null,"memory_bytes":null,"memory_limit_bytes":null,"created_at":1,"started_at":2},"image_reference":"alpine","image_digest":"sha256:aa","command":["sh"],"env":[],"working_dir":"/","user":null,"hostname":"web","cpus":2,"memory_mib":512,"kernel":"6.12","vm_dir":"/tmp","read_only":true,"cap_add":["NET_BIND_SERVICE"],"cap_drop":["ALL"],"tmpfs":[{"target":"/tmp","options":"size=64m"}],"init":true}"#
        let decoded = try Wire.decoder.decode(ContainerDetails.self, from: Data(secJSON.utf8))
        #expect(decoded.readOnly == true)
        #expect(decoded.capAdd == ["NET_BIND_SERVICE"])
        #expect(decoded.capDrop == ["ALL"])
        #expect(decoded.tmpfs == [TmpfsMount(target: "/tmp", options: "size=64m")])
        #expect(decoded.usesInit)

        let plainJSON = #"{"container":{"id":"abc","name":"web","image":"alpine","status":"running","exit_code":null,"ports":[],"mounts":[],"ip_address":null,"cpu_percent":null,"memory_bytes":null,"memory_limit_bytes":null,"created_at":1,"started_at":2},"image_reference":"alpine","image_digest":"sha256:aa","command":["sh"],"env":[],"working_dir":"/","user":null,"hostname":"web","cpus":2,"memory_mib":512,"kernel":"6.12","vm_dir":"/tmp"}"#
        let decodedPlain = try Wire.decoder.decode(ContainerDetails.self, from: Data(plainJSON.utf8))
        #expect(decodedPlain.readOnly == nil && !decodedPlain.usesInit)

        // DemoEngine login/logout lifecycle.
        let demo = DemoEngine()
        #expect(await demo.listLogins().contains { $0.registry == "ghcr.io" })
        let added = try await demo.registryLogin(registry: "https://index.docker.io/v1/", username: "alice", password: "pw")
        #expect(added == RegistryLoginInfo(registry: "docker.io", username: "alice"))
        #expect(await demo.listLogins().contains { $0.registry == "docker.io" })
        #expect(try await demo.registryLogout(registry: "docker.io") == "docker.io")
        await #expect(throws: EngineError.self) { _ = try await demo.registryLogout(registry: "docker.io") }
    }

    @Test func semVerParsingAndComparison() throws {
        let v010 = try #require(SemVer("v0.1.0"))
        let v012 = try #require(SemVer("0.1.2"))
        let v0110 = try #require(SemVer("0.1.10"))
        #expect(v010 < v012)
        #expect(v012 < v0110)

        let short = try #require(SemVer("V1.2"))
        #expect(short == SemVer(major: 1, minor: 2, patch: 0))

        // Build metadata does not change precedence or equality.
        let withBuild = try #require(SemVer("v1.0.0+build.42"))
        let plain = try #require(SemVer("1.0.0"))
        #expect(withBuild == plain)
        #expect(!(withBuild < plain) && !(plain < withBuild))

        // Pre-release ordering (SemVer 2.0.0): alpha < beta.2 < rc.1 < stable
        let alpha = try #require(SemVer("1.0.0-alpha"))
        let beta2 = try #require(SemVer("1.0.0-beta.2"))
        let beta10 = try #require(SemVer("1.0.0-beta.10"))
        let rc1 = try #require(SemVer("1.0.0-rc.1"))
        #expect(alpha < beta2)
        #expect(beta2 < beta10)
        #expect(beta10 < rc1)
        #expect(rc1 < plain)

        #expect(SemVer("") == nil)
        #expect(SemVer("1") == nil)
        #expect(SemVer("v1.2.3-") == nil)
        #expect(SemVer("abc") == nil)
    }

    @Test @MainActor func updateCheckerGitHubReleaseFlows() async throws {
        let suiteName = "dev.graviton.tests.\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        final class MockHTTP: @unchecked Sendable {
            var calls = 0
            var status = 200
            var body = Data()

            func set(status: Int, json: String) {
                self.status = status
                self.body = Data(json.utf8)
            }
        }
        let mock = MockHTTP()
        let checker = UpdateChecker(
            currentVersion: SemVer(major: 0, minor: 1, patch: 0),
            feedURL: URL(string: "https://example.test/releases/latest")!,
            defaults: defaults,
            fetcher: { req in
                mock.calls += 1
                let resp = HTTPURLResponse(
                    url: try #require(req.url),
                    statusCode: mock.status,
                    httpVersion: "HTTP/1.1",
                    headerFields: nil
                )!
                return (mock.body, resp)
            },
            authTokenProvider: { nil },
            demoModeProvider: { false }
        )

        // 1. Auto-check defaults to opt-in (false) and skips background check.
        #expect(!checker.autoCheckEnabled)
        await checker.checkIfDue()
        #expect(mock.calls == 0)
        checker.autoCheckEnabled = true

        // 2. Private / unpublished 404 yields calm .unavailable (and throttles next due check).
        mock.set(status: 404, json: #"{"message":"Not Found"}"#)
        await checker.checkIfDue()
        #expect(mock.calls == 1)
        guard case .unavailable = checker.state else {
            Issue.record("expected .unavailable on 404"); return
        }
        await checker.checkIfDue()
        #expect(mock.calls == 1) // Throttled within 6h window.

        // 3. Same version yields .upToDate.
        mock.set(status: 200, json: #"{"tag_name":"v0.1.0","name":"v0.1.0","html_url":"https://github.com/razeenwasif/Graviton/releases/tag/v0.1.0","draft":false,"prerelease":false,"assets":[]}"#)
        await checker.checkNow(userInitiated: true)
        #expect(checker.state == .upToDate(current: SemVer(major: 0, minor: 1, patch: 0), latest: SemVer(major: 0, minor: 1, patch: 0)))
        #expect(!checker.hasVisibleUpdate)

        // 4. Draft or prerelease is ignored.
        mock.set(status: 200, json: #"{"tag_name":"v0.9.0-beta.1","name":"Beta","html_url":"https://github.com/razeenwasif/Graviton/releases/tag/v0.9.0-beta.1","draft":false,"prerelease":true,"assets":[]}"#)
        await checker.checkNow(userInitiated: true)
        #expect(!checker.hasVisibleUpdate)

        // 5. Newer release selects .dmg asset and honors skip / clearSkipped.
        mock.set(status: 200, json: #"{"tag_name":"v0.2.0","name":"Graviton v0.2.0","body":"Faster snapshots","html_url":"https://github.com/razeenwasif/Graviton/releases/tag/v0.2.0","published_at":"2026-12-01T00:00:00Z","draft":false,"prerelease":false,"assets":[{"name":"Graviton-0.2.0.zip","browser_download_url":"https://github.com/razeenwasif/Graviton/releases/download/v0.2.0/Graviton-0.2.0.zip"},{"name":"Graviton-0.2.0.dmg","browser_download_url":"https://github.com/razeenwasif/Graviton/releases/download/v0.2.0/Graviton-0.2.0.dmg"}]}"#)
        await checker.checkNow(userInitiated: true)
        guard case let .available(release) = checker.state else {
            Issue.record("expected .available for v0.2.0"); return
        }
        #expect(release.version == SemVer(major: 0, minor: 2, patch: 0))
        #expect(release.dmgDownloadURL?.lastPathComponent == "Graviton-0.2.0.dmg")
        #expect(release.preferredDownloadURL == release.dmgDownloadURL)
        #expect(checker.hasVisibleUpdate)

        checker.skip(release)
        #expect(!checker.hasVisibleUpdate)
        checker.clearSkipped()
        #expect(checker.hasVisibleUpdate)

        // 6. Rate-limit 403/429 yields calm .unavailable.
        mock.set(status: 403, json: #"{"message":"API rate limit exceeded"}"#)
        await checker.checkNow(userInitiated: true)
        guard case .unavailable = checker.state else {
            Issue.record("expected .unavailable on 403"); return
        }

        // 7. Rejects non-HTTPS feed URLs and never sends token to non-GitHub hosts.
        let httpChecker = UpdateChecker(
            currentVersion: SemVer(major: 0, minor: 1, patch: 0),
            feedURL: URL(string: "http://example.test/releases/latest")!,
            defaults: defaults,
            fetcher: { _ in Issue.record("HTTP URL must not be fetched"); throw URLError(.cancelled) },
            authTokenProvider: { "secret-tok" },
            demoModeProvider: { false }
        )
        await httpChecker.checkNow(userInitiated: true)
        guard case .unavailable = httpChecker.state else {
            Issue.record("expected .unavailable on http:// feed"); return
        }
    }

    @Test func runSpecSecurityFieldsAndTmpfsParsing() throws {
        #expect(TmpfsMount.parse("/tmp") == TmpfsMount(target: "/tmp", options: ""))
        #expect(TmpfsMount.parse(" /run:rw,noexec,size=64m ") == TmpfsMount(target: "/run", options: "rw,noexec,size=64m"))
        #expect(TmpfsMount.parse("") == nil)
        #expect(TmpfsMount.parse("relative/path") == nil)

        let raw = "\n  /tmp:rw,noexec,size=64m \n /var/cache \n"
        #expect(TmpfsMount.invalidLine(in: raw) == nil)
        #expect(TmpfsMount.parseLines(raw) == [
            TmpfsMount(target: "/tmp", options: "rw,noexec,size=64m"),
            TmpfsMount(target: "/var/cache", options: ""),
        ])
        #expect(TmpfsMount.invalidLine(in: "/tmp\nbadpath:size=10m\n") == "badpath:size=10m")

        // Plain RunSpec omits unset security fields so wire compatibility holds.
        let plainLine = String(decoding: try Wire.line(.run(RunSpec(image: "alpine:3.22"))), as: UTF8.self)
        #expect(!plainLine.contains("read_only") && !plainLine.contains("cap_drop") && !plainLine.contains("tmpfs"))

        let secSpec = RunSpec(
            image: "alpine:3.22",
            name: "locked",
            readOnly: true,
            capAdd: ["NET_BIND_SERVICE"],
            capDrop: ["ALL"],
            tmpfs: [TmpfsMount(target: "/tmp", options: "rw,noexec,size=64m")],
            initProcess: true
        )
        let secLine = String(decoding: try Wire.line(.run(secSpec)), as: UTF8.self)
        #expect(secLine.contains(#""read_only":true"#))
        #expect(secLine.contains(#""init":true"#))
        #expect(secLine.contains(#""cap_drop":["ALL"]"#))
        #expect(secLine.contains(#""cap_add":["NET_BIND_SERVICE"]"#))
        #expect(secLine.contains(#""target":"/tmp""#) && secLine.contains(#""options":"rw,noexec,size=64m""#))
    }

    @Test func userNetworkWireAndDemoLifecycle() async throws {
        // 1. Request wire encoding (snake_case + CodingKeys override `"internal"`).
        let createLine = String(decoding: try Wire.line(.createNetwork(name: "app-net", internal: true, labels: ["env": "dev"])), as: UTF8.self)
        #expect(createLine.contains(#""method":"create_network""#))
        #expect(createLine.contains(#""name":"app-net""#))
        #expect(createLine.contains(#""internal":true"#))
        #expect(createLine.contains(#""env":"dev""#))

        for req: EngineRequest in [
            .listNetworks,
            .removeNetwork(name: "app-net"),
            .connectNetwork(network: "app-net", container: "web"),
            .disconnectNetwork(network: "app-net", container: "web"),
        ] {
            let encoded = try Wire.line(req)
            #expect(!encoded.isEmpty)
        }

        // 2. RunSpec omits networks when nil, encodes when present.
        let plainRun = String(decoding: try Wire.line(.run(RunSpec(image: "alpine:3.22"))), as: UTF8.self)
        #expect(!plainRun.contains("networks"))
        let netRun = String(decoding: try Wire.line(.run(RunSpec(image: "alpine:3.22", networks: ["app-net", "db-net"]))), as: UTF8.self)
        #expect(netRun.contains(#""networks":["app-net","db-net"]"#))

        // 3. Response decoding + backward compatibility when `networks` is omitted.
        let listJson = #"{"type":"networks","data":[{"id":"net1","name":"app-net","driver":"bridge","internal":true,"builtin":false,"created_at":1700000000,"labels":{"env":"dev"},"containers":["web"]}]}"#
        guard case let .networks(decodedNets) = try Wire.decoder.decode(EngineResponse.self, from: Data(listJson.utf8)) else {
            Issue.record("expected .networks"); return
        }
        #expect(decodedNets.count == 1)
        #expect(decodedNets[0].name == "app-net")
        #expect(decodedNets[0].internalOnly == true)
        #expect(decodedNets[0].containers == ["web"])

        let createdJson = #"{"type":"network_created","data":{"id":"net2","name":"db-net","driver":"bridge","internal":false,"builtin":false,"created_at":1700000001}}"#
        guard case let .networkCreated(created) = try Wire.decoder.decode(EngineResponse.self, from: Data(createdJson.utf8)) else {
            Issue.record("expected .networkCreated"); return
        }
        #expect(created.name == "db-net")

        let removedJson = #"{"type":"network_removed","data":{"name":"db-net"}}"#
        guard case let .networkRemoved(remName) = try Wire.decoder.decode(EngineResponse.self, from: Data(removedJson.utf8)) else {
            Issue.record("expected .networkRemoved"); return
        }
        #expect(remName == "db-net")

        let updatedJson = #"{"type":"network_updated","data":{"network":"app-net","container":"web","connected":true}}"#
        guard case let .networkUpdated(netName, ctr, conn) = try Wire.decoder.decode(EngineResponse.self, from: Data(updatedJson.utf8)) else {
            Issue.record("expected .networkUpdated"); return
        }
        #expect(netName == "app-net" && ctr == "web" && conn)

        let legacyNetJson = #"{"name":"graviton","driver":"L2 switch + NAT","subnet":"192.168.64.0/24","gateway":"192.168.64.1","dns_server":"127.0.0.11:53","dns_domain":"graviton","connected_ports":1,"learned_macs":1,"frames_forwarded":10,"bytes_forwarded":200,"endpoints":[{"container_id":"abc","container_name":"web","image":"nginx","status":"running","ip_address":"192.168.64.2","dns_names":["web"],"dns_enabled":true,"ports":[]}]}"#
        let legacyInfo = try Wire.decoder.decode(NetworkInfo.self, from: Data(legacyNetJson.utf8))
        #expect(legacyInfo.networks.isEmpty)
        #expect(legacyInfo.endpoints.first?.attachedNetworks == ["graviton"])

        // 4. DemoEngine network CRUD, validation, connect/disconnect, inspect & networkInfo sync.
        let demo = DemoEngine()
        let createdNet = try await demo.createNetwork(name: "qa-net", internalOnly: true, labels: ["tier": "qa"])
        #expect(createdNet.name == "qa-net" && createdNet.internalOnly)

        do {
            _ = try await demo.createNetwork(name: "qa-net")
            Issue.record("duplicate network should fail")
        } catch {}

        let attached = try await demo.connectNetwork(network: "qa-net", container: "web")
        #expect(attached)
        let webInspect = try await demo.inspect(id: "web")
        #expect(webInspect.attachedNetworks.contains("qa-net"))
        let netSnapshot = await demo.networkInfo()
        #expect(netSnapshot.networks.contains { $0.name == "qa-net" && $0.containers.contains("web") })
        #expect(netSnapshot.endpoints.first { $0.containerName == "web" }?.attachedNetworks.contains("qa-net") == true)

        do {
            _ = try await demo.removeNetwork(name: "qa-net")
            Issue.record("removing network with active container should fail")
        } catch {}

        do {
            _ = try await demo.removeNetwork(name: "graviton")
            Issue.record("removing built-in graviton network should fail")
        } catch {}

        let detached = try await demo.disconnectNetwork(network: "qa-net", container: "web")
        #expect(!detached)
        let removed = try await demo.removeNetwork(name: "qa-net")
        #expect(removed == "qa-net")
        #expect(await demo.networks().contains { $0.name == "qa-net" } == false)
    }

    @Test func shmSizeAndUlimitWireRoundTrip() throws {
        let plainLine = String(decoding: try Wire.line(.run(RunSpec(image: "alpine:latest"))), as: UTF8.self)
        #expect(!plainLine.contains("shm_size_bytes") && !plainLine.contains("ulimits"))

        let spec = RunSpec(
            image: "alpine:latest",
            shmSizeBytes: 134_217_728,
            ulimits: [
                UlimitSpec(name: "nofile", soft: 2048, hard: 4096),
                UlimitSpec(name: "core", soft: -1, hard: -1),
            ]
        )
        let encoded = String(decoding: try Wire.line(.run(spec)), as: UTF8.self)
        #expect(encoded.contains(#""shm_size_bytes":134217728"#))
        #expect(encoded.contains(#""hard":4096,"name":"nofile","soft":2048"#))
        #expect(UlimitSpec(name: "nofile", soft: 2048, hard: 4096).displayLabel == "nofile=2048:4096")
        #expect(UlimitSpec(name: "core", soft: -1, hard: -1).displayLabel == "core=unlimited")

        let detailsJson = #"{"container":{"id":"c1","name":"web","image":"alpine","status":"running","ports":[],"mounts":[],"created_at":1700000000},"image_reference":"alpine:latest","image_digest":"sha256:aaa","command":["sh"],"env":[],"working_dir":"/","hostname":"web","cpus":2,"memory_mib":1024,"kernel":"Linux","vm_dir":"/tmp/vm","shm_size_bytes":134217728,"ulimits":[{"name":"nofile","soft":2048,"hard":4096}]}"#
        let details = try Wire.decoder.decode(ContainerDetails.self, from: Data(detailsJson.utf8))
        #expect(details.shmSizeBytes == 134_217_728)
        #expect(details.ulimits == [UlimitSpec(name: "nofile", soft: 2048, hard: 4096)])
    }
}
