import GravitonKit
import SwiftUI

/// Run a container on the Mac this phone shows (like the Mac app's Run sheet).
struct MobileRunSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var image: String
    @State private var name = ""
    @State private var command = ""
    @State private var ports = ""
    @State private var env = ""
    @State private var memory = 1024.0
    @State private var phase: Phase = .idle
    var onRun: (Container) -> Void = { _ in }

    enum Phase {
        case idle, running
        case failed(String)
    }

    init(image: String = "", onRun: @escaping (Container) -> Void = { _ in }) {
        _image = State(initialValue: image)
        self.onRun = onRun
    }

    private var busy: Bool { if case .running = phase { true } else { false } }
    private var trimmedImage: String { image.trimmingCharacters(in: .whitespaces) }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("e.g. postgres:17-alpine", text: $image)
                        .font(G.mono(15))
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                } header: {
                    Text("Image")
                } footer: {
                    Text("Pulled first if \(store.liveName ?? "the Mac") doesn't have it.")
                }
                Section("Options") {
                    field("Name", "Optional", $name)
                    field("Command", "Optional: replaces the image's", $command)
                    field("Ports", "8080:80, 5432", $ports)
                    field("Environment", "KEY=value, KEY2=value", $env)
                    VStack(alignment: .leading) {
                        HStack {
                            Text("Memory")
                            Spacer()
                            Text(Format.bytes(UInt64(memory) << 20)).foregroundStyle(G.muted).monospacedDigit()
                        }
                        Slider(value: $memory, in: 256...8192, step: 256)
                    }
                }
                switch phase {
                case .idle:
                    EmptyView()
                case .running:
                    Section { HStack(spacing: 10) { ProgressView(); Text("Starting… (a pull can take a while)") } }
                case let .failed(message):
                    Section { Label(message, systemImage: "xmark.octagon.fill").foregroundStyle(G.danger) }
                }
            }
            .scrollContentBackground(.hidden)
            .background(G.background)
            .navigationTitle("Run container")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Run", action: run).disabled(busy || trimmedImage.isEmpty || !store.canManage)
                }
            }
            .disabled(busy)
        }
    }

    private func field(_ label: String, _ placeholder: String, _ text: Binding<String>) -> some View {
        LabeledContent(label) {
            TextField(placeholder, text: text)
                .multilineTextAlignment(.trailing)
                .font(G.mono(14))
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
        }
    }

    private func run() {
        let words = RunSpec.words(command)
        let spec = RunSpec(image: trimmedImage, name: name.trimmingCharacters(in: .whitespaces).isEmpty ? nil : name.trimmingCharacters(in: .whitespaces),
                           command: words.isEmpty ? nil : words, env: RunSpec.list(env), memoryMib: UInt64(memory), ports: RunSpec.list(ports))
        phase = .running
        Task {
            do {
                let c = try await store.run(spec)
                onRun(c)
                dismiss()
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}

/// Pull an image onto the Mac this phone shows.
struct MobilePullSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var reference = ""
    @State private var phase: Phase = .idle
    @FocusState private var focused: Bool

    enum Phase {
        case idle
        case pulling(Date)
        case done(ImageInfo)
        case failed(String)
    }

    private var busy: Bool { if case .pulling = phase { true } else { false } }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("e.g. redis:7-alpine", text: $reference)
                        .font(G.mono(15))
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .focused($focused)
                        .onSubmit(pull)
                } header: {
                    Text("Reference")
                } footer: {
                    Text("From Docker Hub, GHCR or any OCI registry, onto \(store.liveName ?? "the Mac").")
                }
                switch phase {
                case .idle:
                    EmptyView()
                case let .pulling(since):
                    Section {
                        HStack(spacing: 10) {
                            ProgressView()
                            TimelineView(.periodic(from: since, by: 1)) { ctx in
                                Text("Pulling… \(Int(ctx.date.timeIntervalSince(since))) s")
                            }
                        }
                    }
                case let .done(image):
                    Section {
                        Label("Pulled \(image.repository):\(image.tag) · \(Format.bytes(image.sizeBytes))", systemImage: "checkmark.circle.fill")
                            .foregroundStyle(G.running)
                    }
                case let .failed(message):
                    Section { Label(message, systemImage: "xmark.octagon.fill").foregroundStyle(G.danger) }
                }
            }
            .scrollContentBackground(.hidden)
            .background(G.background)
            .navigationTitle("Pull image")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button(isDone ? "Done" : "Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Pull", action: pull).disabled(busy || reference.trimmingCharacters(in: .whitespaces).isEmpty || !store.canManage)
                }
            }
            .onAppear { focused = true }
        }
    }

    private var isDone: Bool { if case .done = phase { true } else { false } }

    private func pull() {
        let ref = reference.trimmingCharacters(in: .whitespaces)
        guard !busy, !ref.isEmpty, store.canManage else { return }
        phase = .pulling(.now)
        Task {
            do {
                phase = .done(try await store.pull(ref))
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}
