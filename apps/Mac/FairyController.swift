import AppKit
import Foundation
import GravitonKit
import LocalAuthentication
import Observation

// MARK: - Touch ID Authentication

protocol BiometricAuthenticator: Sendable {
    var prefersTouchID: Bool { get }
    func authenticate(reason: String) async throws
    func cancelPending()
}

final class TouchIDAuthenticator: BiometricAuthenticator, @unchecked Sendable {
    private let lock = NSLock()
    private var activeContext: LAContext?

    var prefersTouchID: Bool {
        let ctx = LAContext()
        var err: NSError?
        return ctx.canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, error: &err) && ctx.biometryType == .touchID
    }

    func cancelPending() {
        lock.lock()
        let ctx = activeContext
        activeContext = nil
        lock.unlock()
        ctx?.invalidate()
    }

    private func setActiveContext(_ ctx: LAContext?) {
        lock.lock()
        let prev = activeContext
        activeContext = ctx
        lock.unlock()
        if let prev, prev !== ctx {
            prev.invalidate()
        }
    }

    private func clearActiveContextIfSame(_ ctx: LAContext) {
        lock.lock()
        if activeContext === ctx {
            activeContext = nil
        }
        lock.unlock()
    }

    func authenticate(reason: String) async throws {
        try await Task.detached(priority: .userInitiated) { [self] in
            let context = LAContext()
            context.localizedCancelTitle = "Deny"
            var evalError: NSError?
            guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &evalError) else {
                throw EngineError.daemon(evalError?.localizedDescription ?? "Touch ID or Mac authentication is unavailable.")
            }
            self.setActiveContext(context)
            defer { self.clearActiveContextIfSame(context) }
            do {
                try await context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: reason)
            } catch let laError as LAError {
                switch laError.code {
                case .userCancel:
                    throw EngineError.daemon("Authentication cancelled — click Approve to try again or Deny to reject.")
                case .appCancel, .systemCancel:
                    throw EngineError.daemon("Authentication was interrupted — click Approve to retry.")
                case .biometryLockout:
                    throw EngineError.daemon("Touch ID is locked — enter your Mac password to approve.")
                case .passcodeNotSet:
                    throw EngineError.daemon("Set a Mac login password in System Settings to approve sensitive actions.")
                default:
                    throw EngineError.daemon(laError.localizedDescription)
                }
            }
        }.value
    }
}

// MARK: - Pending Approval UI Model

struct FairyPendingApproval: Identifiable {
    let id: UUID
    let summary: FairyActionSummary
    let hostName: String
    let tool: String
    var isAuthenticating: Bool = false
    var authError: String?
}

// MARK: - Controller

@MainActor
@Observable
final class FairyController {
    private static let speakDefaultsKey = "graviton.fairy.speakReplies"

    var isOpen = false
    var fairyName = "Fairy"
    var fairyOnline = false
    var fairyInstalled = FairyPaths.isFairyInstalled
    var isAnswering = false
    var isWaking = false
    var isSpeaking = false
    var speakReplies: Bool
    var messages: [FairyChatMessage] = []
    var conversations: [FairyConversation] = []
    var activeConversationID: UUID?
    var showHistory = false
    var pendingApproval: FairyPendingApproval?

    private let store: EngineStore
    private let hosts: SharedHosts
    private let shells: ShellSessions
    private let router: AppRouter
    private let authenticator: any BiometricAuthenticator
    private let historyStore: FairyHistoryStore
    private let linkClient = FairyLinkClient()
    private let toolServer = FairyToolServer()

    private var activeTask: Task<Void, Never>?
    private var activeGrantToken: String?
    private var activeAssistantMessageID: UUID?
    private var lastSyncedConversationID: UUID?
    private var needsFreshFairySession = false
    private var approvalContinuation: CheckedContinuation<Bool, Never>?
    private var approvalWaiters: [CheckedContinuation<Void, Never>] = []
    private var approvalBusy = false

    init(
        store: EngineStore,
        hosts: SharedHosts,
        shells: ShellSessions,
        router: AppRouter,
        authenticator: any BiometricAuthenticator = TouchIDAuthenticator(),
        historyStore: FairyHistoryStore = FairyHistoryStore()
    ) {
        self.store = store
        self.hosts = hosts
        self.shells = shells
        self.router = router
        self.authenticator = authenticator
        self.historyStore = historyStore
        self.speakReplies = UserDefaults.standard.object(forKey: Self.speakDefaultsKey) as? Bool ?? true

        let snapshot = historyStore.load()
        self.conversations = snapshot.conversations
        if let savedID = snapshot.activeConversationID,
           let conv = snapshot.conversations.first(where: { $0.id == savedID }) {
            self.activeConversationID = conv.id
            self.messages = conv.messages
        } else if let latest = snapshot.conversations.first {
            self.activeConversationID = latest.id
            self.messages = latest.messages
        }
        self.needsFreshFairySession = !self.messages.isEmpty

        do {
            try toolServer.start { [weak self] tool, args in
                guard let self else { throw EngineError.daemon("Graviton is closing.") }
                return try await self.handleToolCall(tool: tool, args: args)
            }
        } catch {
            // If another instance already bound the socket or permissions prevented it,
            // surface the status cleanly when status/ask is checked.
            messages.append(FairyChatMessage(role: .error, text: "Couldn't open Fairy tool socket: \(error.localizedDescription)"))
        }

        NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor in
                self?.shutdown()
            }
        }

        Task { await refreshStatus() }
    }

    var approveButtonLabel: String {
        authenticator.prefersTouchID ? "Approve with Touch ID" : "Approve with Password"
    }

    var approveButtonSymbol: String {
        authenticator.prefersTouchID ? "touchid" : "lock.shield"
    }

    var activeHostDisplay: String {
        if let name = store.liveName, !name.isEmpty { return name }
        return hosts.current?.name ?? "This Mac"
    }

    func toggle() {
        isOpen.toggle()
        if isOpen { Task { await refreshStatus() } }
    }

    func refreshStatus() async {
        fairyInstalled = FairyPaths.isFairyInstalled
        let probe = await linkClient.probe()
        fairyOnline = probe.online
        if let name = probe.name, !name.isEmpty { fairyName = name }
    }

    func toggleSpeakReplies() {
        speakReplies.toggle()
        UserDefaults.standard.set(speakReplies, forKey: Self.speakDefaultsKey)
        if !speakReplies && isSpeaking {
            stop()
        }
    }

    func openConversation(_ id: UUID) {
        if activeConversationID == id {
            showHistory = false
            return
        }
        stop()
        guard let conv = conversations.first(where: { $0.id == id }) else { return }
        activeConversationID = id
        messages = conv.messages
        showHistory = false
        needsFreshFairySession = (lastSyncedConversationID != id)
        persistHistory()
    }

    func newChat() {
        stop()
        activeConversationID = nil
        messages = []
        showHistory = false
        needsFreshFairySession = true
        persistHistory()
        Task { try? await linkClient.reset() }
    }

    func deleteConversation(_ id: UUID) {
        if activeConversationID == id {
            stop()
            activeConversationID = nil
            messages = []
            lastSyncedConversationID = nil
            needsFreshFairySession = true
        }
        conversations.removeAll { $0.id == id }
        persistHistory()
    }

    func clearAllHistory() {
        stop()
        conversations = []
        activeConversationID = nil
        messages = []
        lastSyncedConversationID = nil
        needsFreshFairySession = true
        persistHistory()
        Task { try? await linkClient.reset() }
    }

    func copyTranscript(for conversationID: UUID? = nil) {
        let targetID = conversationID ?? activeConversationID
        let conv = targetID.flatMap { id in conversations.first(where: { $0.id == id }) }
        let transcript = conv?.plainTranscript ?? FairyConversation(
            title: "Fairy Conversation",
            hostName: activeHostDisplay,
            messages: messages
        ).plainTranscript
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(transcript, forType: .string)
    }

    func stop() {
        authenticator.cancelPending()
        activeTask?.cancel()
        activeTask = nil
        linkClient.stop()
        if let tok = activeGrantToken {
            toolServer.revoke(tok)
            activeGrantToken = nil
        }
        if let msgID = activeAssistantMessageID {
            updateAssistantMessage(id: msgID) { msg in
                for i in msg.steps.indices where msg.steps[i].status == .running || msg.steps[i].status == .waitingTouchID {
                    msg.steps[i].status = .failed
                    msg.steps[i].note = "Stopped"
                }
            }
            activeAssistantMessageID = nil
            syncActiveConversationAndSave()
        }
        settlePendingApproval(allowed: false)
        isAnswering = false
        isWaking = false
        isSpeaking = false
    }

    func shutdown() {
        stop()
        toolServer.stop()
    }

    func send(_ rawText: String) {
        let prompt = rawText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !prompt.isEmpty, !isAnswering else { return }
        if !isOpen { isOpen = true }
        if showHistory { showHistory = false }

        let priorMessages = messages
        if activeConversationID == nil {
            let conv = FairyConversation(
                title: FairyHistoryStore.deriveTitle(from: prompt),
                hostName: activeHostDisplay
            )
            conversations.insert(conv, at: 0)
            activeConversationID = conv.id
        }

        messages.append(FairyChatMessage(role: .user, text: prompt))
        let assistantID = UUID()
        messages.append(FairyChatMessage(id: assistantID, role: .assistant, text: ""))
        activeAssistantMessageID = assistantID
        syncActiveConversationAndSave()

        isAnswering = true
        isWaking = false

        let shouldResetFairy = needsFreshFairySession || (lastSyncedConversationID != activeConversationID)
        let continuation = shouldResetFairy ? FairyHistoryStore.continuationSummary(for: priorMessages) : nil
        let context = buildContext(continuation: continuation)
        let grant = toolServer.grant()
        activeGrantToken = grant?.token
        let speak = speakReplies

        activeTask = Task { [weak self] in
            guard let self else { return }
            defer {
                if let tok = self.activeGrantToken {
                    self.toolServer.revoke(tok)
                    self.activeGrantToken = nil
                }
                self.settlePendingApproval(allowed: false)
                self.isAnswering = false
                self.isWaking = false
                self.isSpeaking = false
                self.activeAssistantMessageID = nil
                self.activeTask = nil
                self.syncActiveConversationAndSave()
            }

            do {
                for try await event in self.linkClient.ask(text: prompt, context: context, fresh: shouldResetFairy, speak: speak, tools: grant) {
                    if Task.isCancelled { break }
                    switch event {
                    case .waking:
                        self.isWaking = true
                    case let .hello(name):
                        self.isWaking = false
                        self.fairyOnline = true
                        if !name.isEmpty { self.fairyName = name }
                    case let .text(soFar):
                        self.isWaking = false
                        self.updateAssistantMessage(id: assistantID) { $0.text = soFar }
                    case let .done(final):
                        self.isWaking = false
                        self.lastSyncedConversationID = self.activeConversationID
                        self.needsFreshFairySession = false
                        self.updateAssistantMessage(id: assistantID) { $0.text = final }
                        self.syncActiveConversationAndSave()
                        if let tok = self.activeGrantToken {
                            self.toolServer.revoke(tok)
                            self.activeGrantToken = nil
                        }
                        self.isSpeaking = speak
                    case .spoken:
                        self.isSpeaking = false
                    }
                }
            } catch {
                if !Task.isCancelled {
                    self.updateAssistantMessage(id: assistantID) { msg in
                        msg.role = .error
                        msg.text = error.localizedDescription
                    }
                    self.syncActiveConversationAndSave()
                }
            }
            await self.refreshStatus()
        }
    }

    // MARK: - Touch ID Approval Flow

    func approvePendingWithTouchID() {
        guard var pending = pendingApproval, !pending.isAuthenticating else { return }
        pending.isAuthenticating = true
        pending.authError = nil
        pendingApproval = pending

        Task { [weak self] in
            guard let self else { return }
            do {
                try await self.authenticator.authenticate(reason: pending.summary.touchIDReason)
                self.settlePendingApproval(allowed: true)
            } catch {
                if var current = self.pendingApproval, current.id == pending.id {
                    current.isAuthenticating = false
                    current.authError = error.localizedDescription
                    self.pendingApproval = current
                }
            }
        }
    }

    func denyPending() {
        authenticator.cancelPending()
        settlePendingApproval(allowed: false)
    }

    private func settlePendingApproval(allowed: Bool) {
        pendingApproval = nil
        if let continuation = approvalContinuation {
            approvalContinuation = nil
            continuation.resume(returning: allowed)
        }
    }

    private func acquireApprovalSlot() async {
        if approvalBusy {
            await withCheckedContinuation { cont in
                approvalWaiters.append(cont)
            }
        }
        approvalBusy = true
    }

    private func releaseApprovalSlot() {
        if !approvalWaiters.isEmpty {
            let next = approvalWaiters.removeFirst()
            next.resume()
        } else {
            approvalBusy = false
        }
    }

    private func requestApprovalIfNeeded(tool: String, args: [String: FairyJSON], stepID: UUID) async throws -> Bool {
        let summary = FairyApprovalPolicy.summarize(tool: tool, args: args, host: activeHostDisplay)
        guard FairyApprovalPolicy.requiresTouchID(tool: tool, args: args) else {
            return false
        }

        await acquireApprovalSlot()
        defer { releaseApprovalSlot() }

        if Task.isCancelled { throw CancellationError() }

        updateStep(stepID) {
            $0.status = .waitingTouchID
            $0.title = summary.title
        }
        if !isOpen { isOpen = true }
        NSApp.activate(ignoringOtherApps: true)

        let approved = await withTaskCancellationHandler {
            await withCheckedContinuation { (continuation: CheckedContinuation<Bool, Never>) in
                pendingApproval = FairyPendingApproval(
                    id: UUID(),
                    summary: summary,
                    hostName: activeHostDisplay,
                    tool: tool
                )
                approvalContinuation = continuation
            }
        } onCancel: { [weak self] in
            Task { @MainActor in
                self?.settlePendingApproval(allowed: false)
            }
        }

        guard approved else {
            updateStep(stepID) {
                $0.status = .denied
                $0.note = "Denied or cancelled"
            }
            throw EngineError.daemon("User denied Touch ID approval for \"\(summary.title)\".")
        }
        return true
    }

    // MARK: - Context & Step Helpers

    private func buildContext(continuation: String? = nil) -> FairyAskContext {
        let selected = store.container(router.selectedContainerID)
        var summaryItems = [
            "Containers: \(store.runningCount) running / \(store.containers.count) total",
            "Stacks: \(store.stacks.map { "\($0.name) (\($0.runningCount)/\($0.containers.count))" }.joined(separator: ", "))",
            "Images: \(store.images.count) · Volumes: \(store.volumes.count)",
        ]
        if let e = store.engine, e.memoryBytes > 0 {
            if e.hasHostMemory {
                summaryItems.append("Memory: \(Format.bytes(e.totalMemoryUsedBytes)) / \(Format.bytes(e.memoryBytes)) (VMs \(Format.bytes(e.vmMemoryUsedBytes)), macOS \(Format.bytes(e.otherHostMemoryUsedBytes)))")
            } else {
                summaryItems.append("VM memory: \(Format.bytes(e.vmMemoryUsedBytes)) / \(Format.bytes(e.memoryBytes))")
            }
            summaryItems.append("Engine: \(e.kernel) · \(e.arch) · \(e.cpus) vCPU\(e.rosetta == true ? " · Rosetta" : "")")
        }
        var summaryLines = summaryItems.joined(separator: "\n")
        if let continuation, !continuation.isEmpty {
            summaryLines += "\n\n" + continuation
        }

        var logsTail: String?
        if let sel = selected, let lines = store.logs[sel.id], !lines.isEmpty {
            logsTail = lines.suffix(25).map { "\($0.level.label): \($0.message)" }.joined(separator: "\n")
        }

        return FairyAskContext(
            host: activeHostDisplay,
            connection: "\(store.connection)",
            section: router.activeSection,
            selectedContainer: selected.map { "\($0.name) (\($0.status.rawValue), \($0.image))" },
            summary: summaryLines,
            logsTail: logsTail
        )
    }

    private func syncActiveConversationAndSave() {
        guard let activeID = activeConversationID,
              let idx = conversations.firstIndex(where: { $0.id == activeID }) else { return }
        conversations[idx].messages = messages
        conversations[idx].updatedAt = .now
        conversations[idx].hostName = activeHostDisplay
        let updated = conversations.remove(at: idx)
        conversations.insert(updated, at: 0)
        persistHistory()
    }

    private func persistHistory() {
        let snapshot = FairyHistorySnapshot(activeConversationID: activeConversationID, conversations: conversations)
        try? historyStore.save(snapshot)
    }

    private func appendStep(_ step: FairyToolStep) {
        guard let msgID = activeAssistantMessageID else { return }
        updateAssistantMessage(id: msgID) { $0.steps.append(step) }
        syncActiveConversationAndSave()
    }

    private func updateStep(_ stepID: UUID, mutate: (inout FairyToolStep) -> Void) {
        guard let msgID = activeAssistantMessageID else { return }
        updateAssistantMessage(id: msgID) { msg in
            if let idx = msg.steps.firstIndex(where: { $0.id == stepID }) {
                mutate(&msg.steps[idx])
            }
        }
    }

    private func updateAssistantMessage(id: UUID, mutate: (inout FairyChatMessage) -> Void) {
        guard let idx = messages.firstIndex(where: { $0.id == id }) else { return }
        mutate(&messages[idx])
    }

    // MARK: - Tool Dispatcher

    private func handleToolCall(tool: String, args: [String: FairyJSON]) async throws -> FairyJSON {
        let summary = FairyApprovalPolicy.summarize(tool: tool, args: args, host: activeHostDisplay)
        let stepID = UUID()
        appendStep(FairyToolStep(id: stepID, tool: tool, title: summary.title, status: .running))

        do {
            let touchVerified = try await requestApprovalIfNeeded(tool: tool, args: args, stepID: stepID)
            updateStep(stepID) {
                $0.status = .running
                $0.touchIDVerified = touchVerified
            }
            let result = try await executeTool(tool: tool, args: args)
            updateStep(stepID) {
                $0.status = .done
                $0.touchIDVerified = touchVerified
            }
            syncActiveConversationAndSave()
            return result
        } catch {
            updateStep(stepID) {
                if $0.status != .denied {
                    $0.status = .failed
                    $0.note = error.localizedDescription
                }
            }
            syncActiveConversationAndSave()
            throw error
        }
    }

    private func resolveContainer(_ query: String) throws -> Container {
        let key = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !key.isEmpty else { throw EngineError.daemon("Container name or ID is required.") }
        if let exact = store.containers.first(where: { $0.name == key || $0.id == key }) {
            return exact
        }
        let ci = store.containers.filter { $0.name.caseInsensitiveCompare(key) == .orderedSame }
        if ci.count == 1 { return ci[0] }
        let prefixMatches = store.containers.filter { $0.id.lowercased().hasPrefix(key.lowercased()) }
        if prefixMatches.count == 1 { return prefixMatches[0] }
        if prefixMatches.count > 1 {
            throw EngineError.daemon("Ambiguous container ID prefix \"\(key)\".")
        }
        throw EngineError.daemon("No container named or matching \"\(key)\" on \(activeHostDisplay).")
    }

    private func requireString(_ args: [String: FairyJSON], _ key: String) throws -> String {
        guard let s = args[key]?.stringValue?.trimmingCharacters(in: .whitespacesAndNewlines), !s.isEmpty else {
            throw EngineError.daemon("Missing required parameter \"\(key)\".")
        }
        return s
    }

    private func expandPath(_ raw: String) -> String {
        (raw as NSString).expandingTildeInPath
    }

    private func executeTool(tool: String, args: [String: FairyJSON]) async throws -> FairyJSON {
        switch tool {
        case "status":
            await store.refresh()
            return try FairyJSON.fromEncodable(StatusPayload(
                host: activeHostDisplay,
                isLocal: store.isLocal,
                connection: "\(store.connection)",
                liveError: store.liveError,
                pairedHosts: hosts.hosts.map { "\($0.name) (\($0.hostName), \($0.address))" },
                engine: store.engine,
                containersTotal: store.containers.count,
                containersRunning: store.runningCount,
                stacks: store.stacks.map { "\($0.name) (\($0.runningCount)/\($0.containers.count) running)" },
                imagesCount: store.images.count,
                volumesCount: store.volumes.count
            ))

        case "events":
            let limit = args["limit"]?.intValue ?? 50
            await store.refresh()
            return try FairyJSON.fromEncodable(["events": Array(store.events.suffix(max(1, min(200, limit))))])

        case "switch_host":
            let requested = try requireString(args, "host")
            let normalized = requested.lowercased()
            if normalized == "local" || normalized == "this mac" {
                switchHost(to: nil, store: store, hosts: hosts, shells: shells)
            } else if let mac = hosts.hosts.first(where: {
                $0.name.caseInsensitiveCompare(requested) == .orderedSame
                    || $0.hostName.caseInsensitiveCompare(requested) == .orderedSame
            }) {
                switchHost(to: mac, store: store, hosts: hosts, shells: shells)
            } else {
                let available = (["local"] + hosts.hosts.map(\.name)).joined(separator: ", ")
                throw EngineError.daemon("Unknown host \"\(requested)\". Available hosts: \(available).")
            }
            try? await Task.sleep(for: .milliseconds(350))
            await store.refresh()
            return .object(["active_host": .string(activeHostDisplay), "connection": .string("\(store.connection)")])

        case "navigate":
            let sec = try requireString(args, "section").lowercased()
            switch sec {
            case "containers":
                if let cQuery = args["container"]?.stringValue, !cQuery.isEmpty {
                    let c = try resolveContainer(cQuery)
                    let tabRaw = args["tab"]?.stringValue?.lowercased() ?? "logs"
                    let tab = DetailTab.allCases.first { $0.rawValue.lowercased() == tabRaw } ?? .logs
                    router.pending = .container(c.id, tab)
                } else {
                    router.pending = .containers
                }
            case "images": router.pending = .images
            case "volumes": router.pending = .volumes
            case "networks": router.pending = .networks
            default: throw EngineError.daemon("Unknown section \"\(sec)\".")
            }
            return .object(["navigated": .string(sec)])

        case "containers":
            await store.refresh()
            let filterStr = args["filter"]?.stringValue?.lowercased() ?? "all"
            let project = args["project"]?.stringValue?.trimmingCharacters(in: .whitespacesAndNewlines)
            let filter: ContainerFilter = switch filterStr {
            case "running": .running
            case "stopped": .stopped
            default: .all
            }
            var items = filter.apply(store.containers)
            if let project, !project.isEmpty {
                items = items.filter { $0.project == project }
            }
            return try FairyJSON.fromEncodable(["containers": items])

        case "inspect":
            let c = try resolveContainer(try requireString(args, "container"))
            let includeEnv = args["include_env_values"]?.boolValue ?? false
            var details = try await store.inspect(c.id)
            details.env = FairyApprovalPolicy.redactEnv(details.env, includeValues: includeEnv)
            return try FairyJSON.fromEncodable(details)

        case "logs":
            let c = try resolveContainer(try requireString(args, "container"))
            let tail = max(1, min(500, args["tail"]?.intValue ?? 100))
            await store.loadLogs(for: c.id, tail: tail)
            var lines = store.logs[c.id] ?? []
            if let lvlStr = args["level"]?.stringValue?.lowercased(),
               let minLevel = LogLevel(rawValue: lvlStr) {
                lines = lines.filter { $0.level >= minLevel }
            }
            return .object([
                "container": .string(c.name),
                "lines": try FairyJSON.fromEncodable(Array(lines.suffix(tail))),
            ])

        case "stats":
            let c = try resolveContainer(try requireString(args, "container"))
            let stats = try await store.stats(for: c.id)
            return try FairyJSON.fromEncodable(StatsSummaryPayload(
                id: stats.id,
                container: c.name,
                cpus: stats.cpus,
                latestSample: stats.samples.last,
                processes: stats.processes
            ))

        case "container_action":
            let c = try resolveContainer(try requireString(args, "container"))
            let action = try requireString(args, "action").lowercased()
            switch action {
            case "start":
                let updated = try await store.performStart(c)
                return try FairyJSON.fromEncodable(updated)
            case "stop":
                let updated = try await store.performStop(c)
                return try FairyJSON.fromEncodable(updated)
            case "restart":
                let updated = try await store.performRestart(c)
                return try FairyJSON.fromEncodable(updated)
            case "remove":
                let removed = try await store.performRemove(c)
                return .object(["removed_id": .string(removed), "name": .string(c.name)])
            default:
                throw EngineError.daemon("Unknown container action \"\(action)\".")
            }

        case "run":
            guard store.canManage else {
                throw EngineError.daemon("Running containers requires a live connection to gravitond.")
            }
            let image = try requireString(args, "image")
            let restart: RestartPolicy? = args["restart"]?.stringValue.flatMap(RestartPolicy.init(rawValue:))
            let spec = RunSpec(
                image: image,
                name: args["name"]?.stringValue,
                command: args["command"]?.stringArrayValue,
                env: args["env"]?.stringArrayValue ?? [],
                cpus: args["cpus"]?.intValue.flatMap(UInt32.init(exactly:)),
                memoryMib: args["memory_mib"]?.intValue.flatMap(UInt64.init(exactly:)),
                ports: args["ports"]?.stringArrayValue ?? [],
                volumes: args["volumes"]?.stringArrayValue ?? [],
                restart: restart,
                platform: args["platform"]?.stringValue,
                entrypoint: args["entrypoint"]?.stringArrayValue,
                workingDir: args["working_dir"]?.stringValue,
                user: args["user"]?.stringValue,
                hostname: args["hostname"]?.stringValue,
                project: args["project"]?.stringValue,
                service: args["service"]?.stringValue
            )
            let created = try await store.run(spec)
            return try FairyJSON.fromEncodable(created)

        case "exec":
            let c = try resolveContainer(try requireString(args, "container"))
            guard c.isRunning else { throw EngineError.daemon("Container \(c.name) isn't running.") }
            guard let command = args["command"]?.stringArrayValue, !command.isEmpty else {
                throw EngineError.daemon("`command` must be a non-empty array of strings.")
            }
            let env = args["env"]?.stringArrayValue ?? []
            let res = try await store.execCommand(id: c.id, command: command, env: env)
            return .object([
                "container": .string(c.name),
                "exit_code": .int(Int64(res.exitCode)),
                "output": .string(res.output),
                "truncated": .bool(res.truncated),
            ])

        case "stack_action":
            let project = try requireString(args, "project")
            let action = try requireString(args, "action").lowercased()
            let members = store.containers.filter { $0.project == project }
            guard !members.isEmpty else { throw EngineError.daemon("No Compose stack named \"\(project)\".") }
            switch action {
            case "start": await store.startStack(project)
            case "stop": await store.stopStack(project)
            case "restart": await store.restartStack(project)
            default: throw EngineError.daemon("Unknown stack action \"\(action)\".")
            }
            let updated = store.containers.filter { $0.project == project }
            return .object([
                "project": .string(project),
                "running": .int(Int64(updated.filter(\.isRunning).count)),
                "total": .int(Int64(updated.count)),
            ])

        case "compose":
            return try await runComposeCLI(args: args)

        case "list_files":
            let c = try resolveContainer(try requireString(args, "container"))
            let path = args["path"]?.stringValue ?? "/"
            let listing = try await store.listFiles(in: c.id, path: path)
            return try FairyJSON.fromEncodable(listing)

        case "read_file":
            let c = try resolveContainer(try requireString(args, "container"))
            let path = try requireString(args, "path")
            let file = try await store.readFile(in: c.id, path: path)
            return .object([
                "path": .string(file.path),
                "size": .int(Int64(file.size)),
                "truncated": .bool(file.truncated),
                "is_text": .bool(file.text != nil),
                "text": file.text.map { .string($0) } ?? .null,
            ])

        case "copy_files":
            let direction = try requireString(args, "direction").lowercased()
            let c = try resolveContainer(try requireString(args, "container"))
            let localPath = expandPath(try requireString(args, "local_path"))
            let containerPath = try requireString(args, "container_path")
            let localURL = URL(fileURLWithPath: localPath)
            switch direction {
            case "upload":
                let copied = try await store.uploadFiles(to: c.id, destinationDir: containerPath, urls: [localURL])
                return .object(["direction": .string("upload"), "files": .int(Int64(copied))])
            case "download":
                let tempTar = FileManager.default.temporaryDirectory.appending(path: "graviton-fairy-\(UUID().uuidString).tar")
                defer { try? FileManager.default.removeItem(at: tempTar) }
                let (isDir, archiveName) = try await store.downloadArchive(from: c.id, path: containerPath, to: tempTar)
                try await Task.detached {
                    try BuildUpload.extractArchive(tempTar, isDir: isDir, archiveName: archiveName, to: localURL)
                }.value
                return .object(["direction": .string("download"), "saved_to": .string(localPath), "is_directory": .bool(isDir)])
            default:
                throw EngineError.daemon("Unknown copy direction \"\(direction)\".")
            }

        case "images":
            await store.refresh()
            return try FairyJSON.fromEncodable(["images": store.images])

        case "pull":
            let ref = try requireString(args, "reference")
            let platform = args["platform"]?.stringValue
            let img = try await store.pull(ref, platform: platform)
            return try FairyJSON.fromEncodable(img)

        case "build":
            guard store.canBuild else {
                throw EngineError.daemon("Building images requires a live connection to gravitond.")
            }
            let context = expandPath(try requireString(args, "context"))
            let spec = BuildSpec(
                context: context,
                dockerfile: args["dockerfile"]?.stringValue,
                tag: args["tag"]?.stringValue,
                buildArgs: args["build_args"]?.stringArrayValue ?? [],
                target: args["target"]?.stringValue,
                noCache: args["no_cache"]?.boolValue ?? false,
                platform: args["platform"]?.stringValue
            )
            var logLines: [String] = []
            var built: ImageInfo?
            for try await event in store.build(spec) {
                switch event {
                case let .log(line):
                    logLines.append(line)
                    if logLines.count > 80 { logLines.removeFirst(logLines.count - 80) }
                case let .built(img):
                    built = img
                }
            }
            return try FairyJSON.fromEncodable(BuildSummaryPayload(image: built, logTail: logLines))

        case "remove_image":
            let ref = try requireString(args, "reference")
            let freed = try await store.removeImage(reference: ref)
            return .object(["removed": .string(ref), "freed_bytes": .int(Int64(freed))])

        case "tag_image":
            let source = try requireString(args, "source")
            let target = try requireString(args, "target")
            let tagged = try await store.tagImage(source: source, target: target)
            return try FairyJSON.fromEncodable(tagged)

        case "push_image":
            let ref = try requireString(args, "reference")
            let pushed = try await store.pushImage(reference: ref)
            return try FairyJSON.fromEncodable(pushed)

        case "volumes":
            await store.loadVolumes()
            return try FairyJSON.fromEncodable(["volumes": store.volumes])

        case "volume_action":
            let act = try requireString(args, "action").lowercased()
            let name = try requireString(args, "name")
            switch act {
            case "create":
                let vol = try await store.createVolume(name: name)
                return try FairyJSON.fromEncodable(vol)
            case "remove":
                let dummy = VolumeInfo(name: name, path: "", sizeBytes: 0, fileCount: 0, createdAt: 0)
                let freed = try await store.removeVolume(dummy)
                return .object(["removed": .string(name), "freed_bytes": .int(Int64(freed))])
            default:
                throw EngineError.daemon("Unknown volume action \"\(act)\".")
            }

        case "network":
            await store.refresh()
            return try FairyJSON.fromEncodable(store.network)

        case "disk_and_prune":
            let act = try requireString(args, "action").lowercased()
            switch act {
            case "usage":
                let usage = try await store.diskUsage()
                return try FairyJSON.fromEncodable(usage)
            case "prune_dry_run", "prune":
                let spec = PruneSpec(
                    images: args["images"]?.boolValue ?? false,
                    containers: args["containers"]?.boolValue ?? false,
                    dryRun: act == "prune_dry_run"
                )
                let report = try await store.prune(spec)
                return try FairyJSON.fromEncodable(report)
            default:
                throw EngineError.daemon("Unknown disk/prune action \"\(act)\".")
            }

        case "snapshots":
            let act = try requireString(args, "action").lowercased()
            let info: SnapshotInfo
            switch act {
            case "status":
                info = try await store.snapshotStatus()
            case "warm":
                info = try await store.snapshotWarm(
                    cpus: args["cpus"]?.intValue.flatMap(UInt32.init(exactly:)),
                    memoryMib: args["memory_mib"]?.intValue.flatMap(UInt64.init(exactly:)),
                    rosetta: args["rosetta"]?.boolValue ?? false,
                    slots: args["slots"]?.intValue.flatMap(UInt32.init(exactly:))
                )
            case "clear":
                info = try await store.snapshotClear()
            default:
                throw EngineError.daemon("Unknown snapshot action \"\(act)\".")
            }
            return try FairyJSON.fromEncodable(info)

        default:
            throw EngineError.daemon("Unsupported Graviton tool \"\(tool)\".")
        }
    }

    // MARK: - Compose CLI Runner

    private func resolveGravBinary() throws -> String {
        let candidates = [
            Bundle.main.bundleURL.appending(path: "Contents/MacOS/grav").path,
            "/Applications/Graviton.app/Contents/MacOS/grav",
            NSHomeDirectory() + "/.local/bin/grav",
            NSHomeDirectory() + "/.cargo/bin/grav",
            "/opt/homebrew/bin/grav",
        ]
        if let found = candidates.first(where: { FileManager.default.isExecutableFile(atPath: $0) }) {
            return found
        }
        throw EngineError.daemon("Couldn't find the `grav` CLI binary to run Compose commands.")
    }

    private func runComposeCLI(args: [String: FairyJSON]) async throws -> FairyJSON {
        let action = try requireString(args, "action").lowercased()
        let directory = expandPath(try requireString(args, "directory"))
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: directory, isDirectory: &isDir), isDir.boolValue else {
            throw EngineError.daemon("Compose directory \"\(directory)\" doesn't exist.")
        }

        let gravPath = try resolveGravBinary()
        let hostFlag = store.isLocal ? "local" : (hosts.current?.name ?? "local")

        var argv = ["--host", hostFlag, "compose"]
        for f in args["files"]?.stringArrayValue ?? [] {
            argv.append(contentsOf: ["-f", expandPath(f)])
        }
        if let project = args["project"]?.stringValue, !project.isEmpty {
            argv.append(contentsOf: ["-p", project])
        }
        for prof in args["profiles"]?.stringArrayValue ?? [] {
            argv.append(contentsOf: ["--profile", prof])
        }

        switch action {
        case "config":
            argv.append(contentsOf: ["config", "--json"])
        case "up":
            argv.append(contentsOf: ["up", "-d"])
            if args["build"]?.boolValue == true { argv.append("--build") }
            if args["force_recreate"]?.boolValue == true { argv.append("--force-recreate") }
            argv.append(contentsOf: args["services"]?.stringArrayValue ?? [])
        case "down":
            argv.append("down")
            if args["volumes"]?.boolValue == true { argv.append("-v") }
        case "ps":
            argv.append(contentsOf: ["ps", "-a"])
        case "logs":
            argv.append("logs")
            let tail = args["tail"]?.intValue ?? 50
            argv.append(contentsOf: ["-n", "\(max(1, min(500, tail)))"])
            argv.append(contentsOf: args["services"]?.stringArrayValue ?? [])
        case "stop", "start", "restart":
            argv.append(action)
            argv.append(contentsOf: args["services"]?.stringArrayValue ?? [])
        case "build":
            argv.append("build")
            if args["no_cache"]?.boolValue == true { argv.append("--no-cache") }
            argv.append(contentsOf: args["services"]?.stringArrayValue ?? [])
        default:
            throw EngineError.daemon("Unknown Compose action \"\(action)\".")
        }

        let execArgv = argv
        let (code, output) = try await Task.detached(priority: .userInitiated) {
            let proc = Process()
            proc.executableURL = URL(fileURLWithPath: gravPath)
            proc.currentDirectoryURL = URL(fileURLWithPath: directory)
            proc.arguments = execArgv
            proc.standardInput = FileHandle.nullDevice
            let pipe = Pipe()
            proc.standardOutput = pipe
            proc.standardError = pipe
            try proc.run()
            let data = pipe.fileHandleForReading.readDataToEndOfFile()
            proc.waitUntilExit()
            let text = String(decoding: data, as: UTF8.self)
            let capped = text.count > 48_000 ? "…\n" + String(text.suffix(48_000)) : text
            return (proc.terminationStatus, capped)
        }.value

        await store.refresh()
        await store.loadVolumes()

        guard code == 0 else {
            throw EngineError.daemon(output.isEmpty ? "`grav compose \(action)` exited with \(code)." : output)
        }
        return .object([
            "action": .string(action),
            "directory": .string(directory),
            "exit_code": .int(Int64(code)),
            "output": .string(output),
        ])
    }
}

private struct StatusPayload: Encodable {
    var host: String
    var isLocal: Bool
    var connection: String
    var liveError: String?
    var pairedHosts: [String]
    var engine: EngineInfo?
    var containersTotal: Int
    var containersRunning: Int
    var stacks: [String]
    var imagesCount: Int
    var volumesCount: Int
}

private struct StatsSummaryPayload: Encodable {
    var id: String
    var container: String
    var cpus: UInt32
    var latestSample: StatSample?
    var processes: [ContainerProcess]
}

private struct BuildSummaryPayload: Encodable {
    var image: ImageInfo?
    var logTail: [String]
}
