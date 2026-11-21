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
    case logs(id: String, tail: Int?)
    case start(id: String)
    case stop(id: String)
    case restart(id: String)
    case pull(reference: String, progress: Bool = false)
    case run(RunSpec)
    case build(BuildSpec)
    case remove(id: String)
    case diskUsage
    case removeImage(reference: String)
    case prune(PruneSpec)
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
    private struct LogParams: Encodable { let id: String; let tail: Int? }
    private struct PullParams: Encodable {
        let reference: String
        let progress: Bool?
    }
    private struct PairParams: Encodable { let code: String; let device: String }
    private struct AuthParams: Encodable { let token: String }
    private struct PathParams: Encodable { let id: String; let path: String }
    private struct PutArchiveParams: Encodable { let id: String; let path: String; let size: UInt64 }
    private struct EventsParams: Encodable { let since: UInt64?; let limit: Int? }
    private struct PushParams: Encodable { let token: String; let sandbox: Bool }

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
        case .logs: "logs"
        case .start: "start"
        case .stop: "stop"
        case .restart: "restart"
        case .pull: "pull"
        case .run: "run"
        case .build: "build"
        case .remove: "remove"
        case .diskUsage: "disk_usage"
        case .removeImage: "remove_image"
        case .prune: "prune"
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
        case .ping, .engineInfo, .listContainers, .listImages, .listVolumes, .networkInfo, .pairBegin, .listDevices: break
        case let .createVolume(name), let .removeVolume(name): try c.encode(NameParams(name: name), forKey: .params)
        case let .revokeDevice(id): try c.encode(IDParams(id: id), forKey: .params)
        case let .pair(code, device): try c.encode(PairParams(code: code, device: device), forKey: .params)
        case let .auth(token): try c.encode(AuthParams(token: token), forKey: .params)
        case let .logs(id, tail): try c.encode(LogParams(id: id, tail: tail), forKey: .params)
        case .diskUsage: break
        case let .removeImage(reference): try c.encode(PullParams(reference: reference, progress: nil), forKey: .params)
        case let .prune(spec): try c.encode(spec, forKey: .params)
        case let .start(id), let .stop(id), let .restart(id), let .remove(id), let .stats(id), let .inspect(id):
            try c.encode(IDParams(id: id), forKey: .params)
        case let .listFiles(id, path), let .readFile(id, path), let .getArchive(id, path):
            try c.encode(PathParams(id: id, path: path), forKey: .params)
        case let .putArchive(id, path, size):
            try c.encode(PutArchiveParams(id: id, path: path, size: size), forKey: .params)
        case let .exec(spec): try c.encode(spec, forKey: .params)
        case let .events(since, limit): try c.encode(EventsParams(since: since, limit: limit), forKey: .params)
        case let .registerPush(token, sandbox): try c.encode(PushParams(token: token, sandbox: sandbox), forKey: .params)
        case let .pull(reference, progress):
            try c.encode(PullParams(reference: reference, progress: progress ? true : nil), forKey: .params)
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
    case logs([LogLine])
    case container(Container)
    case image(ImageInfo)
    case pullProgress(PullProgress)
    case removed(id: String)
    case diskUsage(DiskUsage)
    case imageRemoved(reference: String, freedBytes: UInt64, keptBytes: UInt64)
    case pruned(PruneReport)
    case buildLog(String)
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
        case "logs": self = .logs(try c.decode([LogLine].self, forKey: .data))
        case "container": self = .container(try c.decode(Container.self, forKey: .data))
        case "image": self = .image(try c.decode(ImageInfo.self, forKey: .data))
        case "pull_progress": self = .pullProgress(try c.decode(PullProgress.self, forKey: .data))
        case "removed": self = .removed(id: try c.decode(Removed.self, forKey: .data).id)
        case "disk_usage": self = .diskUsage(try c.decode(DiskUsage.self, forKey: .data))
        case "image_removed":
            let r = try c.decode(ImageRemovedData.self, forKey: .data)
            self = .imageRemoved(reference: r.reference, freedBytes: r.freedBytes, keptBytes: r.keptBytes)
        case "pruned": self = .pruned(try c.decode(PruneReport.self, forKey: .data))
        case "attached": self = .attached
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
