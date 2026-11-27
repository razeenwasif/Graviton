import Foundation

/// Control API request: `{"method": "...", "params": {...}}` (see `graviton-proto`).
public enum EngineRequest: Encodable, Sendable, Equatable {
    case ping
    case engineInfo
    case listContainers
    case listImages
    case listVolumes
    case createVolume(name: String)
    case removeVolume(name: String)
    case networkInfo
    case listNetworks
    case createNetwork(name: String, internal: Bool = false, labels: [String: String] = [:])
    case removeNetwork(name: String)
    case connectNetwork(network: String, container: String)
    case disconnectNetwork(network: String, container: String)
    case logs(id: String, tail: Int?, follow: Bool = false, sinceMs: UInt64? = nil, untilMs: UInt64? = nil)
    case start(id: String)
    case stop(id: String)
    case pause(id: String)
    case unpause(id: String)
    case fork(ForkSpec)
    case diff(id: String)
    case getNetworkPolicy(id: String)
    case setNetworkPolicy(id: String, egress: EgressPolicy?, chaos: ChaosProfile?)
    case setXRay(id: String, enabled: Bool)
    case xray(id: String, follow: Bool = false, limit: Int? = nil)
    case restart(id: String)
    case pull(reference: String, progress: Bool = false, platform: String? = nil)
    case tagImage(source: String, target: String)
    case pushImage(reference: String, progress: Bool = false)
    case listLogins
    case registryLogin(registry: String, username: String, password: String)
    case registryLogout(registry: String)
    case saveImage(references: [String])
    case loadImage(size: UInt64)
    case commit(CommitSpec)
    case run(RunSpec)
    case build(BuildSpec)
    case remove(id: String)
    case diskUsage
    case removeImage(reference: String)
    case prune(PruneSpec)
    case snapshotStatus
    case snapshotWarm(cpus: UInt32? = nil, memoryMib: UInt64? = nil, rosetta: Bool = false, slots: UInt32? = nil)
    case snapshotClear
    case freezeAndExport(id: String, destBundleDir: String, keepPaused: Bool = false, exitCode: Int32? = nil, failedStep: String? = nil)
    case restoreFrozen(bundleDir: String, name: String? = nil, ephemeral: Bool = true)
    case hibernateStack(key: String, project: String)
    case thawStack(key: String, stopConflicts: Bool = true)
    case listStackCheckpoints
    case deleteStackCheckpoint(key: String)
    case saveWarmTemplate(key: String, containerId: String)
    case runFromWarmTemplate(key: String, name: String, workspace: String, cpus: UInt32? = nil, memoryMib: UInt64? = nil, env: [String] = [])
    case listWarmTemplates
    case deleteWarmTemplate(key: String)
    case prepareMigration(image: String, digest: String, name: String)
    case exportMigration(id: String, includeVmState: Bool = true)
    case importMigration(size: UInt64, name: String? = nil, start: Bool = true)
    case getPowerAndOffload
    case setOffloadPolicy(OffloadPolicy)
    case startPortTunnel(containerName: String, remoteHost: String, remoteAddr: String, localPort: UInt16, remotePort: UInt16)
    case listPortTunnels
    case stopPortTunnel(id: String)
    case exec(ExecSpec)
    case stats(id: String)
    case listFiles(id: String, path: String)
    case readFile(id: String, path: String)
    case getArchive(id: String, path: String)
    case putArchive(id: String, path: String, size: UInt64)
    case inspect(id: String)
    case events(since: UInt64?, limit: Int?)
    case registerPush(token: String, sandbox: Bool)
    case pairBegin
    case listDevices
    case revokeDevice(id: String)
    case pair(code: String, device: String)
    case auth(token: String)

    private enum Keys: String, CodingKey { case method, params }
    private struct IDParams: Encodable { let id: String }
    private struct NameParams: Encodable { let name: String }
    private struct CreateNetworkParams: Encodable {
        let name: String
        let internalOnly: Bool
        let labels: [String: String]
        enum CodingKeys: String, CodingKey {
            case name
            case internalOnly = "internal"
            case labels
        }
    }
    private struct ConnectNetworkParams: Encodable { let network: String; let container: String }
    private struct LogParams: Encodable {
        let id: String
        let tail: Int?
        let follow: Bool?
        let sinceMs: UInt64?
        let untilMs: UInt64?
    }
    private struct PullParams: Encodable {
        let reference: String
        let progress: Bool?
        let platform: String?
    }
    private struct TagParams: Encodable { let source: String; let target: String }
    private struct PushImageParams: Encodable { let reference: String; let progress: Bool? }
    private struct RegistryLoginParams: Encodable { let registry: String; let username: String; let password: String }
    private struct RegistryParams: Encodable { let registry: String }
    private struct SaveImageParams: Encodable { let references: [String] }
    private struct LoadImageParams: Encodable { let size: UInt64 }
    private struct PairParams: Encodable { let code: String; let device: String }
    private struct AuthParams: Encodable { let token: String }
    private struct PathParams: Encodable { let id: String; let path: String }
    private struct PutArchiveParams: Encodable { let id: String; let path: String; let size: UInt64 }
    private struct SetNetworkPolicyParams: Encodable { let id: String; let egress: EgressPolicy?; let chaos: ChaosProfile? }
    private struct SetXRayParams: Encodable { let id: String; let enabled: Bool }
    private struct XRayParams: Encodable { let id: String; let follow: Bool?; let limit: Int? }
    private struct EventsParams: Encodable { let since: UInt64?; let limit: Int? }
    private struct PushParams: Encodable { let token: String; let sandbox: Bool }
    private struct SnapshotWarmParams: Encodable {
        let cpus: UInt32?
        let memoryMib: UInt64?
        let rosetta: Bool?
        let slots: UInt32?
    }
    private struct FreezeAndExportParams: Encodable {
        let id: String
        let destBundleDir: String
        let keepPaused: Bool
        let exitCode: Int32?
        let failedStep: String?
    }
    private struct RestoreFrozenParams: Encodable {
        let bundleDir: String
        let name: String?
        let ephemeral: Bool
    }
    private struct KeyParams: Encodable { let key: String }
    private struct HibernateStackParams: Encodable { let key: String; let project: String }
    private struct ThawStackParams: Encodable { let key: String; let stopConflicts: Bool }
    private struct SaveWarmTemplateParams: Encodable { let key: String; let containerId: String }
    private struct RunFromWarmTemplateParams: Encodable {
        let key: String
        let name: String
        let workspace: String
        let cpus: UInt32?
        let memoryMib: UInt64?
        let env: [String]
    }
    private struct PrepareMigrationParams: Encodable { let image: String; let digest: String; let name: String }
    private struct ExportMigrationParams: Encodable { let id: String; let includeVmState: Bool }
    private struct ImportMigrationParams: Encodable { let size: UInt64; let name: String?; let start: Bool }
    private struct StartPortTunnelParams: Encodable {
        let containerName: String
        let remoteHost: String
        let remoteAddr: String
        let localPort: UInt16
        let remotePort: UInt16
    }

    public var method: String {
        switch self {
        case .ping: "ping"
        case .engineInfo: "engine_info"
        case .listContainers: "list_containers"
        case .listImages: "list_images"
        case .listVolumes: "list_volumes"
        case .createVolume: "create_volume"
        case .removeVolume: "remove_volume"
        case .networkInfo: "network_info"
        case .listNetworks: "list_networks"
        case .createNetwork: "create_network"
        case .removeNetwork: "remove_network"
        case .connectNetwork: "connect_network"
        case .disconnectNetwork: "disconnect_network"
        case .logs: "logs"
        case .start: "start"
        case .stop: "stop"
        case .pause: "pause"
        case .unpause: "unpause"
        case .fork: "fork"
        case .diff: "diff"
        case .getNetworkPolicy: "get_network_policy"
        case .setNetworkPolicy: "set_network_policy"
        case .setXRay: "set_x_ray"
        case .xray: "x_ray"
        case .restart: "restart"
        case .pull: "pull"
        case .tagImage: "tag_image"
        case .pushImage: "push_image"
        case .listLogins: "list_logins"
        case .registryLogin: "registry_login"
        case .registryLogout: "registry_logout"
        case .saveImage: "save_image"
        case .loadImage: "load_image"
        case .commit: "commit"
        case .run: "run"
        case .build: "build"
        case .remove: "remove"
        case .diskUsage: "disk_usage"
        case .removeImage: "remove_image"
        case .prune: "prune"
        case .snapshotStatus: "snapshot_status"
        case .snapshotWarm: "snapshot_warm"
        case .snapshotClear: "snapshot_clear"
        case .freezeAndExport: "freeze_and_export"
        case .restoreFrozen: "restore_frozen"
        case .hibernateStack: "hibernate_stack"
        case .thawStack: "thaw_stack"
        case .listStackCheckpoints: "list_stack_checkpoints"
        case .deleteStackCheckpoint: "delete_stack_checkpoint"
        case .saveWarmTemplate: "save_warm_template"
        case .runFromWarmTemplate: "run_from_warm_template"
        case .listWarmTemplates: "list_warm_templates"
        case .deleteWarmTemplate: "delete_warm_template"
        case .prepareMigration: "prepare_migration"
        case .exportMigration: "export_migration"
        case .importMigration: "import_migration"
        case .getPowerAndOffload: "get_power_and_offload"
        case .setOffloadPolicy: "set_offload_policy"
        case .startPortTunnel: "start_port_tunnel"
        case .listPortTunnels: "list_port_tunnels"
        case .stopPortTunnel: "stop_port_tunnel"
        case .exec: "exec"
        case .stats: "stats"
        case .listFiles: "list_files"
        case .readFile: "read_file"
        case .getArchive: "get_archive"
        case .putArchive: "put_archive"
        case .inspect: "inspect"
        case .events: "events"
        case .registerPush: "register_push"
        case .pairBegin: "pair_begin"
        case .listDevices: "list_devices"
        case .revokeDevice: "revoke_device"
        case .pair: "pair"
        case .auth: "auth"
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        try c.encode(method, forKey: .method)
        switch self {
        case .ping, .engineInfo, .listContainers, .listImages, .listVolumes, .networkInfo, .listNetworks, .listLogins, .pairBegin, .listDevices, .snapshotStatus, .snapshotClear, .listStackCheckpoints, .listWarmTemplates, .getPowerAndOffload, .listPortTunnels: break
        case let .createNetwork(name, internalOnly, labels):
            try c.encode(CreateNetworkParams(name: name, internalOnly: internalOnly, labels: labels), forKey: .params)
        case let .removeNetwork(name):
            try c.encode(NameParams(name: name), forKey: .params)
        case let .connectNetwork(network, container), let .disconnectNetwork(network, container):
            try c.encode(ConnectNetworkParams(network: network, container: container), forKey: .params)
        case let .registryLogin(registry, username, password):
            try c.encode(RegistryLoginParams(registry: registry, username: username, password: password), forKey: .params)
        case let .registryLogout(registry):
            try c.encode(RegistryParams(registry: registry), forKey: .params)
        case let .createVolume(name), let .removeVolume(name): try c.encode(NameParams(name: name), forKey: .params)
        case let .revokeDevice(id): try c.encode(IDParams(id: id), forKey: .params)
        case let .pair(code, device): try c.encode(PairParams(code: code, device: device), forKey: .params)
        case let .auth(token): try c.encode(AuthParams(token: token), forKey: .params)
        case let .logs(id, tail, follow, sinceMs, untilMs):
            try c.encode(LogParams(id: id, tail: tail, follow: follow ? true : nil, sinceMs: sinceMs, untilMs: untilMs), forKey: .params)
        case .diskUsage: break
        case let .removeImage(reference): try c.encode(PullParams(reference: reference, progress: nil, platform: nil), forKey: .params)
        case let .prune(spec): try c.encode(spec, forKey: .params)
        case let .snapshotWarm(cpus, memoryMib, rosetta, slots):
            try c.encode(SnapshotWarmParams(cpus: cpus, memoryMib: memoryMib, rosetta: rosetta ? true : nil, slots: slots), forKey: .params)
        case let .freezeAndExport(id, destBundleDir, keepPaused, exitCode, failedStep):
            try c.encode(FreezeAndExportParams(id: id, destBundleDir: destBundleDir, keepPaused: keepPaused, exitCode: exitCode, failedStep: failedStep), forKey: .params)
        case let .restoreFrozen(bundleDir, name, ephemeral):
            try c.encode(RestoreFrozenParams(bundleDir: bundleDir, name: name, ephemeral: ephemeral), forKey: .params)
        case let .hibernateStack(key, project):
            try c.encode(HibernateStackParams(key: key, project: project), forKey: .params)
        case let .thawStack(key, stopConflicts):
            try c.encode(ThawStackParams(key: key, stopConflicts: stopConflicts), forKey: .params)
        case let .deleteStackCheckpoint(key), let .deleteWarmTemplate(key):
            try c.encode(KeyParams(key: key), forKey: .params)
        case let .saveWarmTemplate(key, containerId):
            try c.encode(SaveWarmTemplateParams(key: key, containerId: containerId), forKey: .params)
        case let .runFromWarmTemplate(key, name, workspace, cpus, memoryMib, env):
            try c.encode(RunFromWarmTemplateParams(key: key, name: name, workspace: workspace, cpus: cpus, memoryMib: memoryMib, env: env), forKey: .params)
        case let .prepareMigration(image, digest, name):
            try c.encode(PrepareMigrationParams(image: image, digest: digest, name: name), forKey: .params)
        case let .exportMigration(id, includeVmState):
            try c.encode(ExportMigrationParams(id: id, includeVmState: includeVmState), forKey: .params)
        case let .importMigration(size, name, start):
            try c.encode(ImportMigrationParams(size: size, name: name, start: start), forKey: .params)
        case let .setOffloadPolicy(policy):
            try c.encode(policy, forKey: .params)
        case let .startPortTunnel(containerName, remoteHost, remoteAddr, localPort, remotePort):
            try c.encode(StartPortTunnelParams(containerName: containerName, remoteHost: remoteHost, remoteAddr: remoteAddr, localPort: localPort, remotePort: remotePort), forKey: .params)
        case let .start(id), let .stop(id), let .pause(id), let .unpause(id), let .restart(id), let .remove(id), let .stats(id), let .inspect(id), let .diff(id), let .getNetworkPolicy(id), let .stopPortTunnel(id):
            try c.encode(IDParams(id: id), forKey: .params)
        case let .setNetworkPolicy(id, egress, chaos):
            try c.encode(SetNetworkPolicyParams(id: id, egress: egress, chaos: chaos), forKey: .params)
        case let .setXRay(id, enabled):
            try c.encode(SetXRayParams(id: id, enabled: enabled), forKey: .params)
        case let .xray(id, follow, limit):
            try c.encode(XRayParams(id: id, follow: follow ? true : nil, limit: limit), forKey: .params)
        case let .fork(spec): try c.encode(spec, forKey: .params)
        case let .listFiles(id, path), let .readFile(id, path), let .getArchive(id, path):
            try c.encode(PathParams(id: id, path: path), forKey: .params)
        case let .putArchive(id, path, size):
            try c.encode(PutArchiveParams(id: id, path: path, size: size), forKey: .params)
        case let .exec(spec): try c.encode(spec, forKey: .params)
        case let .events(since, limit): try c.encode(EventsParams(since: since, limit: limit), forKey: .params)
        case let .registerPush(token, sandbox): try c.encode(PushParams(token: token, sandbox: sandbox), forKey: .params)
        case let .pull(reference, progress, platform):
            try c.encode(PullParams(reference: reference, progress: progress ? true : nil, platform: platform), forKey: .params)
        case let .tagImage(source, target):
            try c.encode(TagParams(source: source, target: target), forKey: .params)
        case let .pushImage(reference, progress):
            try c.encode(PushImageParams(reference: reference, progress: progress ? true : nil), forKey: .params)
        case let .saveImage(references):
            try c.encode(SaveImageParams(references: references), forKey: .params)
        case let .loadImage(size):
            try c.encode(LoadImageParams(size: size), forKey: .params)
        case let .commit(spec):
            try c.encode(spec, forKey: .params)
        case let .run(spec): try c.encode(spec, forKey: .params)
        case let .build(spec): try c.encode(spec, forKey: .params)
        }
    }
}

/// Control API response: `{"type": "...", "data": ...}`.
public enum EngineResponse: Decodable, Sendable {
    case pong(protocol: UInt32)
    case engine(EngineInfo)
    case containers([Container])
    case images([ImageInfo])
    case volumes([VolumeInfo])
    case volume(VolumeInfo)
    case volumeRemoved(name: String, freedBytes: UInt64)
    case network(NetworkInfo)
    case networks([UserNetwork])
    case networkCreated(UserNetwork)
    case networkRemoved(name: String)
    case networkUpdated(network: String, container: String, connected: Bool)
    case logs([LogLine])
    case log(LogLine)
    case logEnd(exitCode: Int32?)
    case container(Container)
    case forked([Container])
    case diff([DiffEntry])
    case networkPolicy(ContainerNetworkPolicy)
    case xrayStatus(enabled: Bool)
    case xrayEvents([XRayEvent])
    case xrayEvent(XRayEvent)
    case image(ImageInfo)
    case imageArchive
    case pullProgress(PullProgress)
    case imagePushed(ImagePushResult)
    case logins([RegistryLoginInfo])
    case loggedIn(registry: String, username: String)
    case loggedOut(registry: String)
    case removed(id: String)
    case diskUsage(DiskUsage)
    case imageRemoved(reference: String, freedBytes: UInt64, keptBytes: UInt64)
    case pruned(PruneReport)
    case snapshots(SnapshotInfo)
    case frozenExported(FrozenBundleInfo)
    case frozenRestored(FrozenRestoreInfo)
    case stackHibernated(StackCheckpointInfo)
    case stackThawed(key: String, project: String, containers: [Container])
    case stackCheckpoints([StackCheckpointInfo])
    case stackCheckpointDeleted(key: String)
    case warmTemplateSaved(WarmTemplateInfo)
    case warmTemplates([WarmTemplateInfo])
    case warmTemplateDeleted(key: String)
    case migrationReady(MigrationPrepareResult)
    case migrationArchive(MigrationBundleInfo)
    case migrationImported(Container)
    case powerAndOffload(power: PowerStatus, policy: OffloadPolicy)
    case portTunnels([ActiveTunnel])
    case portTunnelStopped(id: String)
    case buildLog(String)
    case execOutput(stream: String, data: [UInt8])
    case execExited(code: Int32)
    case attached
    case stats(ContainerStats)
    case files(DirectoryListing)
    case file(FileContents)
    case archive(isDir: Bool, name: String)
    case copied(files: UInt64)
    case details(ContainerDetails)
    case events([EngineEvent])
    case push(PushInfo)
    case pairing(PairingInfo)
    case devices([PairedDevice])
    case revoked(id: String)
    case paired(token: String, deviceId: String, host: String)
    case authenticated(host: String)
    case error(String)

    private enum Keys: String, CodingKey { case type, data }
    private struct Removed: Decodable { let id: String }
    private struct Paired: Decodable { let token: String; let deviceId: String; let host: String }
    private struct Host: Decodable { let host: String }
    private struct Pong: Decodable { let `protocol`: UInt32 }
    private struct ErrorData: Decodable { let message: String }
    private struct ImageRemovedData: Decodable { let reference: String; let freedBytes: UInt64; let keptBytes: UInt64 }
    private struct BuildLine: Decodable { let line: String }
    private struct ArchiveData: Decodable { let isDir: Bool; let name: String }
    private struct CopiedData: Decodable { let files: UInt64 }
    private struct VolumeRemovedData: Decodable { let name: String; let freedBytes: UInt64 }
    private struct LogEndData: Decodable { let exitCode: Int32? }
    private struct ExecOutputData: Decodable { let stream: String?; let data: [UInt8] }
    private struct ExecExitedData: Decodable { let code: Int32 }
    private struct LoggedInData: Decodable { let registry: String; let username: String }
    private struct LoggedOutData: Decodable { let registry: String }
    private struct NetworkRemovedData: Decodable { let name: String }
    private struct NetworkUpdatedData: Decodable { let network: String; let container: String; let connected: Bool }
    private struct ForkedData: Decodable { let containers: [Container] }
    private struct DiffData: Decodable { let entries: [DiffEntry] }
    private struct XRayStatusData: Decodable { let enabled: Bool }
    private struct StackThawedData: Decodable { let key: String; let project: String; let containers: [Container] }
    private struct KeyData: Decodable { let key: String }
    private struct PowerAndOffloadData: Decodable { let power: PowerStatus; let policy: OffloadPolicy }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "pong": self = .pong(protocol: try c.decode(Pong.self, forKey: .data).protocol)
        case "engine": self = .engine(try c.decode(EngineInfo.self, forKey: .data))
        case "containers": self = .containers(try c.decode([Container].self, forKey: .data))
        case "images": self = .images(try c.decode([ImageInfo].self, forKey: .data))
        case "volumes": self = .volumes(try c.decode([VolumeInfo].self, forKey: .data))
        case "volume": self = .volume(try c.decode(VolumeInfo.self, forKey: .data))
        case "volume_removed":
            let r = try c.decode(VolumeRemovedData.self, forKey: .data)
            self = .volumeRemoved(name: r.name, freedBytes: r.freedBytes)
        case "network": self = .network(try c.decode(NetworkInfo.self, forKey: .data))
        case "networks": self = .networks(try c.decode([UserNetwork].self, forKey: .data))
        case "network_created": self = .networkCreated(try c.decode(UserNetwork.self, forKey: .data))
        case "network_removed": self = .networkRemoved(name: try c.decode(NetworkRemovedData.self, forKey: .data).name)
        case "network_updated":
            let u = try c.decode(NetworkUpdatedData.self, forKey: .data)
            self = .networkUpdated(network: u.network, container: u.container, connected: u.connected)
        case "logs": self = .logs(try c.decode([LogLine].self, forKey: .data))
        case "log": self = .log(try c.decode(LogLine.self, forKey: .data))
        case "log_end": self = .logEnd(exitCode: try c.decode(LogEndData.self, forKey: .data).exitCode)
        case "container": self = .container(try c.decode(Container.self, forKey: .data))
        case "forked": self = .forked(try c.decode(ForkedData.self, forKey: .data).containers)
        case "diff": self = .diff(try c.decode(DiffData.self, forKey: .data).entries)
        case "network_policy": self = .networkPolicy(try c.decode(ContainerNetworkPolicy.self, forKey: .data))
        case "x_ray_status": self = .xrayStatus(enabled: try c.decode(XRayStatusData.self, forKey: .data).enabled)
        case "x_ray_events": self = .xrayEvents(try c.decode([XRayEvent].self, forKey: .data))
        case "x_ray_event": self = .xrayEvent(try c.decode(XRayEvent.self, forKey: .data))
        case "image": self = .image(try c.decode(ImageInfo.self, forKey: .data))
        case "image_archive": self = .imageArchive
        case "pull_progress": self = .pullProgress(try c.decode(PullProgress.self, forKey: .data))
        case "image_pushed": self = .imagePushed(try c.decode(ImagePushResult.self, forKey: .data))
        case "logins": self = .logins(try c.decode([RegistryLoginInfo].self, forKey: .data))
        case "logged_in":
            let r = try c.decode(LoggedInData.self, forKey: .data)
            self = .loggedIn(registry: r.registry, username: r.username)
        case "logged_out":
            let r = try c.decode(LoggedOutData.self, forKey: .data)
            self = .loggedOut(registry: r.registry)
        case "removed": self = .removed(id: try c.decode(Removed.self, forKey: .data).id)
        case "disk_usage": self = .diskUsage(try c.decode(DiskUsage.self, forKey: .data))
        case "image_removed":
            let r = try c.decode(ImageRemovedData.self, forKey: .data)
            self = .imageRemoved(reference: r.reference, freedBytes: r.freedBytes, keptBytes: r.keptBytes)
        case "pruned": self = .pruned(try c.decode(PruneReport.self, forKey: .data))
        case "snapshots": self = .snapshots(try c.decode(SnapshotInfo.self, forKey: .data))
        case "frozen_exported": self = .frozenExported(try c.decode(FrozenBundleInfo.self, forKey: .data))
        case "frozen_restored": self = .frozenRestored(try c.decode(FrozenRestoreInfo.self, forKey: .data))
        case "stack_hibernated": self = .stackHibernated(try c.decode(StackCheckpointInfo.self, forKey: .data))
        case "stack_thawed":
            let t = try c.decode(StackThawedData.self, forKey: .data)
            self = .stackThawed(key: t.key, project: t.project, containers: t.containers)
        case "stack_checkpoints": self = .stackCheckpoints(try c.decode([StackCheckpointInfo].self, forKey: .data))
        case "stack_checkpoint_deleted": self = .stackCheckpointDeleted(key: try c.decode(KeyData.self, forKey: .data).key)
        case "warm_template_saved": self = .warmTemplateSaved(try c.decode(WarmTemplateInfo.self, forKey: .data))
        case "warm_templates": self = .warmTemplates(try c.decode([WarmTemplateInfo].self, forKey: .data))
        case "warm_template_deleted": self = .warmTemplateDeleted(key: try c.decode(KeyData.self, forKey: .data).key)
        case "migration_ready": self = .migrationReady(try c.decode(MigrationPrepareResult.self, forKey: .data))
        case "migration_archive": self = .migrationArchive(try c.decode(MigrationBundleInfo.self, forKey: .data))
        case "migration_imported": self = .migrationImported(try c.decode(Container.self, forKey: .data))
        case "power_and_offload":
            let po = try c.decode(PowerAndOffloadData.self, forKey: .data)
            self = .powerAndOffload(power: po.power, policy: po.policy)
        case "port_tunnels": self = .portTunnels(try c.decode([ActiveTunnel].self, forKey: .data))
        case "port_tunnel_stopped": self = .portTunnelStopped(id: try c.decode(Removed.self, forKey: .data).id)
        case "attached": self = .attached
        case "exec_output":
            let o = try c.decode(ExecOutputData.self, forKey: .data)
            self = .execOutput(stream: o.stream ?? "stdout", data: o.data)
        case "exec_exited":
            self = .execExited(code: try c.decode(ExecExitedData.self, forKey: .data).code)
        case "build_log": self = .buildLog(try c.decode(BuildLine.self, forKey: .data).line)
        case "stats": self = .stats(try c.decode(ContainerStats.self, forKey: .data))
        case "files": self = .files(try c.decode(DirectoryListing.self, forKey: .data))
        case "file": self = .file(try c.decode(FileContents.self, forKey: .data))
        case "archive":
            let a = try c.decode(ArchiveData.self, forKey: .data)
            self = .archive(isDir: a.isDir, name: a.name)
        case "copied": self = .copied(files: try c.decode(CopiedData.self, forKey: .data).files)
        case "details": self = .details(try c.decode(ContainerDetails.self, forKey: .data))
        case "events": self = .events(try c.decode([EngineEvent].self, forKey: .data))
        case "push": self = .push(try c.decode(PushInfo.self, forKey: .data))
        case "pairing": self = .pairing(try c.decode(PairingInfo.self, forKey: .data))
        case "devices": self = .devices(try c.decode([PairedDevice].self, forKey: .data))
        case "revoked": self = .revoked(id: try c.decode(Removed.self, forKey: .data).id)
        case "paired":
            let p = try c.decode(Paired.self, forKey: .data)
            self = .paired(token: p.token, deviceId: p.deviceId, host: p.host)
        case "authenticated": self = .authenticated(host: try c.decode(Host.self, forKey: .data).host)
        case "error": self = .error(try c.decode(ErrorData.self, forKey: .data).message)
        default:
            throw DecodingError.dataCorruptedError(forKey: .type, in: c, debugDescription: "unknown response type \(type)")
        }
    }
}

public enum Wire {
    public static var encoder: JSONEncoder {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        e.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return e
    }

    public static var decoder: JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }

    /// One request as a JSON line.
    public static func line(_ request: EngineRequest) throws -> Data {
        var data = try encoder.encode(request)
        data.append(UInt8(ascii: "\n"))
        return data
    }
}
