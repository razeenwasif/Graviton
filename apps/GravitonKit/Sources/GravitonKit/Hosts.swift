import Foundation
import Network
import Observation
import Security

/// The Macs this device has paired with. Host details live in UserDefaults;
/// each token lives in the Keychain, readable only after first unlock on this device.
@MainActor
@Observable
public final class HostStore {
    public private(set) var hosts: [RemoteHost] = []
    /// `nil` means demo data.
    public private(set) var selectedID: String?

    private let defaults: UserDefaults
    private static let hostsKey = "graviton.hosts"
    private static let selectedKey = "graviton.selectedHost"
    private static let keychainService = "dev.graviton.remote"

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        if let data = defaults.data(forKey: Self.hostsKey), let saved = try? JSONDecoder().decode([RemoteHost].self, from: data) {
            hosts = saved
        }
        selectedID = defaults.string(forKey: Self.selectedKey).flatMap { id in hosts.contains { $0.id == id } ? id : nil }
    }

    public var selected: RemoteHost? { hosts.first { $0.id == selectedID } }

    /// An engine for the selected Mac, or `nil` for demo data.
    /// An engine for any paired Mac.
    public func engine(for id: String) -> RemoteEngine? {
        guard let host = hosts.first(where: { $0.id == id }), let token = Self.token(for: host.id) else { return nil }
        return RemoteEngine(host: host, token: token)
    }

    public func engineForSelection() -> RemoteEngine? {
        guard let host = selected, let token = Self.token(for: host.id) else { return nil }
        return RemoteEngine(host: host, token: token)
    }

    public func add(_ result: PairingResult) throws {
        try Self.setToken(result.token, for: result.host.id)
        hosts.removeAll { $0.id == result.host.id || ($0.fingerprint == result.host.fingerprint && $0.name == result.host.name) }
        hosts.append(result.host)
        selectedID = result.host.id
        save()
    }

    public func select(_ id: String?) {
        selectedID = id
        save()
    }

    /// Update ordered addresses, port, or Bonjour service name on an existing paired Mac.
    public func updateAddresses(for id: String, addresses: [String], port: UInt16? = nil, serviceName: String? = nil) {
        guard let i = hosts.firstIndex(where: { $0.id == id }) else { return }
        let ordered = RemoteHost.normalize(addresses)
        let cleanSvc = serviceName?.trimmingCharacters(in: .whitespacesAndNewlines)
        hosts[i] = RemoteHost(
            deviceID: hosts[i].deviceID,
            name: hosts[i].name,
            serviceName: cleanSvc.flatMap { $0.isEmpty ? nil : $0 } ?? (serviceName == nil ? hosts[i].serviceName : nil),
            address: ordered.first,
            addresses: ordered,
            port: port ?? hosts[i].port,
            fingerprint: hosts[i].fingerprint
        )
        save()
    }

    /// Match discovered Bonjour TXT `fp=` records against paired Macs so renames or IP shifts still resolve.
    public func syncBonjour(_ found: [HostBrowser.Found]) {
        guard !found.isEmpty else { return }
        var changed = false
        for i in hosts.indices {
            if let hit = found.first(where: { $0.fingerprint?.caseInsensitiveCompare(hosts[i].fingerprint) == .orderedSame }),
               hosts[i].serviceName != hit.name {
                hosts[i].serviceName = hit.name
                changed = true
            }
        }
        if changed { save() }
    }

    /// Forget a Mac on this device. (Its gravitond still lists the device until `grav devices revoke`.)
    public func forget(_ id: String) {
        Self.deleteToken(for: id)
        hosts.removeAll { $0.id == id }
        if selectedID == id { selectedID = nil }
        save()
    }

    private func save() {
        defaults.set(try? JSONEncoder().encode(hosts), forKey: Self.hostsKey)
        defaults.set(selectedID, forKey: Self.selectedKey)
    }

    // MARK: Keychain

    private static func query(_ id: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: keychainService, kSecAttrAccount as String: id]
    }

    static func setToken(_ token: String, for id: String) throws {
        deleteToken(for: id)
        var item = query(id)
        item[kSecValueData as String] = Data(token.utf8)
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let status = SecItemAdd(item as CFDictionary, nil)
        guard status == errSecSuccess else { throw EngineError.daemon("Couldn't save the token in the Keychain (\(status)).") }
    }

    static func token(for id: String) -> String? {
        var q = query(id)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess, let data = out as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func deleteToken(for id: String) {
        SecItemDelete(query(id) as CFDictionary)
    }
}

/// Macs advertising gravitond on the local network (`_graviton._tcp`).
@MainActor
@Observable
public final class HostBrowser {
    public struct Found: Hashable, Identifiable, Sendable {
        public var name: String
        public var fingerprint: String?
        public var id: String { name }

        public init(name: String, fingerprint: String? = nil) {
            self.name = name
            self.fingerprint = fingerprint
        }
    }

    public private(set) var found: [Found] = []
    public private(set) var problem: String?
    private var browser: NWBrowser?

    public init() {}

    public func match(fingerprint: String) -> Found? {
        found.first { $0.fingerprint?.caseInsensitiveCompare(fingerprint) == .orderedSame }
    }

    public func start() {
        guard browser == nil else { return }
        let descriptor = NWBrowser.Descriptor.bonjourWithTXTRecord(type: RemoteHost.bonjourType, domain: nil)
        let browser = NWBrowser(for: descriptor, using: .tcp)
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            var byName: [String: String?] = [:]
            for result in results {
                guard case let .service(name, _, _, _) = result.endpoint else { continue }
                var fp: String?
                if case let .bonjour(txt) = result.metadata,
                   let raw = txt.dictionary["fp"]?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased(),
                   raw.count == 64, raw.allSatisfy(\.isHexDigit) {
                    fp = raw
                }
                if byName[name] == nil || fp != nil {
                    byName[name] = fp
                }
            }
            let items = byName.keys.sorted().map { Found(name: $0, fingerprint: byName[$0] ?? nil) }
            Task { @MainActor in
                self?.found = items
            }
        }
        browser.stateUpdateHandler = { [weak self] state in
            Task { @MainActor in
                switch state {
                case let .failed(error), let .waiting(error):
                    self?.problem = "Can't look for Macs on this network (\(error.localizedDescription))."
                case .ready:
                    self?.problem = nil
                default:
                    break
                }
            }
        }
        browser.start(queue: .main)
        self.browser = browser
    }

    public func stop() {
        browser?.cancel()
        browser = nil
    }
}
