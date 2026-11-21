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
    @State private var memory: Double = 1024
    @State private var restartPolicy: RestartPolicy = .no
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

    private static let memoryChoices: [Double] = [512, 1024, 2048, 4096, 8192]

    var body: some View {
        MobileSheet("Run container", actionTitle: "Run", actionEnabled: !busy && !trimmedImage.isEmpty && store.canManage, action: run) {
            GFormRow("Image", hint: "Pulled first if \(store.liveName ?? "the Mac") doesn't have it.") {
                MobileField(placeholder: "e.g. postgres:17-alpine", text: $image)
            }
            if !store.images.isEmpty && trimmedImage.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        ForEach(store.images.prefix(8)) { img in
                            Button("\(img.repository):\(img.tag)") { image = "\(img.repository):\(img.tag)" }
                                .buttonStyle(.g(.secondary, size: .small))
                                .font(G.mono(12))
                        }
                    }
                }
                .scrollClipDisabled()
            }
            GFormRow("Name") { MobileField(placeholder: "Optional", text: $name, mono: false) }
            GFormRow("Command") { MobileField(placeholder: "Optional: replaces the image's", text: $command) }
            GFormRow("Ports", hint: "On 127.0.0.1 on the Mac") { MobileField(placeholder: "8080:80, 5432", text: $ports, keyboard: .numbersAndPunctuation) }
            GFormRow("Environment") { MobileField(placeholder: "KEY=value, KEY2=value", text: $env) }
            GFormRow("Memory") {
                GSegmented(selection: $memory, options: Self.memoryChoices.map { ($0, $0 < 1024 ? "\(Int($0)) MB" : "\(Int($0) / 1024) GB") }, stretch: true)
            }
            GFormRow("Restart") {
                GSegmented(selection: $restartPolicy, options: RestartPolicy.allCases.map { ($0, $0.label) }, stretch: true)
            }

            switch phase {
            case .idle:
                EmptyView()
            case .running:
                GStatusLine(.working, "Starting… (a pull can take a while)")
            case let .failed(message):
                GStatusLine(.failure, message)
            }
        }
        .disabled(busy)
        .interactiveDismissDisabled(busy)
    }

    private func run() {
        let words = RunSpec.words(command)
        let spec = RunSpec(image: trimmedImage, name: name.trimmingCharacters(in: .whitespaces).isEmpty ? nil : name.trimmingCharacters(in: .whitespaces),
                           command: words.isEmpty ? nil : words, env: RunSpec.list(env), memoryMib: UInt64(memory), ports: RunSpec.list(ports),
                           restart: restartPolicy == .no ? nil : restartPolicy)
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
        MobileSheet("Pull image", subtitle: "From Docker Hub, GHCR or any OCI registry, onto \(store.liveName ?? "the Mac").",
                    cancelTitle: isDone ? "Done" : "Cancel", actionTitle: "Pull",
                    actionEnabled: !busy && !reference.trimmingCharacters(in: .whitespaces).isEmpty && store.canManage, action: pull) {
            GFormRow("Reference") {
                MobileField(placeholder: "e.g. redis:7-alpine", text: $reference)
                    .focused($focused)
                    .submitLabel(.go)
                    .onSubmit(pull)
            }
            switch phase {
            case .idle:
                EmptyView()
            case let .pulling(since):
                TimelineView(.periodic(from: since, by: 1)) { ctx in
                    GStatusLine(.working, "Pulling… \(Int(ctx.date.timeIntervalSince(since))) s. You can close this; the pull carries on.")
                }
            case let .done(image):
                GStatusLine(.success, "Pulled \(image.repository):\(image.tag) · \(Format.bytes(image.sizeBytes))")
            case let .failed(message):
                GStatusLine(.failure, message)
            }
        }
        .presentationDetents([.medium, .large])
        .onAppear { focused = true }
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
