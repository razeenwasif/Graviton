import Foundation

/// Control API request: `{"method": "...", "params": {...}}` (see `graviton-proto`).
public enum EngineRequest: Encodable, Sendable, Equatable {
    case ping
    case engineInfo
    case listContainers
    case listImages
    case logs(id: String, tail: Int?)
    case start(id: String)
    case stop(id: String)
    case restart(id: String)
    case pull(reference: String)
    case run(RunSpec)
    case remove(id: String)
    case pairBegin
    case listDevices
    case revokeDevice(id: String)
    case pair(code: String, device: String)
    case auth(token: String)

    private enum Keys: String, CodingKey { case method, params }
    private struct IDParams: Encodable { let id: String }
    private struct LogParams: Encodable { let id: String; let tail: Int? }
    private struct PullParams: Encodable { let reference: String }
    private struct PairParams: Encodable { let code: String; let device: String }
    private struct AuthParams: Encodable { let token: String }

    public var method: String {
        switch self {
        case .ping: "ping"
        case .engineInfo: "engine_info"
        case .listContainers: "list_containers"
        case .listImages: "list_images"
        case .logs: "logs"
        case .start: "start"
        case .stop: "stop"
        case .restart: "restart"
        case .pull: "pull"
        case .run: "run"
        case .remove: "remove"
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
        case .ping, .engineInfo, .listContainers, .listImages, .pairBegin, .listDevices: break
        case let .revokeDevice(id): try c.encode(IDParams(id: id), forKey: .params)
        case let .pair(code, device): try c.encode(PairParams(code: code, device: device), forKey: .params)
        case let .auth(token): try c.encode(AuthParams(token: token), forKey: .params)
        case let .logs(id, tail): try c.encode(LogParams(id: id, tail: tail), forKey: .params)
        case let .start(id), let .stop(id), let .restart(id), let .remove(id): try c.encode(IDParams(id: id), forKey: .params)
        case let .pull(reference): try c.encode(PullParams(reference: reference), forKey: .params)
        case let .run(spec): try c.encode(spec, forKey: .params)
        }
    }
}

/// Control API response: `{"type": "...", "data": ...}`.
public enum EngineResponse: Decodable, Sendable {
    case pong(protocol: UInt32)
    case engine(EngineInfo)
    case containers([Container])
    case images([ImageInfo])
    case logs([LogLine])
    case container(Container)
    case image(ImageInfo)
    case removed(id: String)
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

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "pong": self = .pong(protocol: try c.decode(Pong.self, forKey: .data).protocol)
        case "engine": self = .engine(try c.decode(EngineInfo.self, forKey: .data))
        case "containers": self = .containers(try c.decode([Container].self, forKey: .data))
        case "images": self = .images(try c.decode([ImageInfo].self, forKey: .data))
        case "logs": self = .logs(try c.decode([LogLine].self, forKey: .data))
        case "container": self = .container(try c.decode(Container.self, forKey: .data))
        case "image": self = .image(try c.decode(ImageInfo.self, forKey: .data))
        case "removed": self = .removed(id: try c.decode(Removed.self, forKey: .data).id)
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
