import GravitonKit
import SwiftUI

/// Runs a new container through gravitond: pulls the image if it isn't local,
/// boots its VM, and returns once the process is running (or has failed).
struct RunContainerSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    var initialImage: String = ""
    var onStarted: (Container) -> Void = { _ in }

    @State private var image = ""
    @State private var name = ""
    @State private var command = ""
    @State private var envText = ""
    @State private var portsText = ""
    @State private var volumesText = ""
    @State private var cpus = 2
    @State private var memoryMib: UInt64 = 1024
    @State private var restartPolicy: RestartPolicy = .no
    @State private var running: Date?
    @State private var failure: String?
    @FocusState private var imageFocused: Bool

    private static let memoryChoices: [UInt64] = [512, 1024, 2048, 4096, 8192]
    private var maxCPUs: Int { max(1, ProcessInfo.processInfo.activeProcessorCount) }

    private var envPairs: [String] {
        envText.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
    }
    private var badEnv: String? { envPairs.first { !$0.contains("=") || $0.hasPrefix("=") } }
    private var trimmedImage: String { image.trimmingCharacters(in: .whitespaces) }
    private var canRun: Bool { running == nil && !trimmedImage.isEmpty && badEnv == nil && store.canManage }

    var body: some View {
        GSheet("Run container", subtitle: "Each container boots its own VM. Its output goes to its logs.", symbol: "play.circle", width: 600) {
            GFormRow("Image") {
                HStack(spacing: 8) {
                    GTextField("e.g. python:3.13-slim", text: $image)
                        .focused($imageFocused)
                        .gravitonField(mono: true, height: 34, icon: "square.stack.3d.up")
                    Menu {
                        if store.images.isEmpty {
                            Text("No images pulled yet")
                        }
                        ForEach(store.images) { img in
                            Button("\(img.repository):\(img.tag)") { image = "\(img.repository):\(img.tag)" }
                        }
                    } label: {
                        HStack(spacing: 6) {
                            Text("Pulled")
                            Image(systemName: "chevron.down").font(.system(size: 9, weight: .bold))
                        }
                        .font(.system(size: 12.5, weight: .medium))
                        .foregroundStyle(G.textSecondary)
                        .padding(.horizontal, 12)
                        .frame(height: 34)
                        .background(G.control, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(G.borderStrong))
                    }
                    .gravitonMenu()
                    .help("Images already pulled")
                }
            }
            HStack(alignment: .top, spacing: 12) {
                GFormRow("Name") { field("Optional", text: $name, mono: false) }
                GFormRow("Command") { field("The image's default", text: $command) }
            }
            HStack(alignment: .top, spacing: 12) {
                GFormRow("Ports", hint: "Default 127.0.0.1; add IP: or /udp") { field("8080:80, 0.0.0.0:8080:80, 5353:53/udp", text: $portsText) }
                GFormRow("Volumes",
                         hint: !store.isLocal && !volumesText.isEmpty ? "Folders are on \(store.liveName ?? "that Mac"), not this one." : "Folders or named volumes") {
                    field("~/code:/app, pgdata:/data", text: $volumesText)
                }
            }
            GFormRow("Environment",
                     hint: "One KEY=value per line, added to the image's own.",
                     error: badEnv.map { "“\($0)” isn't KEY=value" }) {
                TextEditor(text: $envText)
                    .scrollContentBackground(.hidden)
                    .frame(height: 58)
                    .gravitonField(mono: true, height: nil, invalid: badEnv != nil)
            }
            GFormRow("Virtual machine") {
                HStack(spacing: 14) {
                    GStepper(value: $cpus, in: 1...maxCPUs) { "\($0) vCPU" }
                    GSegmented(selection: $memoryMib, options: Self.memoryChoices.map { ($0, $0 < 1024 ? "\($0) MB" : "\($0 / 1024) GB") })
                    Text("memory").font(.system(size: 12)).foregroundStyle(G.faint)
                }
            }
            GFormRow("Restart", hint: "Bring it back on failure or when gravitond starts.") {
                GSegmented(selection: $restartPolicy, options: RestartPolicy.allCases.map { ($0, $0.label) })
            }

            if let since = running {
                TimelineView(.periodic(from: since, by: 1)) { context in
                    GStatusLine(.working, "Pulling if needed and booting a VM… \(Int(context.date.timeIntervalSince(since))) s")
                }
            } else if let failure {
                GStatusLine(.failure, failure)
            } else if !store.canManage {
                GStatusLine(.warning, "Start gravitond to run containers: scripts/dev-gravitond.sh")
            }
        } footer: {
            Spacer()
            Button("Cancel") { dismiss() }
                .buttonStyle(.gSecondary)
                .keyboardShortcut(.cancelAction)
                .disabled(running != nil)
            Button("Run", action: run)
                .buttonStyle(.gPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(!canRun)
        }
        .onAppear {
            if image.isEmpty { image = initialImage }
            imageFocused = image.isEmpty
        }
    }

    private func field(_ placeholder: String, text: Binding<String>, mono: Bool = true) -> some View {
        GTextField(placeholder, text: text).gravitonField(mono: mono, height: 34)
    }

    private func run() {
        guard canRun else { return }
        let words = RunSpec.words(command)
        let trimmedName = name.trimmingCharacters(in: .whitespaces)
        let spec = RunSpec(
            image: trimmedImage,
            name: trimmedName.isEmpty ? nil : trimmedName,
            command: words.isEmpty ? nil : words,
            env: envPairs,
            cpus: UInt32(cpus),
            memoryMib: memoryMib,
            ports: RunSpec.list(portsText),
            volumes: RunSpec.list(volumesText),
            restart: restartPolicy == .no ? nil : restartPolicy
        )
        failure = nil
        running = .now
        Task {
            do {
                let container = try await store.run(spec)
                running = nil
                onStarted(container)
                dismiss()
            } catch {
                running = nil
                failure = error.localizedDescription
            }
        }
    }
}
