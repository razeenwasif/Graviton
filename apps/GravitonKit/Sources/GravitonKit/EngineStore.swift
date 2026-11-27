import Foundation
import Observation

/// The app's single source of truth. Polls the engine, keeps short metric
/// histories for sparklines, and falls back to `DemoEngine` when gravitond
/// isn't reachable.
@MainActor
@Observable
public final class EngineStore {
    public enum Connection: Equatable, Sendable {
        case connecting
        /// Connected to gravitond.
        case live
        /// Using sample data (no daemon, or no Mac chosen on iOS).
        case demo
        /// A chosen Mac isn't answering; the last data it sent stays on screen.
        case unreachable
    }

    public private(set) var connection: Connection = .connecting
    public private(set) var containers: [Container] = []
    public private(set) var images: [ImageInfo] = []
    public private(set) var volumes: [VolumeInfo] = []
    public private(set) var registryLogins: [RegistryLoginInfo] = []
    public private(set) var network: NetworkInfo?
    public private(set) var networks: [UserNetwork] = []
    public private(set) var engine: EngineInfo?
    public private(set) var logs: [String: [LogLine]] = [:]
    /// Recent events, newest last.
    public private(set) var events: [EngineEvent] = []
    public private(set) var cpuHistory: [String: [Double]] = [:]
    public private(set) var memoryHistory: [String: [Double]] = [:]
    public var lastError: String?
    /// Why the live engine isn't answering (shown on iOS's host chip).
    public private(set) var liveError: String?

    private var volumesLoaded = false
    private var lastWidgetSignature: String?
    private var lastWidgetWrite = Date.distantPast

    public static let historyLength = 40

    /// The engine tried first: gravitond's socket on the Mac, a paired Mac on iOS.
    private var live: (any EngineService)?
    private let demo: DemoEngine
    /// What `live` is called in the UI ("This Mac", or the paired Mac's name).
    public private(set) var liveName: String?
    /// Show sample data when `live` doesn't answer (the Mac app without gravitond).
    /// Off for a chosen Mac on iOS, where sample data would be mistaken for it.
    private var fallBackToDemo = true

    /// - Parameter useDaemon: try gravitond's local socket first (macOS). Pass `false` for demo data.
    public init(useDaemon: Bool = true) {
        live = useDaemon ? SocketEngine() : nil
        liveName = useDaemon ? "This Mac" : nil
        demo = DemoEngine()
    }

    /// Back to gravitond on this Mac (falling back to demo data when it isn't running).
    public func useLocal() {
        use(SocketEngine(), named: "This Mac")
        fallBackToDemo = true
    }

    /// Whether the engine shown is this Mac's own.
    public var isLocal: Bool { live is SocketEngine }

    /// Switch to another engine (a paired Mac), or `nil` for demo data. Clears what was shown.
    public func use(_ engine: (any EngineService)?, named name: String?) {
        live = engine
        liveName = name
        fallBackToDemo = false
        connection = .connecting
        containers = []
        images = []
        volumes = []
        volumesLoaded = false
        registryLogins = []
        network = nil
        networks = []
        self.engine = nil
        logs = [:]
        events = []
        cpuHistory = [:]
        memoryHistory = [:]
        Task { await refresh() }
    }

    private var service: any EngineService {
        if connection == .live, let live { return live }
        return demo
    }

    public var runningCount: Int { containers.filter(\.isRunning).count }
    public var pausedCount: Int { containers.filter(\.isPaused).count }
    public var activeCount: Int { containers.filter(\.isActive).count }
    public var totalCPU: Double { containers.compactMap(\.cpuPercent).reduce(0, +) }
    public var totalMemory: UInt64 { containers.compactMap(\.memoryBytes).reduce(0, +) }
    public var stacks: [ContainerStack] { ContainerStack.group(containers).stacks }

    public func container(_ id: String?) -> Container? {
        guard let id else { return nil }
        return containers.first { $0.id == id }
    }

    /// Poll until the surrounding task is cancelled (use from `.task {}`).
    public func run(interval: Duration = .seconds(2)) async {
        while !Task.isCancelled {
            await refresh()
            try? await Task.sleep(for: interval)
        }
    }

    public func refresh() async {
        let previousConnection = connection
        if let live {
            do {
                _ = try await live.ping()
                connection = .live
                liveError = nil
            } catch {
                liveError = error.localizedDescription
                if !fallBackToDemo {
                    connection = .unreachable
                    return
                }
                connection = .demo
            }
        } else {
            connection = .demo
            liveError = nil
        }
        if connection != previousConnection {
            volumesLoaded = false
        }
        do {
            let service = self.service
            async let list = service.containers()
            async let imgs = service.images()
            async let info = service.engineInfo()
            containers = try await list
            images = try await imgs
            engine = try await info
            // Gracefully fall back if talking to an older gravitond without network_info / list_volumes.
            if let net = try? await service.networkInfo() {
                network = net
                if !net.networks.isEmpty {
                    networks = net.networks
                } else if let nets = try? await service.networks() {
                    networks = nets
                } else {
                    networks = [UserNetwork(id: "graviton", name: net.name, driver: net.driver, internalOnly: false, builtin: true, createdAt: 0, containers: net.endpoints.map(\.containerName))]
                }
            } else {
                let synth = NetworkInfo.synthesized(from: containers)
                network = synth
                networks = synth.networks
            }
            if !volumesLoaded, let vols = try? await service.volumes() {
                volumes = vols
                volumesLoaded = true
            }
            recordHistory()
            await refreshEvents(from: service)
            publishWidgetSnapshot()
        } catch {
            lastError = error.localizedDescription
            publishWidgetSnapshot()
        }
    }

    /// Fetch events newer than the last one shown (the whole recent list the first time).
    private func refreshEvents(from service: any EngineService) async {
        guard let fresh = try? await service.events(since: events.last?.seq, limit: events.isEmpty ? 200 : nil) else { return }
        // An engine that restarted with a fresh log starts again from 1.
        if let first = fresh.first, let last = events.last, first.seq <= last.seq { events = [] }
        events.append(contentsOf: fresh)
        if events.count > 500 { events.removeFirst(events.count - 500) }
    }

    public func loadLogs(for id: String, tail: Int? = 500, sinceMs: UInt64? = nil, untilMs: UInt64? = nil) async {
        do {
            logs[id] = try await service.logs(id: id, tail: tail, sinceMs: sinceMs, untilMs: untilMs)
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// Fetch logs for one-off queries (e.g. Fairy) without mutating the UI tab buffer.
    public func fetchLogs(for id: String, tail: Int? = 500, sinceMs: UInt64? = nil, untilMs: UInt64? = nil) async throws -> [LogLine] {
        try await service.logs(id: id, tail: tail, sinceMs: sinceMs, untilMs: untilMs)
    }

    /// Stream container logs in real time over Unix socket / TLS, falling back to polling in demo mode.
    public func followLogs(for id: String, tail: Int? = 500, sinceMs: UInt64? = nil, untilMs: UInt64? = nil) async {
        guard connection == .live else {
            while !Task.isCancelled {
                await loadLogs(for: id, tail: tail, sinceMs: sinceMs, untilMs: untilMs)
                try? await Task.sleep(for: .seconds(2))
            }
            return
        }

        let upstream: AsyncThrowingStream<LogLine, Error>
        switch live {
        case let local as SocketEngine:
            upstream = local.logsStream(id: id, tail: tail, sinceMs: sinceMs, untilMs: untilMs)
        case let remote as RemoteEngine:
            upstream = remote.logsStream(id: id, tail: tail, sinceMs: sinceMs, untilMs: untilMs)
        default:
            while !Task.isCancelled {
                await loadLogs(for: id, tail: tail, sinceMs: sinceMs, untilMs: untilMs)
                try? await Task.sleep(for: .seconds(2))
            }
            return
        }

        let hasWindow = sinceMs != nil || untilMs != nil
        if hasWindow {
            logs[id] = []
        }
        var replaced = hasWindow
        let maxLines = 5_000
        do {
            for try await entry in upstream {
                try Task.checkCancellation()
                if !replaced {
                    logs[id] = [entry]
                    replaced = true
                } else {
                    var current = logs[id] ?? []
                    if current.last?.seq != entry.seq {
                        current.append(entry)
                        if current.count > maxLines {
                            current.removeFirst(current.count - maxLines)
                        }
                        logs[id] = current
                    }
                }
            }
            if !Task.isCancelled && !replaced {
                logs[id] = []
            }
        } catch is CancellationError {
            return
        } catch {
            if !Task.isCancelled {
                lastError = error.localizedDescription
            }
        }
    }

    /// Pulling and running need gravitond; the demo engine can't.
    public var canManage: Bool { connection == .live }

    /// Pull an image. Throws so a sheet can show the error in place.
    @discardableResult
    public func pull(_ reference: String, platform: String? = nil) async throws -> ImageInfo {
        if let platform, !platform.isEmpty {
            var pulled: ImageInfo?
            for try await event in pullStream(reference, platform: platform) {
                if case let .pulled(image) = event { pulled = image }
            }
            guard let pulled else { throw EngineError.unexpectedResponse }
            return pulled
        }
        let image = try await service.pull(reference: reference)
        await refresh()
        return image
    }

    /// Give an existing local image an additional name or registry tag.
    @discardableResult
    public func tagImage(source: String, target: String) async throws -> ImageInfo {
        let image = try await service.tagImage(source: source, target: target)
        await refresh()
        return image
    }

    /// Push a local image to its registry using Keychain credentials.
    @discardableResult
    public func pushImage(reference: String) async throws -> ImagePushResult {
        let result = try await service.pushImage(reference: reference)
        await refresh()
        return result
    }

    /// Snapshot a container's filesystem and config into a new local image.
    @discardableResult
    public func commit(_ spec: CommitSpec) async throws -> ImageInfo {
        let image = try await service.commit(spec)
        await refresh()
        return image
    }

    /// Fork a container's overlay state and config into one or more isolated containers.
    @discardableResult
    public func fork(_ spec: ForkSpec) async throws -> [Container] {
        let created = try await service.fork(spec)
        await refresh()
        return created
    }

    /// Inspect overlayfs filesystem changes inside a running container.
    public func diff(id: String) async throws -> [DiffEntry] {
        try await service.diff(id: id)
    }

    /// Query the container's egress firewall & chaos policy.
    public func getNetworkPolicy(id: String) async throws -> ContainerNetworkPolicy {
        try await service.getNetworkPolicy(id: id)
    }

    /// Update the container's egress firewall & chaos policy on the fly.
    @discardableResult
    public func setNetworkPolicy(id: String, egress: EgressPolicy?, chaos: ChaosProfile?) async throws -> ContainerNetworkPolicy {
        let policy = try await service.setNetworkPolicy(id: id, egress: egress, chaos: chaos)
        await refresh()
        return policy
    }

    /// Toggle hypervisor X-Ray event tracing for a container.
    @discardableResult
    public func setXRay(id: String, enabled: Bool) async throws -> Bool {
        let active = try await service.setXRay(id: id, enabled: enabled)
        await refresh()
        return active
    }

    /// Fetch recent X-Ray telemetry events for a container.
    public func xrayEvents(id: String, limit: Int? = nil) async throws -> [XRayEvent] {
        try await service.xrayEvents(id: id, limit: limit)
    }

    /// Stream live X-Ray telemetry events for a container.
    public func xrayStream(id: String, limit: Int? = nil) -> AsyncThrowingStream<XRayEvent, Error> {
        switch live {
        case let local as SocketEngine where canManage:
            return local.xrayStream(id: id, limit: limit)
        case let remote as RemoteEngine where canManage:
            return remote.xrayStream(id: id, limit: limit)
        default:
            let svc = service
            return AsyncThrowingStream { continuation in
                let task = Task {
                    do {
                        for event in try await svc.xrayEvents(id: id, limit: limit) {
                            continuation.yield(event)
                        }
                        continuation.finish()
                    } catch {
                        continuation.finish(throwing: error)
                    }
                }
                continuation.onTermination = { _ in task.cancel() }
            }
        }
    }

    /// Pull an image with streamed per-layer progress. Refreshes the image list when done.
    public func pullStream(_ reference: String, platform: String? = nil) -> AsyncThrowingStream<PullEvent, Error> {
        let upstream: AsyncThrowingStream<PullEvent, Error>
        switch live {
        case let local as SocketEngine where canManage: upstream = local.pullStream(reference: reference, platform: platform)
        case let remote as RemoteEngine where canManage: upstream = remote.pullStream(reference: reference, platform: platform)
        default:
            return AsyncThrowingStream { $0.finish(throwing: EngineError.daemon("Pulls need gravitond running, here or on a paired Mac.")) }
        }
        return AsyncThrowingStream { continuation in
            let task = Task { @MainActor [weak self] in
                do {
                    for try await event in upstream { continuation.yield(event) }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
                await self?.refresh()
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// Run a container (pulling its image first if needed). Throws so a sheet can show the error.
    @discardableResult
    public func run(_ spec: RunSpec) async throws -> Container {
        let container = try await service.run(spec)
        await refresh()
        if volumesLoaded { await loadVolumes() }
        return container
    }

    /// Builds need a real engine: this Mac's reads the folder; a paired Mac's is sent it.
    public var canBuild: Bool { connection == .live && (live is SocketEngine || live is RemoteEngine) }

    /// Build an image; the stream carries log lines, then the image. The image list refreshes at the end.
    public func build(_ spec: BuildSpec) -> AsyncThrowingStream<BuildEvent, Error> {
        let upstream: AsyncThrowingStream<BuildEvent, Error>
        switch live {
        case let local as SocketEngine where canBuild: upstream = local.build(spec)
        case let remote as RemoteEngine where canBuild: upstream = remote.build(spec)
        default:
            return AsyncThrowingStream { $0.finish(throwing: EngineError.daemon("Builds need gravitond running, here or on a paired Mac.")) }
        }
        return AsyncThrowingStream { continuation in
            let task = Task { @MainActor [weak self] in
                do {
                    for try await event in upstream { continuation.yield(event) }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
                await self?.refresh()
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    // Disk space. They throw so a sheet can show the error in place.
    public func diskUsage() async throws -> DiskUsage { try await service.diskUsage() }

    /// Remove an image no container uses; returns the bytes freed.
    @discardableResult
    public func removeImage(_ image: ImageInfo) async throws -> UInt64 {
        try await removeImage(reference: image.reference)
    }

    /// Remove an image by reference string; returns the bytes freed.
    @discardableResult
    public func removeImage(reference: String) async throws -> UInt64 {
        let freed = try await service.removeImage(reference: reference)
        await refresh()
        return freed
    }

    public func prune(_ spec: PruneSpec) async throws -> PruneReport {
        let report = try await service.prune(spec)
        if !spec.dryRun { await refresh() }
        return report
    }

    public func remove(_ c: Container) async {
        do {
            _ = try await service.remove(id: c.id)
            logs[c.id] = nil
            await refresh()
            if volumesLoaded { await loadVolumes() }
        } catch {
            lastError = error.localizedDescription
        }
    }

    // Volumes.
    public func loadVolumes() async {
        do {
            volumes = try await service.volumes()
            volumesLoaded = true
        } catch {
            // Older daemons don't have list_volumes; keep empty without popping an alert.
            if case EngineError.daemon = error { volumesLoaded = true }
        }
    }

    @discardableResult
    public func createVolume(name: String) async throws -> VolumeInfo {
        let vol = try await service.createVolume(name: name)
        await loadVolumes()
        return vol
    }

    @discardableResult
    public func removeVolume(_ volume: VolumeInfo) async throws -> UInt64 {
        let freed = try await service.removeVolume(name: volume.name)
        await loadVolumes()
        return freed
    }

    // Networks.
    public func loadNetworks() async {
        if let list = try? await service.networks() {
            networks = list
        }
    }

    @discardableResult
    public func createNetwork(name: String, internalOnly: Bool = false, labels: [String: String] = [:]) async throws -> UserNetwork {
        let net = try await service.createNetwork(name: name, internalOnly: internalOnly, labels: labels)
        await refresh()
        return net
    }

    @discardableResult
    public func removeNetwork(_ name: String) async throws -> String {
        let removed = try await service.removeNetwork(name: name)
        await refresh()
        return removed
    }

    @discardableResult
    public func connectNetwork(_ network: String, container: String) async throws -> Bool {
        let connected = try await service.connectNetwork(network: network, container: container)
        await refresh()
        return connected
    }

    @discardableResult
    public func disconnectNetwork(_ network: String, container: String) async throws -> Bool {
        let connected = try await service.disconnectNetwork(network: network, container: container)
        await refresh()
        return connected
    }

    // Registry logins (usernames on disk; passwords stored in the Mac's Keychain).
    public func loadRegistryLogins() async {
        do {
            registryLogins = try await service.listLogins()
        } catch {
            if case EngineError.daemon = error { registryLogins = [] }
        }
    }

    @discardableResult
    public func listLogins() async -> [RegistryLoginInfo] {
        await loadRegistryLogins()
        return registryLogins
    }

    @discardableResult
    public func registryLogin(registry: String, username: String, password: String) async throws -> RegistryLoginInfo {
        let info = try await service.registryLogin(registry: registry, username: username, password: password)
        await loadRegistryLogins()
        return info
    }

    @discardableResult
    public func registryLogout(registry: String) async throws -> String {
        let removed = try await service.registryLogout(registry: registry)
        await loadRegistryLogins()
        return removed
    }

    // Panels call these directly and show errors in place.
    public func stats(for id: String) async throws -> ContainerStats { try await service.stats(id: id) }
    public func listFiles(in id: String, path: String) async throws -> DirectoryListing { try await service.listFiles(id: id, path: path) }
    public func readFile(in id: String, path: String) async throws -> FileContents { try await service.readFile(id: id, path: path) }
    public func inspect(_ id: String) async throws -> ContainerDetails { try await service.inspect(id: id) }
    public func openTerminal(_ spec: ExecSpec) async throws -> any TerminalSession { try await service.openTerminal(spec) }
    public func execCommand(id: String, command: [String], env: [String] = [], timeout: Int = 120) async throws -> ExecResult {
        try await service.execCommand(id: id, command: command, env: env, timeout: timeout)
    }
    public func snapshotStatus() async throws -> SnapshotInfo { try await service.snapshotStatus() }
    public func snapshotWarm(cpus: UInt32? = nil, memoryMib: UInt64? = nil, rosetta: Bool = false, slots: UInt32? = nil) async throws -> SnapshotInfo {
        try await service.snapshotWarm(cpus: cpus, memoryMib: memoryMib, rosetta: rosetta, slots: slots)
    }
    public func snapshotClear() async throws -> SnapshotInfo { try await service.snapshotClear() }

    /// File uploads and downloads need a real gravitond (local or remote).
    public var canTransferFiles: Bool { connection == .live && (live is SocketEngine || live is RemoteEngine) }

    /// Pack local files or folders into a tar and unpack them inside `destinationDir` of a running container.
    @discardableResult
    public func uploadFiles(to id: String, destinationDir: String, urls: [URL]) async throws -> UInt64 {
        guard canTransferFiles, !urls.isEmpty else {
            throw EngineError.daemon("File transfers need gravitond running, here or on a paired Mac.")
        }
        let (archive, size) = try await Task.detached { try BuildUpload.packItems(urls) }.value
        defer { try? FileManager.default.removeItem(at: archive) }
        switch live {
        case let local as SocketEngine:
            return try await local.putArchive(id: id, path: destinationDir, archiveFile: archive, size: size)
        case let remote as RemoteEngine:
            return try await remote.putArchive(id: id, path: destinationDir, archiveFile: archive, size: size)
        default:
            throw EngineError.daemon("File transfers need gravitond running, here or on a paired Mac.")
        }
    }

    /// Download a file or folder from a running container into a local tar file at `archiveFile`.
    public func downloadArchive(from id: String, path: String, to archiveFile: URL) async throws -> (isDir: Bool, name: String) {
        guard canTransferFiles else {
            throw EngineError.daemon("File transfers need gravitond running, here or on a paired Mac.")
        }
        switch live {
        case let local as SocketEngine:
            return try await local.getArchive(id: id, path: path, to: archiveFile)
        case let remote as RemoteEngine:
            return try await remote.getArchive(id: id, path: path, to: archiveFile)
        default:
            throw EngineError.daemon("File transfers need gravitond running, here or on a paired Mac.")
        }
    }

    /// Export one or more local images into a Docker/OCI `.tar` archive at `archiveFile`.
    public func saveImage(references: [String], to archiveFile: URL) async throws {
        guard canTransferFiles else {
            throw EngineError.daemon("Exporting images needs gravitond running, here or on a paired Mac.")
        }
        switch live {
        case let local as SocketEngine:
            try await local.saveImage(references: references, to: archiveFile)
        case let remote as RemoteEngine:
            try await remote.saveImage(references: references, to: archiveFile)
        default:
            throw EngineError.daemon("Exporting images needs gravitond running, here or on a paired Mac.")
        }
    }

    /// Load one or more images from a `.tar` / `.tar.gz` archive into the active engine.
    @discardableResult
    public func loadImage(from archiveFile: URL) async throws -> [ImageInfo] {
        guard canTransferFiles else {
            throw EngineError.daemon("Importing images needs gravitond running, here or on a paired Mac.")
        }
        let attrs = try? FileManager.default.attributesOfItem(atPath: archiveFile.path)
        guard let size = (attrs?[.size] as? NSNumber)?.uint64Value, size > 0 else {
            throw EngineError.daemon("Image archive \(archiveFile.lastPathComponent) is missing or empty.")
        }
        let loaded: [ImageInfo]
        switch live {
        case let local as SocketEngine:
            loaded = try await local.loadImage(from: archiveFile, size: size)
        case let remote as RemoteEngine:
            loaded = try await remote.loadImage(from: archiveFile, size: size)
        default:
            throw EngineError.daemon("Importing images needs gravitond running, here or on a paired Mac.")
        }
        await refresh()
        return loaded
    }

    public func start(_ c: Container) async { await lifecycle { try await $0.start(id: c.id) } }
    public func stop(_ c: Container) async { await lifecycle { try await $0.stop(id: c.id) } }
    public func pause(_ c: Container) async { await lifecycle { try await $0.pause(id: c.id) } }
    public func unpause(_ c: Container) async { await lifecycle { try await $0.unpause(id: c.id) } }
    public func restart(_ c: Container) async { await lifecycle { try await $0.restart(id: c.id) } }

    /// Throwing lifecycle variants for programmatic callers (e.g. Fairy) that surface errors inline.
    @discardableResult
    public func performStart(_ c: Container) async throws -> Container { try await throwingLifecycle { try await $0.start(id: c.id) } }
    @discardableResult
    public func performStop(_ c: Container) async throws -> Container { try await throwingLifecycle { try await $0.stop(id: c.id) } }
    @discardableResult
    public func performPause(_ c: Container) async throws -> Container { try await throwingLifecycle { try await $0.pause(id: c.id) } }
    @discardableResult
    public func performUnpause(_ c: Container) async throws -> Container { try await throwingLifecycle { try await $0.unpause(id: c.id) } }
    @discardableResult
    public func performRestart(_ c: Container) async throws -> Container { try await throwingLifecycle { try await $0.restart(id: c.id) } }
    @discardableResult
    public func performRemove(_ c: Container) async throws -> String {
        let removed = try await service.remove(id: c.id)
        logs[c.id] = nil
        await refresh()
        if volumesLoaded { await loadVolumes() }
        return removed
    }

    private func throwingLifecycle(_ action: (any EngineService) async throws -> Container) async throws -> Container {
        let updated = try await action(service)
        if let i = containers.firstIndex(where: { $0.id == updated.id }) { containers[i] = updated }
        await refresh()
        return updated
    }

    /// Start all stopped containers belonging to Compose project `project`.
    public func startStack(_ project: String) async {
        let targets = containers.filter { $0.project == project && !$0.isRunning }
        guard !targets.isEmpty else { return }
        for c in targets {
            do {
                let updated = try await service.start(id: c.id)
                if let i = containers.firstIndex(where: { $0.id == updated.id }) { containers[i] = updated }
            } catch {
                lastError = error.localizedDescription
            }
        }
        await refresh()
    }

    /// Stop all running containers belonging to Compose project `project`.
    public func stopStack(_ project: String) async {
        let targets = containers.filter { $0.project == project && $0.isRunning }
        guard !targets.isEmpty else { return }
        for c in targets {
            do {
                let updated = try await service.stop(id: c.id)
                if let i = containers.firstIndex(where: { $0.id == updated.id }) { containers[i] = updated }
            } catch {
                lastError = error.localizedDescription
            }
        }
        await refresh()
    }

    /// Restart all running containers in Compose project `project` (or start all if none are running).
    public func restartStack(_ project: String) async {
        let members = containers.filter { $0.project == project }
        guard !members.isEmpty else { return }
        let running = members.filter(\.isRunning)
        let targets = running.isEmpty ? members : running
        for c in targets {
            do {
                let updated = c.isRunning ? try await service.restart(id: c.id) : try await service.start(id: c.id)
                if let i = containers.firstIndex(where: { $0.id == updated.id }) { containers[i] = updated }
            } catch {
                lastError = error.localizedDescription
            }
        }
        await refresh()
    }

    private func lifecycle(_ action: (any EngineService) async throws -> Container) async {
        do {
            let updated = try await action(service)
            if let i = containers.firstIndex(where: { $0.id == updated.id }) { containers[i] = updated }
            await refresh()
        } catch {
            lastError = error.localizedDescription
        }
    }

    private func recordHistory() {
        for c in containers {
            append(&cpuHistory, c.id, c.cpuPercent ?? 0)
            append(&memoryHistory, c.id, Double(c.memoryBytes ?? 0))
        }
    }

    private func publishWidgetSnapshot() {
        let sorted = containers.sorted { a, b in
            if a.isRunning != b.isRunning { return a.isRunning }
            return a.name.localizedStandardCompare(b.name) == .orderedAscending
        }
        let items = sorted.prefix(8).map { c in
            WidgetContainerItem(
                id: c.id,
                name: c.name,
                image: c.image,
                status: c.status,
                cpuPercent: c.cpuPercent,
                memoryBytes: c.memoryBytes,
                portLabel: c.ports.first.map { ":\($0.hostPort)" },
                stack: c.project
            )
        }
        let snap = GravitonWidgetSnapshot(
            hostName: liveName ?? "This Mac",
            online: connection == .live,
            isDemo: connection == .demo,
            runningCount: runningCount,
            pausedCount: pausedCount,
            totalContainers: containers.count,
            imageCount: images.count,
            volumeCount: volumes.count,
            networkCount: networks.count,
            totalCPU: totalCPU,
            vmMemoryUsedBytes: engine?.vmMemoryUsedBytes ?? totalMemory,
            hostMemoryBytes: engine?.memoryBytes ?? 0,
            vmMemoryFraction: engine?.vmMemoryFraction ?? 0,
            otherHostMemoryFraction: engine?.otherHostMemoryFraction ?? 0,
            containers: items
        )
        let signature = "\(snap.hostName)|\(snap.online)|\(snap.runningCount)/\(snap.totalContainers)|\(snap.imageCount)|\(snap.volumeCount)|\(items.map { "\($0.id):\($0.status)" }.joined(separator: ","))"
        let elapsed = Date.now.timeIntervalSince(lastWidgetWrite)
        if signature != lastWidgetSignature || elapsed >= 15 {
            lastWidgetSignature = signature
            lastWidgetWrite = .now
            GravitonWidgetStore.save(snap)
        }
    }

    private func append(_ history: inout [String: [Double]], _ id: String, _ value: Double) {
        var series = history[id] ?? []
        series.append(value)
        if series.count > Self.historyLength { series.removeFirst(series.count - Self.historyLength) }
        history[id] = series
    }
}

public enum ContainerFilter: String, CaseIterable, Identifiable, Sendable {
    case all = "All", running = "Running", stopped = "Stopped"
    public var id: String { rawValue }

    public func apply(_ list: [Container], search: String = "") -> [Container] {
        list.filter { c in
            let statusOK = switch self {
            case .all: true
            case .running: c.isActive
            case .stopped: !c.isActive
            }
            let q = search.trimmingCharacters(in: .whitespaces).lowercased()
            return statusOK && (
                q.isEmpty
                    || c.name.lowercased().contains(q)
                    || c.image.lowercased().contains(q)
                    || (c.project?.lowercased().contains(q) ?? false)
                    || (c.service?.lowercased().contains(q) ?? false)
            )
        }
    }
}
