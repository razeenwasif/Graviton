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
