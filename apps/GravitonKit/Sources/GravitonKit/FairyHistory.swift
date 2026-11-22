import Foundation

// MARK: - Fairy Chat & History Models

/// A single tool step executed (or awaiting Touch ID) during a Fairy assistant turn.
public struct FairyToolStep: Identifiable, Codable, Hashable, Sendable {
    public enum Status: String, Codable, Hashable, Sendable {
        case running, waitingTouchID, done, denied, failed
    }

    public let id: UUID
    public var tool: String
    public var title: String
    public var status: Status
    public var touchIDVerified: Bool
    public var note: String?

    public init(
        id: UUID = UUID(),
        tool: String,
        title: String,
        status: Status = .running,
        touchIDVerified: Bool = false,
        note: String? = nil
    ) {
        self.id = id
        self.tool = tool
        self.title = title
        self.status = status
        self.touchIDVerified = touchIDVerified
        self.note = note
    }
}

/// One user, assistant, or error message inside a Fairy conversation.
public struct FairyChatMessage: Identifiable, Codable, Hashable, Sendable {
    public enum Role: String, Codable, Hashable, Sendable {
        case user, assistant, error
    }

    public let id: UUID
    public var role: Role
    public var text: String
    public var steps: [FairyToolStep]
    public var date: Date

    public init(
        id: UUID = UUID(),
        role: Role,
        text: String,
        steps: [FairyToolStep] = [],
        date: Date = .now
    ) {
        self.id = id
        self.role = role
        self.text = text
        self.steps = steps
        self.date = date
    }
}

/// A saved Fairy conversation thread in Graviton.
public struct FairyConversation: Identifiable, Codable, Hashable, Sendable {
    public let id: UUID
    public var title: String
    public var hostName: String
    public var createdAt: Date
    public var updatedAt: Date
    public var messages: [FairyChatMessage]

    public init(
        id: UUID = UUID(),
        title: String,
        hostName: String,
        createdAt: Date = .now,
        updatedAt: Date = .now,
        messages: [FairyChatMessage] = []
    ) {
        self.id = id
        self.title = title
        self.hostName = hostName
        self.createdAt = createdAt
        self.updatedAt = updatedAt
        self.messages = messages
    }

    /// Single-line snippet of the most recent substantive message for history rows.
    public var preview: String {
        for msg in messages.reversed() {
            let singleLine = msg.text
                .split(whereSeparator: \.isNewline)
                .joined(separator: " ")
                .trimmingCharacters(in: .whitespacesAndNewlines)
            if !singleLine.isEmpty {
                if singleLine.count <= 100 { return singleLine }
                return String(singleLine.prefix(99)).trimmingCharacters(in: .whitespaces) + "…"
            }
        }
        return messages.isEmpty ? "Empty conversation" : "Tools ran without text reply"
    }

    /// Total tool invocations recorded across the conversation.
    public var stepCount: Int {
        messages.reduce(0) { $0 + $1.steps.count }
    }

    /// Markdown transcript suitable for copying to the clipboard.
    public var plainTranscript: String {
        var lines: [String] = ["# \(title) (\(hostName))", ""]
        for msg in messages {
            let speaker: String
            switch msg.role {
            case .user: speaker = "You"
            case .assistant: speaker = "Fairy"
            case .error: speaker = "Error"
            }
            if !msg.steps.isEmpty {
                for step in msg.steps {
                    let badge = step.touchIDVerified ? " [Touch ID]" : ""
                    lines.append("- [\(step.status.rawValue)] \(step.title)\(badge)")
                }
            }
            if !msg.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                lines.append("**\(speaker):** \(msg.text)")
            }
            lines.append("")
        }
        return lines.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

/// Persisted snapshot of all saved Fairy conversations and the last active thread ID.
public struct FairyHistorySnapshot: Codable, Hashable, Sendable {
    public var version: Int
    public var activeConversationID: UUID?
    public var conversations: [FairyConversation]

    public init(version: Int = 1, activeConversationID: UUID? = nil, conversations: [FairyConversation] = []) {
        self.version = version
        self.activeConversationID = activeConversationID
        self.conversations = conversations
    }
}

// MARK: - Persistent Store

/// Reads and atomically persists Graviton's Fairy conversation history under `0600` permissions.
public final class FairyHistoryStore: Sendable {
    public static let maxConversations = 200
    public static let maxMessagesPerConversation = 200

    public let url: URL

    public static var defaultURL: URL {
        #if os(macOS)
        FairyPaths.historyFileURL
        #else
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? FileManager.default.temporaryDirectory
        return base.appending(path: "Graviton/fairy-history.json")
        #endif
    }

    public init(url: URL = FairyHistoryStore.defaultURL) {
        self.url = url
    }

    /// Load saved conversations sorted newest-first, normalizing any interrupted tool steps.
    public func load() -> FairyHistorySnapshot {
        guard let data = try? Data(contentsOf: url) else {
            return FairyHistorySnapshot()
        }
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        guard var snapshot = try? decoder.decode(FairyHistorySnapshot.self, from: data) else {
            return FairyHistorySnapshot()
        }
        snapshot.conversations = snapshot.conversations
            .map { conv in
                var copy = conv
                copy.messages = copy.messages.map { msg in
                    var m = msg
                    m.steps = m.steps.map { step in
                        guard step.status == .running || step.status == .waitingTouchID else { return step }
                        var s = step
                        s.status = .failed
                        if s.note == nil { s.note = "Interrupted" }
                        return s
                    }
                    return m
                }
                return copy
            }
            .sorted { $0.updatedAt > $1.updatedAt }
        return snapshot
    }

    /// Save snapshot atomically with `0600` permissions, bounded to `maxConversations` and `maxMessagesPerConversation`.
    public func save(_ snapshot: FairyHistorySnapshot) throws {
        let dir = url.deletingLastPathComponent()
        try FileManager.default.createDirectory(
            at: dir,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )

        var bounded = snapshot
        bounded.conversations = snapshot.conversations
            .sorted { $0.updatedAt > $1.updatedAt }
            .prefix(Self.maxConversations)
            .map { conv in
                var copy = conv
                if copy.messages.count > Self.maxMessagesPerConversation {
                    copy.messages = Array(copy.messages.suffix(Self.maxMessagesPerConversation))
                }
                return copy
            }

        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        let data = try encoder.encode(bounded)
        try data.write(to: url, options: .atomic)
        try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
    }

    // MARK: - Helpers

    /// Derive a concise, single-line conversation title from a user's first prompt.
    public static func deriveTitle(from prompt: String) -> String {
        let collapsed = prompt
            .split(whereSeparator: \.isWhitespace)
            .joined(separator: " ")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard !collapsed.isEmpty else { return "New conversation" }
        if collapsed.count <= 64 { return collapsed }
        return String(collapsed.prefix(63)).trimmingCharacters(in: .whitespaces) + "…"
    }

    /// Filter conversations by substring match across title, host, message text, and tool step titles/names.
    public static func filter(_ conversations: [FairyConversation], query: String) -> [FairyConversation] {
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return conversations }
        return conversations.filter { conv in
            if conv.title.localizedCaseInsensitiveContains(trimmed) { return true }
            if conv.hostName.localizedCaseInsensitiveContains(trimmed) { return true }
            return conv.messages.contains { msg in
                if msg.text.localizedCaseInsensitiveContains(trimmed) { return true }
                return msg.steps.contains {
                    $0.title.localizedCaseInsensitiveContains(trimmed)
                        || $0.tool.localizedCaseInsensitiveContains(trimmed)
                }
            }
        }
    }

    /// Build a bounded recap of recent turns for continuing an older saved thread on a fresh Fairy session.
    public static func continuationSummary(for messages: [FairyChatMessage], maxChars: Int = 2200) -> String? {
        let substantive = messages.filter { msg in
            msg.role != .error && !msg.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
        guard !substantive.isEmpty else { return nil }

        var parts: [String] = []
        var used = 0
        for msg in substantive.reversed() {
            let role = msg.role == .user ? "User" : "Fairy"
            let clean = msg.text
                .split(whereSeparator: \.isNewline)
                .joined(separator: " ")
                .trimmingCharacters(in: .whitespacesAndNewlines)
            let clipped = clean.count > 420 ? String(clean.prefix(419)) + "…" : clean
            let stepSuffix = msg.steps.isEmpty ? "" : " [\(msg.steps.count) tool steps]"
            let entry = "- \(role)\(stepSuffix): \(clipped)"
            let needed = entry.count + (parts.isEmpty ? 0 : 1)
            if used + needed > maxChars {
                if parts.isEmpty {
                    let room = max(40, maxChars)
                    parts.append(String(entry.prefix(room - 1)) + "…")
                }
                break
            }
            parts.append(entry)
            used += needed
        }
        guard !parts.isEmpty else { return nil }
        return "Prior conversation context (user resumed saved thread):\n" + parts.reversed().joined(separator: "\n")
    }
}
