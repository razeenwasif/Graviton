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
    private struct EventsParams: Encodable { let since: UInt64?; let limit: Int? }
    private struct PushParams: Encodable { let token: String; let sandbox: Bool }
    private struct SnapshotWarmParams: Encodable {
        let cpus: UInt32?
        let memoryMib: UInt64?
        let rosetta: Bool?
        let slots: UInt32?
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
        case .ping, .engineInfo, .listContainers, .listImages, .listVolumes, .networkInfo, .listNetworks, .listLogins, .pairBegin, .listDevices, .snapshotStatus, .snapshotClear: break
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
        case let .start(id), let .stop(id), let .pause(id), let .unpause(id), let .restart(id), let .remove(id), let .stats(id), let .inspect(id):
            try c.encode(IDParams(id: id), forKey: .params)
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
