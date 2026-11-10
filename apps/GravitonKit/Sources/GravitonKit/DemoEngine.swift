import Foundation

/// In-process engine with the same sample data as `Store::demo()` in Rust.
/// CPU and memory drift a little and api-gateway keeps logging, so the UI
/// looks alive without a daemon.
public actor DemoEngine: EngineService {
    private var list: [Container]
    private var logLines: [String: [LogLine]]
    private let imageList: [ImageInfo]
    private var tick = 0

    public init() {
        let now = UInt64(Date.now.timeIntervalSince1970)
        let mib: UInt64 = 1024 * 1024
        func tcp(_ h: UInt16, _ c: UInt16) -> PortMapping { PortMapping(hostPort: h, containerPort: c, protocol: .tcp) }
        func make(_ id: String, _ name: String, _ image: String, up: (UInt64, Double, UInt64)?, exit: Int32? = nil,
                  ports: [PortMapping] = [], mounts: [Mount] = [], ip: String? = nil) -> Container {
            Container(id: id, name: name, image: image, status: up == nil ? .exited : .running, exitCode: exit,
                      ports: ports, mounts: mounts, ipAddress: ip, cpuPercent: up?.1, memoryBytes: up.map { $0.2 * mib },
                      memoryLimitBytes: 1024 * mib, createdAt: now - 4 * 3600, startedAt: up.map { now - $0.0 })
        }
        list = [
            make("a3f9c2e81b07", "api-gateway", "storefront/api:0.4.2", up: (11_520, 12.4, 412), ports: [tcp(8080, 80)],
                 mounts: [Mount(source: "~/code/storefront", target: "/app", readOnly: false)], ip: "192.168.64.4"),
            make("5d10e7b3c4aa", "postgres", "postgres:17-alpine", up: (11_520, 3.1, 286), ports: [tcp(5432, 5432)],
                 mounts: [Mount(source: "pgdata", target: "/var/lib/postgresql/data", readOnly: false)], ip: "192.168.64.2"),
            make("9be2041f6d33", "redis-cache", "redis:7.4", up: (11_520, 0.6, 24), ports: [tcp(6379, 6379)], ip: "192.168.64.3"),
            make("c71a88e09f12", "web", "node:22-slim", up: (2_460, 7.8, 538), ports: [tcp(3000, 3000)],
                 mounts: [Mount(source: "~/code/storefront/web", target: "/srv", readOnly: false)], ip: "192.168.64.5"),
            make("e4c5d6f7a8b9", "worker", "storefront/worker:0.4.2", up: nil, exit: 0),
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

    public func engineInfo() -> EngineInfo {
        let used = list.compactMap(\.memoryBytes).reduce(0, +) + 1_900 * 1024 * 1024
        return EngineInfo(version: "0.1.0", protocol: 1, kernel: "Linux 6.12 (demo)", arch: "arm64", cpus: 4,
                          memoryBytes: 8 << 30, memoryUsedBytes: used, diskBytes: 64 << 30, diskUsedBytes: 6963 << 20)
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
