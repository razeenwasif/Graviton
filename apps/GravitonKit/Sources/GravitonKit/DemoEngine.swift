import Foundation

/// In-process engine with the same sample data as `Store::demo()` in Rust.
/// CPU and memory drift a little and api-gateway keeps logging, so the UI
/// looks alive without a daemon.
public actor DemoEngine: EngineService {
    private var list: [Container]
    private var logLines: [String: [LogLine]]
    private var imageList: [ImageInfo]
    private var volumeList: [VolumeInfo]
    /// Pretend build cache, for the Clean Up sheet.
    private var cacheSteps: UInt32 = 23
    private var cacheBytes: UInt64 = 412 << 20
    private var tick = 0

    public init() {
        let now = UInt64(Date.now.timeIntervalSince1970)
        let mib: UInt64 = 1024 * 1024
        func tcp(_ h: UInt16, _ c: UInt16) -> PortMapping { PortMapping(hostPort: h, containerPort: c, protocol: .tcp) }
        func make(_ id: String, _ name: String, _ image: String, up: (UInt64, Double, UInt64)?, exit: Int32? = nil,
                  ports: [PortMapping] = [], mounts: [Mount] = [], ip: String? = nil,
                  project: String? = nil, service: String? = nil) -> Container {
            Container(id: id, name: name, image: image, status: up == nil ? .exited : .running, exitCode: exit,
                      ports: ports, mounts: mounts, ipAddress: ip, cpuPercent: up?.1, memoryBytes: up.map { $0.2 * mib },
                      memoryLimitBytes: 1024 * mib, createdAt: now - 4 * 3600, startedAt: up.map { now - $0.0 },
                      project: project, service: service)
        }
        list = [
            make("a3f9c2e81b07", "api-gateway", "storefront/api:0.4.2", up: (11_520, 12.4, 412), ports: [tcp(8080, 80)],
                 mounts: [Mount(source: "~/code/storefront", target: "/app", readOnly: false)], ip: "192.168.64.4",
                 project: "storefront", service: "api"),
            make("5d10e7b3c4aa", "postgres", "postgres:17-alpine", up: (11_520, 3.1, 286), ports: [tcp(5432, 5432)],
                 mounts: [Mount(source: "pgdata", target: "/var/lib/postgresql/data", readOnly: false)], ip: "192.168.64.2",
                 project: "storefront", service: "postgres"),
            make("9be2041f6d33", "redis-cache", "redis:7.4", up: (11_520, 0.6, 24), ports: [tcp(6379, 6379)], ip: "192.168.64.3",
                 project: "storefront", service: "redis"),
            make("c71a88e09f12", "web", "node:22-slim", up: (2_460, 7.8, 538), ports: [tcp(3000, 3000)],
                 mounts: [Mount(source: "~/code/storefront/web", target: "/srv", readOnly: false)], ip: "192.168.64.5",
                 project: "storefront", service: "web"),
            make("e4c5d6f7a8b9", "worker", "storefront/worker:0.4.2", up: nil, exit: 0,
                 project: "storefront", service: "worker"),
            make("1f2e3d4c5b6a", "minio", "minio/minio:latest", up: nil, exit: 137, ports: [tcp(9000, 9000), tcp(9001, 9001)]),
        ]

        func img(_ repo: String, _ tag: String, _ mb: UInt64, _ arch: String, _ used: UInt32) -> ImageInfo {
            let qualified = repo.contains(".") ? repo : (repo.contains("/") ? "docker.io/\(repo)" : "docker.io/library/\(repo)")
            return ImageInfo(reference: "\(qualified):\(tag)", repository: repo, tag: tag, digest: "sha256:" + String(repo.utf8.reduce(UInt64(5381)) { ($0 &* 33) &+ UInt64($1) } & 0xffff_ffff_ffff, radix: 16),
                             sizeBytes: mb * mib, architecture: arch, usedBy: used)
        }
        imageList = [
            img("storefront/api", "0.4.2", 84, "arm64", 1), img("storefront/worker", "0.4.2", 79, "arm64", 1),
            img("postgres", "17-alpine", 274, "arm64", 1), img("redis", "7.4", 117, "arm64", 1),
            img("node", "22-slim", 221, "arm64", 1), img("minio/minio", "latest", 162, "arm64", 1),
            img("python", "3.13-slim", 146, "arm64", 0), img("ubuntu", "24.04", 101, "amd64", 0),
            img("ghcr.io/astral-sh/uv", "0.9", 38, "arm64", 0),
        ]

        volumeList = [
            VolumeInfo(name: "pgdata", path: "~/Library/Application Support/Graviton/volumes/pgdata", sizeBytes: 184 * mib, fileCount: 412, createdAt: now - 4 * 3600),
            VolumeInfo(name: "redis-snapshots", path: "~/Library/Application Support/Graviton/volumes/redis-snapshots", sizeBytes: 18 * mib, fileCount: 14, createdAt: now - 26 * 3600),
            VolumeInfo(name: "model-cache", path: "~/Library/Application Support/Graviton/volumes/model-cache", sizeBytes: 642 * mib, fileCount: 83, createdAt: now - 72 * 3600),
        ]

        let t0 = (now - 30) * 1000
        let seed: [(LogLevel, String)] = [
            (.info, "graviton-agent: process started (pid 1 → 27)"),
            (.info, "loading config from /app/config.toml"),
            (.info, "connected to postgres://postgres:5432/storefront"),
            (.info, "connected to redis://redis-cache:6379"),
            (.info, "listening on 0.0.0.0:80"),
            (.info, "GET  /v1/health 200 0.4ms"),
            (.info, "GET  /v1/products?page=1 200 4.1ms"),
            (.info, "GET  /v1/products/8812 200 2.7ms"),
            (.info, "POST /v1/cart 201 9.3ms"),
            (.info, "GET  /v1/cart 200 7.0ms"),
            (.warn, "slow query: SELECT * FROM inventory … (182ms)"),
            (.info, "GET  /v1/products?page=2 200 3.8ms"),
            (.error, "POST /v1/checkout 502 upstream payments timed out after 5000ms"),
            (.info, "retrying payments request (1/3)"),
            (.info, "POST /v1/checkout 200 548ms"),
            (.info, "GET  /v1/orders/30117 200 3.2ms"),
            (.info, "GET  /v1/health 200 0.3ms"),
            (.info, "GET  /v1/products?q=lamp 200 5.6ms"),
        ]
        logLines = ["a3f9c2e81b07": seed.enumerated().map { i, entry in
            LogLine(seq: 1267 + UInt64(i), timestampMs: t0 + UInt64(i) * 1350, level: entry.0, message: entry.1)
        }]
    }

    public func ping() -> UInt32 { 2 }

    public func engineInfo() -> EngineInfo {
        let used = list.compactMap(\.memoryBytes).reduce(0, +) + 1_900 * 1024 * 1024
        return EngineInfo(version: "0.1.0", protocol: 2, kernel: "Linux 6.12 (demo)", arch: "arm64", cpus: 4,
                          memoryBytes: 8 << 30, memoryUsedBytes: used, diskBytes: 64 << 30, diskUsedBytes: 6963 << 20,
                          rosetta: true, hostMemoryUsedBytes: min(8 << 30, (2_800 << 20) + used))
    }

    public func containers() -> [Container] {
        tick += 1
        for i in list.indices where list[i].isRunning {
            let cpu = list[i].cpuPercent ?? 1
            list[i].cpuPercent = max(0.1, min(95, cpu + Double.random(in: -1.5...1.5) * max(cpu, 1) / 6))
            if let mem = list[i].memoryBytes {
                let delta = Int64.random(in: -2...3) * 1024 * 1024
                list[i].memoryBytes = UInt64(max(Int64(8 << 20), Int64(mem) + delta))
            }
        }
        appendTraffic()
        return list
    }

    public func images() -> [ImageInfo] { imageList }

    public func volumes() -> [VolumeInfo] {
        volumeList.map { v in
            var copy = v
            copy.usedBy = list.flatMap { c in
                c.mounts.filter { $0.source == v.name }.map { m in
                    VolumeMountRef(containerId: c.id, containerName: c.name, target: m.target, readOnly: m.readOnly, running: c.isRunning)
                }
            }
            return copy
        }.sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    }

    public func createVolume(name: String) throws -> VolumeInfo {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard let first = trimmed.first, first.isASCII && (first.isLetter || first.isNumber),
              trimmed.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || "_.-".contains($0)) }) else {
            throw EngineError.daemon("`\(trimmed)` isn't a valid volume name (start with a letter or digit, then letters, digits, _ . -)")
        }
        guard !volumeList.contains(where: { $0.name == trimmed }) else {
            throw EngineError.daemon("volume \(trimmed) already exists")
        }
        let vol = VolumeInfo(name: trimmed, path: "~/Library/Application Support/Graviton/volumes/\(trimmed)",
                             sizeBytes: 0, fileCount: 0, createdAt: UInt64(Date.now.timeIntervalSince1970))
        volumeList.append(vol)
        return vol
    }

    public func removeVolume(name: String) throws -> UInt64 {
        guard let i = volumeList.firstIndex(where: { $0.name == name }) else {
            throw EngineError.daemon("no volume \(name) here")
        }
        let users = list.filter { $0.mounts.contains { $0.source == name } }.map(\.name)
        guard users.isEmpty else {
            throw EngineError.daemon("volume \(name) is used by \(users.joined(separator: ", ")): remove \(users.count == 1 ? "it" : "them") first")
        }
        return volumeList.remove(at: i).sizeBytes
    }

    public func networkInfo() -> NetworkInfo {
        var info = NetworkInfo.synthesized(from: list)
        info.framesForwarded = UInt64(1840 + tick * 14)
        info.bytesForwarded = UInt64(4_810_000 + tick * 6120)
        return info
    }

    public func logs(id: String, tail: Int?) throws -> [LogLine] {
        let c = try resolve(id)
        let lines = logLines[list[c].id] ?? []
        guard let tail else { return lines }
        return Array(lines.suffix(tail))
    }

    public func start(id: String) throws -> Container {
        let i = try resolve(id)
        guard !list[i].isRunning else { throw EngineError.daemon("container \(list[i].name) is already running") }
        list[i].status = .running
        list[i].exitCode = nil
        list[i].startedAt = UInt64(Date.now.timeIntervalSince1970)
        list[i].cpuPercent = 2
        list[i].memoryBytes = 48 << 20
        return list[i]
    }

    public func stop(id: String) throws -> Container {
        let i = try resolve(id)
        guard list[i].isRunning else { throw EngineError.daemon("container \(list[i].name) is already stopped") }
        list[i].status = .exited
        list[i].exitCode = 0
        list[i].startedAt = nil
        list[i].cpuPercent = nil
        list[i].memoryBytes = nil
        return list[i]
    }

    public func restart(id: String) throws -> Container {
        if try list[resolve(id)].isRunning { _ = try stop(id: id) }
        return try start(id: id)
    }

    public func pull(reference: String) throws -> ImageInfo {
        throw EngineError.daemon("Pulling images needs gravitond. Start it with scripts/dev-gravitond.sh.")
    }

    public func run(_ spec: RunSpec) throws -> Container {
        throw EngineError.daemon("Running containers needs gravitond. Start it with scripts/dev-gravitond.sh.")
    }

    public func remove(id: String) throws -> String {
        let i = try resolve(id)
        guard !list[i].isRunning else { throw EngineError.daemon("container \(list[i].name) is running; stop it first") }
        let removed = list.remove(at: i)
        logLines[removed.id] = nil
        return removed.id
    }

    // MARK: Disk space

    private func inUse(_ image: ImageInfo) -> Bool {
        list.contains { "\($0.image)" == "\(image.repository):\(image.tag)" || image.reference.hasSuffix($0.image) } || image.usedBy > 0
    }

    public func diskUsage() -> DiskUsage {
        let unused = imageList.filter { !inUse($0) }
        let stopped = list.filter { !$0.isRunning }
        let disk: (Container) -> UInt64 = { c in (c.memoryBytes ?? 180 << 20) / 2 + (96 << 20) }
        return DiskUsage(images: UInt32(imageList.count), unusedImages: UInt32(unused.count),
                         blobsBytes: imageList.map(\.sizeBytes).reduce(0, +) + cacheBytes,
                         unusedImagesBytes: unused.map(\.sizeBytes).reduce(0, +),
                         containers: UInt32(list.count), stoppedContainers: UInt32(stopped.count),
                         containerDisksBytes: list.map(disk).reduce(0, +), stoppedDisksBytes: stopped.map(disk).reduce(0, +),
                         buildCacheSteps: cacheSteps, buildCacheBytes: cacheBytes, unreferencedBytes: cacheSteps > 0 ? 37 << 20 : 0)
    }

    public func removeImage(reference: String) throws -> UInt64 {
        guard let i = imageList.firstIndex(where: { $0.reference == reference || "\($0.repository):\($0.tag)" == reference }) else {
            throw EngineError.daemon("no image \(reference) here")
        }
        guard !inUse(imageList[i]) else { throw EngineError.daemon("\(reference) is used by a container: remove it first") }
        return imageList.remove(at: i).sizeBytes
    }

    public func prune(_ spec: PruneSpec) -> PruneReport {
        let before = diskUsage()
        var report = PruneReport(containers: [], images: [], buildCacheSteps: cacheSteps, blobs: cacheSteps > 0 ? 31 : 0,
                                 freedBytes: before.alwaysReclaimable, dryRun: spec.dryRun)
        if spec.containers {
            report.containers = list.filter { !$0.isRunning }.map(\.name)
            report.freedBytes += before.stoppedDisksBytes
            if !spec.dryRun { list.removeAll { !$0.isRunning } }
        }
        if spec.images {
            let unused = imageList.filter { !inUse($0) }
            report.images = unused.map(\.reference)
            report.freedBytes += unused.map(\.sizeBytes).reduce(0, +)
            if !spec.dryRun { imageList.removeAll { !inUse($0) } }
        }
        if !spec.dryRun {
            cacheSteps = 0
            cacheBytes = 0
        }
        return report
    }

    // MARK: Stats, files, details and terminals (made up, but plausible)

    private var statHistory: [String: [StatSample]] = [:]

    public func stats(id: String) throws -> ContainerStats {
        let c = list[try resolve(id)]
        guard c.isRunning else { return ContainerStats(id: c.id, cpus: 2, intervalSecs: 2, samples: statHistory[c.id] ?? [], processes: []) }
        var samples = statHistory[c.id] ?? []
        let now = UInt64(Date.now.timeIntervalSince1970 * 1000)
        if samples.isEmpty {
            // Backfill five minutes so the charts have something to show.
            var cpu = c.cpuPercent ?? 5, mem = Double(c.memoryBytes ?? 64 << 20), rx = 48_000_000.0, tx = 9_000_000.0
            for i in 0..<150 {
                cpu = max(0.2, min(180, cpu + Double.random(in: -1...1) * max(cpu, 1) / 5))
                mem = max(16e6, mem + Double.random(in: -2...2.2) * 1e6)
                rx += Double.random(in: 4e3...90e3); tx += Double.random(in: 1e3...30e3)
                samples.append(StatSample(timestampMs: now - UInt64(149 - i) * 2000, cpuPercent: cpu, memoryBytes: UInt64(mem),
                                          memoryLimitBytes: c.memoryLimitBytes ?? 1 << 30, netRxBytes: UInt64(rx), netTxBytes: UInt64(tx)))
            }
        } else if let last = samples.last, now - last.timestampMs >= 2000 {
            samples.append(StatSample(timestampMs: now, cpuPercent: c.cpuPercent ?? last.cpuPercent, memoryBytes: c.memoryBytes ?? last.memoryBytes,
                                      memoryLimitBytes: last.memoryLimitBytes, netRxBytes: last.netRxBytes + UInt64.random(in: 4_000...90_000),
                                      netTxBytes: last.netTxBytes + UInt64.random(in: 1_000...30_000)))
            samples = Array(samples.suffix(150))
        }
        statHistory[c.id] = samples
        let mib: UInt64 = 1 << 20
        let main: (String, UInt64) = switch c.name {
        case "postgres": ("postgres -c shared_buffers=128MB", 96)
        case "redis-cache": ("redis-server *:6379", 9)
        case "web": ("node /srv/server.js", 182)
        default: ("/app/bin/api --config /app/config.toml", 148)
        }
        let processes = [
            ContainerProcess(pid: 412, ppid: 1, uid: 0, state: "S", command: main.0, rssBytes: main.1 * mib, cpuMs: 1_843_000),
            ContainerProcess(pid: 431, ppid: 412, uid: 0, state: "S", command: main.0.split(separator: " ")[0] + " (worker)", rssBytes: main.1 / 3 * mib, cpuMs: 402_000),
            ContainerProcess(pid: 433, ppid: 412, uid: 0, state: "R", command: main.0.split(separator: " ")[0] + " (worker)", rssBytes: main.1 / 3 * mib, cpuMs: 397_000),
        ]
        return ContainerStats(id: c.id, cpus: 2, intervalSecs: 2, samples: samples, processes: processes)
    }

    public func listFiles(id: String, path: String) throws -> DirectoryListing {
        let c = list[try resolve(id)]
        guard c.isRunning else { throw EngineError.daemon("container \(c.name) isn't running") }
        let t = Int64(Date.now.timeIntervalSince1970) - 86_400 * 3
        func d(_ n: String) -> FileEntry { FileEntry(name: n, kind: .directory, size: 4096, mode: 0o755, uid: 0, gid: 0, modified: t, linkTarget: nil) }
        func f(_ n: String, _ size: UInt64, _ mode: UInt32 = 0o644) -> FileEntry { FileEntry(name: n, kind: .file, size: size, mode: mode, uid: 0, gid: 0, modified: t + 3600, linkTarget: nil) }
        func l(_ n: String, _ to: String) -> FileEntry { FileEntry(name: n, kind: .symlink, size: UInt64(to.count), mode: 0o777, uid: 0, gid: 0, modified: t, linkTarget: to) }
        let tree: [String: [FileEntry]] = [
            "/": [d("app"), d("bin"), d("dev"), d("etc"), d("home"), d("lib"), d("proc"), d("root"), d("sbin"), d("tmp"), d("usr"), d("var")],
            "/app": [d("bin"), d("migrations"), f("config.toml", 1_284), f("README.md", 2_310), f("VERSION", 6)],
            "/app/bin": [f("api", 18_874_368, 0o755), f("healthcheck", 1_203_712, 0o755)],
            "/app/migrations": [f("0001_init.sql", 4_120), f("0002_inventory.sql", 1_877), f("0003_orders.sql", 2_990)],
            "/etc": [d("apk"), d("ssl"), f("group", 682), f("hostname", UInt64(c.name.count + 1)), f("hosts", 120), f("os-release", 191), f("passwd", 1_172), f("resolv.conf", 28), l("mtab", "../proc/mounts")],
            "/bin": [f("busybox", 808_712, 0o755), l("sh", "/bin/busybox"), l("ls", "/bin/busybox"), l("cat", "/bin/busybox")],
        ]
        let key = path == "" ? "/" : path
        return DirectoryListing(path: key, entries: tree[key] ?? [])
    }

    public func readFile(id: String, path: String) throws -> FileContents {
        let c = list[try resolve(id)]
        let files: [String: String] = [
            "/app/config.toml": "[server]\nlisten = \"0.0.0.0:80\"\nworkers = 4\n\n[database]\nurl = \"postgres://postgres:5432/storefront\"\npool = 16\n\n[cache]\nurl = \"redis://redis-cache:6379\"\nttl_secs = 300\n",
            "/app/README.md": "# storefront api\n\nServes /v1/products, /v1/cart and /v1/checkout.\n",
            "/app/VERSION": "0.4.2\n",
            "/etc/hostname": c.name + "\n",
            "/etc/hosts": "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n\(c.ipAddress ?? "127.0.1.1")\t\(c.name)\n",
            "/etc/os-release": "NAME=\"Alpine Linux\"\nID=alpine\nVERSION_ID=3.22.2\nPRETTY_NAME=\"Alpine Linux v3.22\"\n",
            "/etc/resolv.conf": "nameserver 192.168.64.1\n",
            "/app/migrations/0001_init.sql": "CREATE TABLE products (\n  id bigserial PRIMARY KEY,\n  name text NOT NULL,\n  price_cents integer NOT NULL\n);\n",
        ]
        if let text = files[path] {
            let data = Array(text.utf8)
            return FileContents(path: path, size: UInt64(data.count), data: data, truncated: false)
        }
        if path.hasPrefix("/app/bin/") || path == "/bin/busybox" {
            let data: [UInt8] = [0x7f, 0x45, 0x4c, 0x46, 2, 1, 1, 0] + Array(repeating: 0, count: 248)
            return FileContents(path: path, size: 18_874_368, data: data, truncated: true)
        }
        throw EngineError.daemon("\(path): No such file or directory")
    }

    public func inspect(id: String) throws -> ContainerDetails {
        let c = list[try resolve(id)]
        let command: [String] = switch c.name {
        case "postgres": ["docker-entrypoint.sh", "postgres"]
        case "redis-cache": ["docker-entrypoint.sh", "redis-server"]
        case "web": ["node", "/srv/server.js"]
        case "minio": ["minio", "server", "/data", "--console-address", ":9001"]
        default: ["/app/bin/api", "--config", "/app/config.toml"]
        }
        let image = imageList.first { $0.reference.hasSuffix(c.image) || "\($0.repository):\($0.tag)" == c.image }
        let restart: RestartPolicy = switch c.name {
        case "postgres", "redis-cache", "api-gateway": .always
        case "web": .onFailure
        default: .no
        }
        var labels: [String: String] = [:]
        if let project = c.project { labels["com.docker.compose.project"] = project }
        if let service = c.service { labels["com.docker.compose.service"] = service }
        return ContainerDetails(container: c, imageReference: image?.reference ?? c.image, imageDigest: image?.digest ?? "sha256:…",
                                command: command, env: ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "APP_ENV=development", "LOG_LEVEL=info"],
                                workingDir: c.name == "web" ? "/srv" : "/app", user: c.name == "postgres" ? "postgres" : nil,
                                hostname: c.name, cpus: 2, memoryMib: (c.memoryLimitBytes ?? 1 << 30) >> 20,
                                kernel: "Linux 6.12 (demo)", vmDir: "~/Library/Application Support/Graviton/vms/\(c.id)", restart: restart,
                                project: c.project, service: c.service, labels: labels.isEmpty ? nil : labels)
    }

    public func openTerminal(_ spec: ExecSpec) throws -> any TerminalSession {
        let c = list[try resolve(spec.id)]
        guard c.isRunning else { throw EngineError.daemon("container \(c.name) isn't running") }
        return DemoTerminal(container: c.name)
    }

    public func execCommand(id: String, command: [String], env: [String] = [], timeout: Int = 120) throws -> ExecResult {
        let c = list[try resolve(id)]
        guard c.isRunning else { throw EngineError.daemon("container \(c.name) isn't running") }
        return ExecResult(exitCode: 0, output: "(demo) executed \(command.joined(separator: " ")) inside \(c.name)\n", truncated: false)
    }

    public func snapshotStatus() -> SnapshotInfo {
        SnapshotInfo(enabled: true, totalSlots: 2, inUse: 0, totalBytes: 27 * 1024 * 1024, maxSlotsPerShape: 8,
                     groups: [SnapshotGroup(cpus: 2, memoryMib: 1024, rosetta: false, slots: 2, inUse: 0, sizeBytes: 27 * 1024 * 1024)])
    }

    public func snapshotWarm(cpus: UInt32? = nil, memoryMib: UInt64? = nil, rosetta: Bool = false, slots: UInt32? = nil) -> SnapshotInfo {
        let count = max(1, min(8, slots ?? 1))
        return SnapshotInfo(enabled: true, totalSlots: count, inUse: 0, totalBytes: UInt64(count) * 14 * 1024 * 1024, maxSlotsPerShape: 8,
                            groups: [SnapshotGroup(cpus: cpus ?? 2, memoryMib: memoryMib ?? 1024, rosetta: rosetta, slots: count, inUse: 0, sizeBytes: UInt64(count) * 14 * 1024 * 1024)])
    }

    public func snapshotClear() -> SnapshotInfo {
        SnapshotInfo(enabled: true, totalSlots: 0, inUse: 0, totalBytes: 0, maxSlotsPerShape: 8, groups: [])
    }

    public func events(since: UInt64?, limit: Int?) -> [EngineEvent] {
        let now = UInt64(Date.now.timeIntervalSince1970 * 1000)
        let min: UInt64 = 60_000
        let all = [
            EngineEvent(seq: 1, timestampMs: now - 260 * min, kind: .pulled, image: "postgres:17-alpine", message: "Pulled postgres:17-alpine (274 MB)"),
            EngineEvent(seq: 2, timestampMs: now - 240 * min, kind: .started, containerId: "5d10e7b3c4aa", container: "postgres", image: "postgres:17-alpine", message: "postgres started"),
            EngineEvent(seq: 3, timestampMs: now - 192 * min, kind: .built, image: "storefront/api:0.4.2", message: "Built storefront/api:0.4.2"),
            EngineEvent(seq: 4, timestampMs: now - 190 * min, kind: .started, containerId: "a3f9c2e81b07", container: "api-gateway", image: "storefront/api:0.4.2", message: "api-gateway started"),
            EngineEvent(seq: 5, timestampMs: now - 95 * min, kind: .failed, containerId: "1f2e3d4c5b6a", container: "minio", image: "minio/minio:latest", message: "minio exited with code 137"),
            EngineEvent(seq: 6, timestampMs: now - 52 * min, kind: .stopped, containerId: "e4c5d6f7a8b9", container: "worker", image: "storefront/worker:0.4.2", message: "worker exited"),
            EngineEvent(seq: 7, timestampMs: now - 41 * min, kind: .started, containerId: "c71a88e09f12", container: "web", image: "node:22-slim", message: "web started"),
            EngineEvent(seq: 8, timestampMs: now - 12 * min, kind: .buildFailed, image: "storefront/web", message: "Build of storefront/web failed: Dockerfile line 7: RUN npm ci: the command returned 1"),
        ]
        let after = all.filter { since == nil || $0.seq > since! }
        return Array(after.suffix(limit ?? after.count))
    }

    private func resolve(_ key: String) throws -> Int {
        if let i = list.firstIndex(where: { $0.name == key || $0.id == key }) { return i }
        let hits = list.indices.filter { !key.isEmpty && list[$0].id.hasPrefix(key) }
        guard hits.count == 1 else { throw EngineError.daemon("no such container: \(key)") }
        return hits[0]
    }

    private func appendTraffic() {
        let id = "a3f9c2e81b07"
        guard tick % 2 == 0, list.first(where: { $0.id == id })?.isRunning == true else { return }
        let samples: [(LogLevel, String)] = [
            (.info, "GET  /v1/products?page=\(Int.random(in: 1...9)) 200 \(Int.random(in: 2...8)).\(Int.random(in: 0...9))ms"),
            (.info, "GET  /v1/health 200 0.\(Int.random(in: 2...6))ms"),
            (.info, "POST /v1/cart 201 \(Int.random(in: 6...14)).\(Int.random(in: 0...9))ms"),
            (.warn, "slow query: SELECT * FROM inventory … (\(Int.random(in: 120...260))ms)"),
        ]
        let pick = samples[Int.random(in: 0..<(tick % 7 == 0 ? 4 : 3))]
        var lines = logLines[id] ?? []
        lines.append(LogLine(seq: (lines.last?.seq ?? 0) + 1, timestampMs: UInt64(Date.now.timeIntervalSince1970 * 1000),
                             level: pick.0, message: pick.1))
        logLines[id] = Array(lines.suffix(2000))
    }
}
