import GravitonKit
import SwiftUI

/// Clones a container's overlay state and config into one or more isolated forks via APFS CoW.
struct ForkContainerSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    var container: Container
    var onForked: ([Container]) -> Void = { _ in }

    @State private var name = ""
    @State private var count = 1
    @State private var startImmediately = true
    @State private var autoRemove = false
    @State private var portsText = ""
    @State private var envText = ""
    @State private var phase: Phase = .idle
    @FocusState private var focused: Bool

    enum Phase: Equatable {
        case idle
        case forking
        case done([Container])
        case failed(String)
    }

    private var forking: Bool { if case .forking = phase { true } else { false } }

    private var envPairs: [String] {
        envText.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
    }
    private var badEnv: String? { envPairs.first { !$0.contains("=") || $0.hasPrefix("=") } }

    private var parsedPorts: (ports: [PortMapping], invalid: String?) {
        let tokens = portsText.split { $0 == "," || $0.isWhitespace }.map(String.init).filter { !$0.isEmpty }
        var out: [PortMapping] = []
        for token in tokens {
            guard let mapping = PortMapping.parse(token) else { return ([], token) }
            out.append(mapping)
        }
        return (out, nil)
    }

    private var canFork: Bool {
        !forking && store.canManage && badEnv == nil && (count > 1 || parsedPorts.invalid == nil)
    }

    var body: some View {
        GSheet(
            "Fork container",
            subtitle: "Create instant APFS copy-on-write clones from \(container.name)’s live filesystem and config.",
            symbol: "arrow.triangle.branch",
            width: 560
        ) {
            HStack(alignment: .top, spacing: 12) {
                GFormRow("Name", hint: count > 1 ? "Multiple forks use \(container.name)-fork-1…\(count)." : "Optional; defaults to \(container.name)-fork-1.") {
                    GTextField(count > 1 ? "\(container.name)-fork-1…\(count)" : "\(container.name)-fork-1", text: $name)
                        .focused($focused)
                        .disabled(forking || count > 1)
                        .onSubmit(fork)
                        .gravitonField(mono: true, height: 34)
                }
                GFormRow("Forks") {
                    GStepper(value: $count, in: 1...16) { $0 == 1 ? "1 fork" : "\($0) forks" }
                        .disabled(forking)
                }
            }

            if count == 1 {
                GFormRow(
                    "Override Ports",
                    hint: "Optional (e.g. 8081:80). Left blank to avoid host port conflicts with \(container.name).",
                    error: parsedPorts.invalid.map { "“\($0)” isn't a valid port mapping" }
                ) {
                    GTextField("Optional host:container mapping", text: $portsText)
                        .disabled(forking)
                        .gravitonField(mono: true, height: 34, invalid: parsedPorts.invalid != nil)
                }
            }

            GFormRow(
                "Environment Overrides",
                hint: "One KEY=value per line, merged on top of \(container.name)’s environment.",
                error: badEnv.map { "“\($0)” isn't KEY=value" }
            ) {
                TextEditor(text: $envText)
                    .scrollContentBackground(.hidden)
                    .disabled(forking)
                    .frame(height: 60)
                    .gravitonField(mono: true, height: nil, invalid: badEnv != nil)
            }

            HStack(spacing: 18) {
                Toggle("Start forked VM immediately", isOn: $startImmediately)
                    .toggleStyle(.checkbox)
                    .disabled(forking)
                Toggle("Auto-remove on exit (--rm)", isOn: $autoRemove)
                    .toggleStyle(.checkbox)
                    .disabled(forking)
            }
            .font(.system(size: 12.5))
            .foregroundStyle(G.textSecondary)

            switch phase {
            case .idle:
                if !store.canManage {
                    GStatusLine(.warning, "Start gravitond to fork containers.")
                }
            case .forking:
                GStatusLine(.working, container.isRunning ? "Syncing ext4 and cloning \(container.name)…" : "Cloning \(container.name)…")
            case let .done(created):
                let summary = created.map(\.name).joined(separator: ", ")
                GStatusLine(.success, "Forked \(created.count == 1 ? summary : "\(created.count) containers: \(summary)")")
            case let .failed(reason):
                GStatusLine(.failure, reason)
            }
        } footer: {
            Spacer()
            Button("Close") { dismiss() }
                .buttonStyle(.gSecondary)
                .keyboardShortcut(.cancelAction)
            Button("Fork", action: fork)
                .buttonStyle(.gPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(!canFork)
        }
        .onAppear { focused = true }
    }

    private func fork() {
        guard canFork else { return }
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let names: [String] = (count == 1 && !trimmedName.isEmpty) ? [trimmedName] : []
        let spec = ForkSpec(
            source: container.id,
            names: names,
            count: UInt32(count),
            start: startImmediately,
            autoRemove: autoRemove,
            ports: count == 1 ? parsedPorts.ports : [],
            env: envPairs,
            command: nil
        )
        phase = .forking
        Task {
            do {
                let created = try await store.fork(spec)
                phase = .done(created)
                onForked(created)
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}
