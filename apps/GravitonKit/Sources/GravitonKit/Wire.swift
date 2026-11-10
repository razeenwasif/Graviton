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

    private enum Keys: String, CodingKey { case method, params }
    private struct IDParams: Encodable { let id: String }
    private struct LogParams: Encodable { let id: String; let tail: Int? }

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
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        try c.encode(method, forKey: .method)
        switch self {
        case .ping, .engineInfo, .listContainers, .listImages: break
        case let .logs(id, tail): try c.encode(LogParams(id: id, tail: tail), forKey: .params)
        case let .start(id), let .stop(id), let .restart(id): try c.encode(IDParams(id: id), forKey: .params)
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
    case error(String)

    private enum Keys: String, CodingKey { case type, data }
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
        e.outputFormatting = .sortedKeys
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
