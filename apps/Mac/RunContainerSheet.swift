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
    @State private var cpus = 2
    @State private var memoryMib: UInt64 = 1024
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
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Run container").font(.system(size: 16, weight: .semibold))
                Text("Each container boots its own VM. Its output goes to its logs.").font(.system(size: 12.5)).foregroundStyle(G.muted)
            }

            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 12) {
                GridRow {
                    FieldLabel("Image")
                    HStack(spacing: 6) {
                        SheetField(placeholder: "e.g. python:3.13-slim", text: $image, mono: true)
                            .focused($imageFocused)
                        Menu {
                            if store.images.isEmpty {
                                Text("No images pulled yet")
                            }
                            ForEach(store.images) { img in
                                Button("\(img.repository):\(img.tag)") { image = "\(img.repository):\(img.tag)" }
                            }
                        } label: {
                            Image(systemName: "chevron.down")
                        }
                        .menuStyle(.borderlessButton)
                        .menuIndicator(.hidden)
                        .fixedSize()
                        .help("Images already pulled")
                    }
                }
                GridRow {
                    FieldLabel("Name")
                    SheetField(placeholder: "Optional: the image name plus a short suffix", text: $name, mono: false)
                }
                GridRow {
                    FieldLabel("Command")
                    SheetField(placeholder: "Optional: the image's default command", text: $command, mono: true)
                }
                GridRow(alignment: .top) {
                    FieldLabel("Environment").padding(.top, 8)
                    VStack(alignment: .leading, spacing: 4) {
                        TextEditor(text: $envText)
                            .font(G.mono(12.5))
                            .scrollContentBackground(.hidden)
                            .padding(.horizontal, 8)
                            .padding(.vertical, 6)
                            .frame(height: 64)
                            .background(G.background, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                            .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(badEnv == nil ? G.borderStrong : G.danger))
                        Text(badEnv.map { "“\($0)” isn't KEY=value" } ?? "One KEY=value per line, added to the image's own.")
                            .font(.system(size: 11.5))
                            .foregroundStyle(badEnv == nil ? G.faint : G.danger)
                    }
                }
                GridRow {
                    FieldLabel("VM")
                    HStack(spacing: 18) {
                        Stepper("\(cpus) vCPU", value: $cpus, in: 1...maxCPUs)
                            .fixedSize()
                        Picker("Memory", selection: $memoryMib) {
                            ForEach(Self.memoryChoices, id: \.self) { mib in
                                Text(mib < 1024 ? "\(mib) MB" : "\(mib / 1024) GB").tag(mib)
                            }
                        }
                        .labelsHidden()
                        .fixedSize()
                        Text("memory").foregroundStyle(G.muted)
                    }
                    .font(.system(size: 13))
                }
            }

            Group {
                if let since = running {
                    HStack(spacing: 10) {
                        ProgressView().controlSize(.small)
                        TimelineView(.periodic(from: since, by: 1)) { context in
                            Text("Pulling if needed and booting a VM… \(Int(context.date.timeIntervalSince(since))) s")
                        }
                    }
                    .foregroundStyle(G.textSecondary)
                } else if let failure {
                    Label(failure, systemImage: "xmark.octagon.fill")
                        .foregroundStyle(G.danger)
                        .textSelection(.enabled)
                } else if !store.canManage {
                    Label("Start gravitond to run containers: scripts/dev-gravitond.sh", systemImage: "exclamationmark.triangle")
                        .foregroundStyle(G.warning)
                }
            }
            .font(.system(size: 12.5))
            .fixedSize(horizontal: false, vertical: true)

            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                    .disabled(running != nil)
                Button("Run", action: run)
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canRun)
            }
        }
        .padding(22)
        .frame(width: 560)
        .background(Color(hex: 0x15161A))
        .onAppear {
            if image.isEmpty { image = initialImage }
            imageFocused = image.isEmpty
        }
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
            memoryMib: memoryMib
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

private struct FieldLabel: View {
    var text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text).font(.system(size: 12, weight: .medium)).foregroundStyle(G.textSecondary)
            .gridColumnAlignment(.trailing)
    }
}

private struct SheetField: View {
    var placeholder: String
    @Binding var text: String
    var mono: Bool

    var body: some View {
        TextField(placeholder, text: $text)
            .textFieldStyle(.plain)
            .font(mono ? G.mono(13) : .system(size: 13))
            .padding(.horizontal, 12)
            .frame(height: 34)
            .background(G.background, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(G.borderStrong))
    }
}
