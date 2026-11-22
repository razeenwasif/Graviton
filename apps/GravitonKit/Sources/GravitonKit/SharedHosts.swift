import Foundation
import Observation

/// A Mac this one is paired with, as `grav hosts` keeps it.
public struct PairedMac: Identifiable, Hashable, Sendable {
    /// What `grav --host` calls it.
    public var name: String
    public var address: String
    public var addresses: [String]
    public var port: UInt16
    public var serviceName: String?
    public var fingerprint: String
    public var token: String
    public var deviceID: String
    /// The name the Mac gave itself ("My Mac mini").
    public var hostName: String

    public var id: String { name }

    public init(
        name: String,
        address: String,
        addresses: [String] = [],
        port: UInt16,
        serviceName: String? = nil,
        fingerprint: String,
        token: String,
        deviceID: String,
        hostName: String
    ) {
        let ordered = RemoteHost.normalize([address] + addresses)
        self.name = name
        self.address = ordered.first ?? address
        self.addresses = ordered
        self.port = port
        self.serviceName = serviceName
        self.fingerprint = fingerprint
        self.token = token
        self.deviceID = deviceID
        self.hostName = hostName
    }

    public var allAddresses: [String] {
        RemoteHost.normalize([address] + addresses)
    }

    public var remoteHost: RemoteHost {
        RemoteHost(
            deviceID: deviceID,
            name: hostName,
            serviceName: serviceName ?? hostName,
            address: allAddresses.first,
            addresses: allAddresses,
            port: port,
            fingerprint: fingerprint
        )
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
/// either shows up in both, and so does the default: the Mac `grav` sends
/// commands to without `--host` is the one the app shows, and picking a Mac
/// in the app's Host menu is `grav hosts use`.
@MainActor
@Observable
public final class SharedHosts {
    public private(set) var hosts: [PairedMac] = []
    /// The Mac the app shows and grav's default; `nil` is this Mac.
    public private(set) var selected: String?

    private let url: URL

    public init(url: URL = SharedHosts.defaultURL) {
        self.url = url
        reload()
        selected = (try? File.read(url).default).flatMap { name in hosts.contains { $0.name == name } ? name : nil }
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

    /// Show `name` (`nil`: this Mac) and make it grav's default too.
    public func select(_ name: String?) {
        selected = name
        guard var file = try? File.read(url), file.default != name else { return }
        file.default = name
        try? file.write(url)
    }

    /// Remember a newly paired Mac under `name` (replacing one with the same name or certificate).
    @discardableResult
    public func add(_ result: PairingResult, name: String? = nil) throws -> PairedMac {
        var file = (try? File.read(url)) ?? File(default: nil, hosts: [])
        let all = result.host.allAddresses
        let mac = PairedMac(
            name: name ?? PairedMac.shortName(result.host.name),
            address: all.first ?? "",
            addresses: all,
            port: result.host.port,
            serviceName: result.host.serviceName ?? result.host.name,
            fingerprint: result.host.fingerprint,
            token: result.token,
            deviceID: result.host.deviceID,
            hostName: result.host.name
        )
        file.hosts.removeAll { $0.name.caseInsensitiveCompare(mac.name) == .orderedSame || $0.fingerprint == mac.fingerprint }
        file.hosts.append(File.Entry(mac))
        try file.write(url)
        reload()
        return mac
    }

    /// Update ordered addresses, port, or Bonjour service name on an already paired Mac.
    @discardableResult
    public func update(_ name: String, addresses: [String], port: UInt16? = nil, serviceName: String? = nil) throws -> PairedMac {
        var file = try File.read(url)
        guard let idx = file.hosts.firstIndex(where: { $0.name.caseInsensitiveCompare(name) == .orderedSame }) else {
            throw EngineError.daemon("No paired Mac named \(name).")
        }
        var current = file.hosts[idx].paired
        let cleanAddrs = RemoteHost.normalize(addresses)
        let cleanService = serviceName.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
        let effectiveService = cleanService.flatMap { $0.isEmpty ? nil : $0 } ?? (serviceName == nil ? current.serviceName : nil)
        guard !cleanAddrs.isEmpty || effectiveService != nil else {
            throw EngineError.daemon("Enter at least one address or Bonjour service name.")
        }
        current = PairedMac(
            name: current.name,
            address: cleanAddrs.first ?? "",
            addresses: cleanAddrs,
            port: port ?? current.port,
            serviceName: effectiveService,
            fingerprint: current.fingerprint,
            token: current.token,
            deviceID: current.deviceID,
            hostName: current.hostName
        )
        file.hosts[idx] = File.Entry(current)
        try file.write(url)
        reload()
        return hosts.first { $0.name == current.name } ?? current
    }

    /// Update Bonjour service names for any paired Mac whose pinned certificate fingerprint appears in Bonjour TXT records.
    public func syncBonjour(_ found: [HostBrowser.Found]) {
        guard !found.isEmpty else { return }
        var changed = false
        var updated = hosts
        for i in updated.indices {
            if let hit = found.first(where: { $0.fingerprint?.caseInsensitiveCompare(updated[i].fingerprint) == .orderedSame }),
               updated[i].serviceName != hit.name {
                updated[i].serviceName = hit.name
                changed = true
            }
        }
        guard changed else { return }
        hosts = updated
        if var file = try? File.read(url) {
            for mac in updated {
                if let idx = file.hosts.firstIndex(where: { $0.name == mac.name }) {
                    file.hosts[idx] = File.Entry(mac)
                }
            }
            try? file.write(url)
        }
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
            var addresses: [String]?
            var port: UInt16
            var serviceName: String?
            var fingerprint: String
            var token: String
            var deviceId: String
            var hostName: String

            init(_ m: PairedMac) {
                let all = m.allAddresses
                self.name = m.name
                self.address = all.first ?? m.address
                self.addresses = all.count > 1 ? all : nil
                self.port = m.port
                let svc = m.serviceName?.trimmingCharacters(in: .whitespacesAndNewlines)
                self.serviceName = (svc?.isEmpty == false) ? svc : nil
                self.fingerprint = m.fingerprint
                self.token = m.token
                self.deviceId = m.deviceID
                self.hostName = m.hostName
            }

            var paired: PairedMac {
                PairedMac(
                    name: name,
                    address: address,
                    addresses: addresses ?? [],
                    port: port,
                    serviceName: serviceName,
                    fingerprint: fingerprint,
                    token: token,
                    deviceID: deviceId,
                    hostName: hostName
                )
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
