import Foundation
import Observation

/// Minimal SemVer 2.0.0 version parser and comparator (ignores `+build` metadata in precedence).
public struct SemVer: Comparable, Equatable, Hashable, Sendable, CustomStringConvertible {
    public let major: Int
    public let minor: Int
    public let patch: Int
    public let prerelease: [String]

    public var isPrerelease: Bool { !prerelease.isEmpty }

    public var description: String {
        let base = "\(major).\(minor).\(patch)"
        return prerelease.isEmpty ? base : "\(base)-\(prerelease.joined(separator: "."))"
    }

    public init(major: Int, minor: Int, patch: Int, prerelease: [String] = []) {
        self.major = major
        self.minor = minor
        self.patch = patch
        self.prerelease = prerelease
    }

    public init?(_ raw: String) {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if s.hasPrefix("v") || s.hasPrefix("V") {
            s.removeFirst()
        }
        guard !s.isEmpty else { return nil }

        // Strip build metadata (+...) before comparing or splitting pre-release.
        if let plus = s.firstIndex(of: "+") {
            s = String(s[..<plus])
        }

        let coreString: String
        let prereleaseTokens: [String]
        if let dash = s.firstIndex(of: "-") {
            coreString = String(s[..<dash])
            let pre = String(s[s.index(after: dash)...])
            guard !pre.isEmpty else { return nil }
            let tokens = pre.split(separator: ".", omittingEmptySubsequences: false).map(String.init)
            guard tokens.allSatisfy({ !$0.isEmpty }) else { return nil }
            prereleaseTokens = tokens
        } else {
            coreString = s
            prereleaseTokens = []
        }

        let parts = coreString.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 2 || parts.count == 3,
              let major = Int(parts[0]), major >= 0,
              let minor = Int(parts[1]), minor >= 0
        else { return nil }
        let patch: Int
        if parts.count == 3 {
            guard let p = Int(parts[2]), p >= 0 else { return nil }
            patch = p
        } else {
            patch = 0
        }

        self.major = major
        self.minor = minor
        self.patch = patch
        self.prerelease = prereleaseTokens
    }

    public static func < (lhs: SemVer, rhs: SemVer) -> Bool {
        if lhs.major != rhs.major { return lhs.major < rhs.major }
        if lhs.minor != rhs.minor { return lhs.minor < rhs.minor }
        if lhs.patch != rhs.patch { return lhs.patch < rhs.patch }

        switch (lhs.prerelease.isEmpty, rhs.prerelease.isEmpty) {
        case (true, true): return false
        case (true, false): return false // stable > prerelease
        case (false, true): return true  // prerelease < stable
        case (false, false):
            let count = min(lhs.prerelease.count, rhs.prerelease.count)
            for i in 0..<count {
                let a = lhs.prerelease[i]
                let b = rhs.prerelease[i]
                if a == b { continue }
                let aNum = UInt64(a)
                let bNum = UInt64(b)
                switch (aNum, bNum) {
                case let (.some(an), .some(bn)): return an < bn
                case (.some, .none): return true  // numeric identifiers < non-numeric
                case (.none, .some): return false
                case (.none, .none): return a < b
                }
            }
            return lhs.prerelease.count < rhs.prerelease.count
        }
    }
}

/// A published Graviton release fetched from GitHub Releases.
public struct ReleaseInfo: Equatable, Sendable {
    public let tagName: String
    public let version: SemVer
    public let title: String
    public let notes: String
    public let releaseURL: URL
    public let dmgDownloadURL: URL?
    public let zipDownloadURL: URL?
    public let publishedAt: String?

    public var preferredDownloadURL: URL {
        dmgDownloadURL ?? zipDownloadURL ?? releaseURL
    }

    public init(
        tagName: String,
        version: SemVer,
        title: String,
        notes: String,
        releaseURL: URL,
        dmgDownloadURL: URL? = nil,
        zipDownloadURL: URL? = nil,
        publishedAt: String? = nil
    ) {
        self.tagName = tagName
        self.version = version
        self.title = title
        self.notes = notes
        self.releaseURL = releaseURL
        self.dmgDownloadURL = dmgDownloadURL
        self.zipDownloadURL = zipDownloadURL
        self.publishedAt = publishedAt
    }
}

/// Subset of the GitHub Releases REST API JSON payload (`GET /repos/{owner}/{repo}/releases/latest`).
public struct GitHubReleasePayload: Decodable, Sendable {
    public struct Asset: Decodable, Sendable {
        public let name: String
        public let browserDownloadUrl: String

        enum CodingKeys: String, CodingKey {
            case name
            case browserDownloadUrl = "browser_download_url"
        }
    }

    public let tagName: String
    public let name: String?
    public let body: String?
    public let draft: Bool?
    public let prerelease: Bool?
    public let htmlUrl: String
    public let publishedAt: String?
    public let assets: [Asset]?

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name"
        case name, body, draft, prerelease
        case htmlUrl = "html_url"
        case publishedAt = "published_at"
        case assets
    }
}

public enum UpdateState: Equatable, Sendable {
    case idle
    case checking
    case upToDate(current: SemVer, latest: SemVer)
    case available(ReleaseInfo)
    case unavailable(String)
    case failed(String)
}

/// Quiet, non-intrusive background & on-demand update checker backed by GitHub Releases.
@MainActor
@Observable
public final class UpdateChecker {
    public typealias Fetcher = @Sendable (URLRequest) async throws -> (Data, HTTPURLResponse)

    public static let autoCheckKey = "graviton.updates.autoCheck"
    public static let skippedVersionKey = "graviton.updates.skippedVersion"
    public static let lastCheckedAtKey = "graviton.updates.lastCheckedAt"
    public static let defaultRepoFeed = URL(string: "https://api.github.com/repos/razeenwasif/Graviton/releases/latest")!

    public let currentVersion: SemVer
    public let feedURL: URL

    public private(set) var state: UpdateState = .idle
    public var autoCheckEnabled: Bool {
        didSet { defaults.set(autoCheckEnabled, forKey: Self.autoCheckKey) }
    }
    public private(set) var skippedVersion: String? {
        didSet {
            if let skippedVersion {
                defaults.set(skippedVersion, forKey: Self.skippedVersionKey)
            } else {
                defaults.removeObject(forKey: Self.skippedVersionKey)
            }
        }
    }
    public private(set) var lastCheckedAt: Date? {
        didSet {
            if let lastCheckedAt {
                defaults.set(lastCheckedAt.timeIntervalSince1970, forKey: Self.lastCheckedAtKey)
            } else {
                defaults.removeObject(forKey: Self.lastCheckedAtKey)
            }
        }
    }

    /// True only when a newer release exists and hasn't been explicitly skipped.
    public var hasVisibleUpdate: Bool {
        guard case let .available(info) = state else { return false }
        return skippedVersion != info.version.description
    }

    public var visibleUpdate: ReleaseInfo? {
        guard case let .available(info) = state, skippedVersion != info.version.description else { return nil }
        return info
    }

    private let fetcher: Fetcher
    private let defaults: UserDefaults
    private let authTokenProvider: @Sendable () -> String?
    private let demoModeProvider: @Sendable () -> Bool

    public init(
        currentVersion: SemVer = UpdateChecker.bundleVersion(),
        feedURL: URL = UpdateChecker.defaultFeedURL(),
        defaults: UserDefaults = .standard,
        fetcher: Fetcher? = nil,
        authTokenProvider: (@Sendable () -> String?)? = nil,
        demoModeProvider: (@Sendable () -> Bool)? = nil
    ) {
        self.currentVersion = currentVersion
        self.feedURL = feedURL
        self.defaults = defaults
        self.fetcher = fetcher ?? Self.defaultFetch
        self.authTokenProvider = authTokenProvider ?? {
            let env = ProcessInfo.processInfo.environment
            let raw = env["GRAVITON_UPDATE_TOKEN"] ?? env["GITHUB_TOKEN"]
            let trimmed = raw?.trimmingCharacters(in: .whitespacesAndNewlines)
            return (trimmed?.isEmpty == false) ? trimmed : nil
        }
        self.demoModeProvider = demoModeProvider ?? {
            ProcessInfo.processInfo.environment["GRAVITON_DEMO"] == "1"
        }

        if defaults.object(forKey: Self.autoCheckKey) == nil {
            self.autoCheckEnabled = true
        } else {
            self.autoCheckEnabled = defaults.bool(forKey: Self.autoCheckKey)
        }
        self.skippedVersion = defaults.string(forKey: Self.skippedVersionKey)
        let ts = defaults.double(forKey: Self.lastCheckedAtKey)
        self.lastCheckedAt = ts > 0 ? Date(timeIntervalSince1970: ts) : nil
    }

    public static func bundleVersion() -> SemVer {
        let raw = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String
        return raw.flatMap(SemVer.init) ?? SemVer(major: 0, minor: 1, patch: 0)
    }

    public static func defaultFeedURL() -> URL {
        if let override = ProcessInfo.processInfo.environment["GRAVITON_UPDATE_URL"],
           !override.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
           let url = URL(string: override) {
            return url
        }
        return defaultRepoFeed
    }

    public static func parseRelease(from data: Data) throws -> ReleaseInfo? {
        let payload = try JSONDecoder().decode(GitHubReleasePayload.self, from: data)
        if payload.draft == true || payload.prerelease == true {
            return nil
        }
        guard let version = SemVer(payload.tagName),
              let releaseURL = URL(string: payload.htmlUrl)
        else { return nil }

        let assets = payload.assets ?? []
        let dmg = assets
            .first { $0.name.lowercased().hasSuffix(".dmg") }
            .flatMap { URL(string: $0.browserDownloadUrl) }
        let zip = assets
            .first { $0.name.lowercased().hasSuffix(".zip") }
            .flatMap { URL(string: $0.browserDownloadUrl) }
        let title = (payload.name?.trimmingCharacters(in: .whitespacesAndNewlines)).flatMap {
            $0.isEmpty ? nil : $0
        } ?? payload.tagName

        return ReleaseInfo(
            tagName: payload.tagName,
            version: version,
            title: title,
            notes: payload.body?.trimmingCharacters(in: .whitespacesAndNewlines) ?? "",
            releaseURL: releaseURL,
            dmgDownloadURL: dmg,
            zipDownloadURL: zip,
            publishedAt: payload.publishedAt
        )
    }

    /// Background startup check: respects `autoCheckEnabled`, demo mode, and `minInterval`.
    public func checkIfDue(minInterval: TimeInterval = 6 * 3600, now: Date = Date()) async {
        guard autoCheckEnabled, !demoModeProvider() else { return }
        if let lastCheckedAt, now.timeIntervalSince(lastCheckedAt) < minInterval {
            return
        }
        await checkNow(userInitiated: false, now: now)
    }

    /// Perform an update check now. Never throws or presents blocking alerts.
    public func checkNow(userInitiated: Bool = true, now: Date = Date()) async {
        if case .checking = state { return }
        state = .checking

        var request = URLRequest(url: feedURL)
        request.timeoutInterval = 15
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        request.setValue("2022-11-28", forHTTPHeaderField: "X-GitHub-Api-Version")
        request.setValue("Graviton/\(currentVersion)", forHTTPHeaderField: "User-Agent")
        if let token = authTokenProvider() {
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }

        do {
            let (data, response) = try await fetcher(request)
            switch response.statusCode {
            case 200:
                lastCheckedAt = now
                do {
                    guard let info = try Self.parseRelease(from: data) else {
                        state = .upToDate(current: currentVersion, latest: currentVersion)
                        return
                    }
                    if info.version > currentVersion {
                        state = .available(info)
                    } else {
                        state = .upToDate(current: currentVersion, latest: info.version)
                    }
                } catch {
                    state = .failed("Could not parse release feed (\(error.localizedDescription)).")
                }
            case 404:
                lastCheckedAt = now
                state = .unavailable("No public release feed found yet (repository may be private or have no published releases).")
            case 403, 429:
                state = .unavailable("GitHub API rate limit reached; try again later.")
            default:
                state = .failed("Update check returned HTTP \(response.statusCode).")
            }
        } catch {
            // Keep background startup failures quiet & non-alarming.
            let _ = userInitiated
            state = .unavailable("Couldn't reach GitHub Releases.")
        }
    }

    public func skip(_ release: ReleaseInfo) {
        skippedVersion = release.version.description
    }

    public func clearSkipped() {
        skippedVersion = nil
    }

    private static func defaultFetch(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let (data, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse else {
            throw URLError(.badServerResponse)
        }
        return (data, http)
    }
}
