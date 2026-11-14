import Foundation
import Observation

/// A Mac this one is paired with, as `grav hosts` keeps it.
public struct PairedMac: Identifiable, Hashable, Sendable {
    /// What `grav --host` calls it.
    public var name: String
    public var address: String
    public var port: UInt16
    public var fingerprint: String
    public var token: String
    public var deviceID: String
    /// The name the Mac gave itself ("My Mac mini").
    public var hostName: String

    public var id: String { name }

    public var remoteHost: RemoteHost {
        RemoteHost(deviceID: deviceID, name: hostName, serviceName: nil, address: address, port: port, fingerprint: fingerprint)
    }

    public var shortFingerprint: String { RemoteHost.short(fingerprint) }

    /// "My Mac mini" → "my-mac-mini", as grav names hosts.
    public static func shortName(_ host: String) -> String {
        let mapped = host.lowercased().compactMap { c -> Character? in
            if c.isASCII && (c.isLetter || c.isNumber) { return c }
            return c == " " || c == "-" ? "-" : nil
        }
        let name = String(mapped).split(separator: "-").joined(separator: "-")
        return name.isEmpty ? "mac" : name
    }
}

/// The Macs `grav hosts` knows, from `hosts.json` in Graviton's support folder
/// (readable only by you). The Mac app and `grav` share it, so pairing in
/// either shows up in both. Which Mac the app shows is the app's own choice.
@MainActor
@Observable
public final class SharedHosts {
    public private(set) var hosts: [PairedMac] = []
    /// The Mac the app shows; `nil` is this Mac.
    public private(set) var selected: String?

    private let url: URL
    private let defaults: UserDefaults
    private static let selectedKey = "graviton.mac.host"

    public init(url: URL = SharedHosts.defaultURL, defaults: UserDefaults = .standard) {
        self.url = url
        self.defaults = defaults
        reload()
        selected = defaults.string(forKey: Self.selectedKey).flatMap { name in hosts.contains { $0.name == name } ? name : nil }
    }

    public nonisolated static var defaultURL: URL {
        URL(fileURLWithPath: NSHomeDirectory()).appending(path: "Library/Application Support/Graviton/hosts.json")
    }

    public var current: PairedMac? { hosts.first { $0.name == selected } }

    /// Read the file again (grav may have paired a Mac meanwhile).
    public func reload() {
        hosts = (try? File.read(url).hosts.map(\.paired)) ?? []
        if let selected, !hosts.contains(where: { $0.name == selected }) { self.selected = nil }
    }

    public func select(_ name: String?) {
        selected = name
        defaults.set(name, forKey: Self.selectedKey)
    }

    /// Remember a newly paired Mac under `name` (replacing one with the same name or certificate).
    @discardableResult
    public func add(_ result: PairingResult, name: String? = nil) throws -> PairedMac {
        var file = (try? File.read(url)) ?? File(default: nil, hosts: [])
        let mac = PairedMac(name: name ?? PairedMac.shortName(result.host.name), address: result.host.address ?? "", port: result.host.port,
                            fingerprint: result.host.fingerprint, token: result.token, deviceID: result.host.deviceID, hostName: result.host.name)
        file.hosts.removeAll { $0.name.caseInsensitiveCompare(mac.name) == .orderedSame || $0.fingerprint == mac.fingerprint }
        file.hosts.append(File.Entry(mac))
        try file.write(url)
        reload()
        return mac
    }

    /// Forget a Mac here and for grav (its gravitond keeps the device until `grav devices revoke` there).
    public func forget(_ name: String) throws {
        var file = try File.read(url)
        file.hosts.removeAll { $0.name == name }
        if file.default == name { file.default = nil }
        try file.write(url)
        if selected == name { select(nil) }
        reload()
    }

    /// `hosts.json`, as grav writes it.
    struct File: Codable {
        var `default`: String?
        var hosts: [Entry]

        struct Entry: Codable {
            var name: String
            var address: String
            var port: UInt16
            var fingerprint: String
            var token: String
            var deviceId: String
            var hostName: String

            init(_ m: PairedMac) {
                (name, address, port, fingerprint, token, deviceId, hostName) = (m.name, m.address, m.port, m.fingerprint, m.token, m.deviceID, m.hostName)
            }

            var paired: PairedMac {
                PairedMac(name: name, address: address, port: port, fingerprint: fingerprint, token: token, deviceID: deviceId, hostName: hostName)
            }
        }

        static func read(_ url: URL) throws -> File {
            let d = JSONDecoder()
            d.keyDecodingStrategy = .convertFromSnakeCase
            return try d.decode(File.self, from: Data(contentsOf: url))
        }

        func write(_ url: URL) throws {
            let e = JSONEncoder()
            e.keyEncodingStrategy = .convertToSnakeCase
            e.outputFormatting = [.prettyPrinted, .sortedKeys]
            let data = try e.encode(self)
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            let tmp = url.appendingPathExtension("tmp")
            FileManager.default.createFile(atPath: tmp.path, contents: data, attributes: [.posixPermissions: 0o600])
            _ = try FileManager.default.replaceItemAt(url, withItemAt: tmp)
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
        }
    }
}
