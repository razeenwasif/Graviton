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
    @State private var showSecurity = false
    @State private var readOnlyRootfs = false
    @State private var useInit = false
    @State private var capDrop = ""
    @State private var capAdd = ""
    @State private var tmpfsText = ""
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
    private var badTmpfs: String? { TmpfsMount.invalidLine(in: tmpfsText) }
    private var capDropList: [String] { RunSpec.list(capDrop) }
    private var capAddList: [String] { RunSpec.list(capAdd) }
    private var tmpfsMounts: [TmpfsMount] { TmpfsMount.parseLines(tmpfsText) }
    private var securityActiveCount: Int {
        (readOnlyRootfs ? 1 : 0) + (useInit ? 1 : 0) + capDropList.count + capAddList.count + tmpfsMounts.count
    }

    private static let memoryChoices: [Double] = [512, 1024, 2048, 4096, 8192]

    var body: some View {
        MobileSheet("Run container", actionTitle: "Run", actionEnabled: !busy && !trimmedImage.isEmpty && badTmpfs == nil && store.canManage, action: run) {
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
            GFormRow("Ports", hint: "Default 127.0.0.1 on the Mac; add IP: or /udp") { MobileField(placeholder: "8080:80, 0.0.0.0:8080:80, 5353/udp", text: $ports, keyboard: .numbersAndPunctuation) }
            GFormRow("Environment") { MobileField(placeholder: "KEY=value, KEY2=value", text: $env) }
            GFormRow("Memory") {
                GSegmented(selection: $memory, options: Self.memoryChoices.map { ($0, $0 < 1024 ? "\(Int($0)) MB" : "\(Int($0) / 1024) GB") }, stretch: true)
            }
            GFormRow("Restart") {
                GSegmented(selection: $restartPolicy, options: RestartPolicy.allCases.map { ($0, $0.label) }, stretch: true)
            }

            VStack(alignment: .leading, spacing: 12) {
                Button {
                    withAnimation(.snappy(duration: 0.18)) { showSecurity.toggle() }
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: showSecurity ? "chevron.down" : "chevron.right")
                            .font(.system(size: 11, weight: .bold))
                            .foregroundStyle(G.muted)
                            .frame(width: 14)
                        Text("Security & Runtime")
                            .font(.system(size: 14, weight: .medium))
                            .foregroundStyle(G.textSecondary)
                        if securityActiveCount > 0 {
                            GBadge("\(securityActiveCount)", color: G.accent)
                        } else if badTmpfs != nil {
                            GBadge("!", color: G.danger)
                        }
                        Spacer()
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                if showSecurity {
                    VStack(alignment: .leading, spacing: 14) {
                        Toggle("Read-only root filesystem", isOn: $readOnlyRootfs)
                            .toggleStyle(.gSwitch)
                        Toggle("Init process reaper (docker-init)", isOn: $useInit)
                            .toggleStyle(.gSwitch)
                        GFormRow("Drop capabilities", hint: "Comma-separated, e.g. ALL, NET_RAW") {
                            MobileField(placeholder: "ALL", text: $capDrop)
                        }
                        GFormRow("Add capabilities", hint: "Comma-separated, e.g. NET_BIND_SERVICE") {
                            MobileField(placeholder: "NET_BIND_SERVICE", text: $capAdd)
                        }
                        GFormRow("Tmpfs mounts",
                                 hint: "One /path[:options] per line, e.g. /tmp:rw,noexec,size=64m",
                                 error: badTmpfs.map { "“\($0)” must start with /" }) {
                            TextEditor(text: $tmpfsText)
                                .scrollContentBackground(.hidden)
                                .textInputAutocapitalization(.never)
                                .autocorrectionDisabled()
                                .frame(height: 68)
                                .gravitonField(mono: true, height: nil, invalid: badTmpfs != nil, size: 15)
                        }
                    }
                    .padding(14)
                    .mobileCard(radius: 12)
                }
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
        let spec = RunSpec(
            image: trimmedImage,
            name: name.trimmingCharacters(in: .whitespaces).isEmpty ? nil : name.trimmingCharacters(in: .whitespaces),
            command: words.isEmpty ? nil : words,
            env: RunSpec.list(env),
            memoryMib: UInt64(memory),
            ports: RunSpec.list(ports),
            restart: restartPolicy == .no ? nil : restartPolicy,
            readOnly: readOnlyRootfs ? true : nil,
            capAdd: capAddList.isEmpty ? nil : capAddList,
            capDrop: capDropList.isEmpty ? nil : capDropList,
            tmpfs: tmpfsMounts.isEmpty ? nil : tmpfsMounts,
            initProcess: useInit ? true : nil
        )
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

/// Pull an image onto the Mac this phone shows, with streamed per-layer progress.
struct MobilePullSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var reference = ""
    @State private var phase: Phase = .idle
    @State private var progress = PullState()
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
                    .disabled(busy)
                    .submitLabel(.go)
                    .onSubmit(pull)
            }
            switch phase {
            case .idle:
                EmptyView()
            case let .pulling(since):
                VStack(alignment: .leading, spacing: 8) {
                    GPullProgressView(reference: reference.trimmingCharacters(in: .whitespaces), since: since, state: progress)
                    Text("You can close this; the pull carries on.")
                        .font(.system(size: 12))
                        .foregroundStyle(G.faint)
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
        progress = PullState()
        phase = .pulling(.now)
        Task {
            do {
                var finished: ImageInfo?
                for try await event in store.pullStream(ref) {
                    switch event {
                    case let .progress(update):
                        progress.apply(update)
                    case let .pulled(image):
                        finished = image
                    }
                }
                if let image = finished {
                    phase = .done(image)
                } else {
                    phase = .failed("Pull finished without an image response.")
                }
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}

/// Snapshot a container's filesystem and configuration into a new image on the paired Mac (`grav commit`).
struct MobileCommitSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    var container: Container
    @State private var reference = ""
    @State private var author = ""
    @State private var message = ""
    @State private var changesText = ""
    @State private var phase: Phase = .idle
    @FocusState private var focused: Bool

    enum Phase {
        case idle
        case committing
        case done(ImageInfo)
        case failed(String)
    }

    private var busy: Bool { if case .committing = phase { true } else { false } }
    private var isDone: Bool { if case .done = phase { true } else { false } }

    private var parsedChanges: [String] {
        changesText
            .split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
    }

    var body: some View {
        MobileSheet("Commit to image",
                    subtitle: "Snapshot \(container.name) into a local OCI image on \(store.liveName ?? "the Mac").",
                    cancelTitle: isDone ? "Done" : "Cancel",
                    actionTitle: "Commit",
                    actionEnabled: !busy && store.canManage,
                    action: commit) {
            GFormRow("Reference", hint: "repo[:tag]. Leave blank for committed:\(String(container.id.prefix(12))).") {
                MobileField(placeholder: "e.g. \(container.name):snapshot", text: $reference)
                    .focused($focused)
                    .disabled(busy)
                    .submitLabel(.go)
                    .onSubmit(commit)
            }
            GFormRow("Author") {
                MobileField(placeholder: "Optional name or email", text: $author, mono: false)
                    .disabled(busy)
            }
            GFormRow("Message") {
                MobileField(placeholder: "Optional commit message", text: $message, mono: false)
                    .disabled(busy)
            }
            GFormRow("Config changes", hint: "Optional Dockerfile lines (one per line), e.g. ENV MODE=prod") {
                TextEditor(text: $changesText)
                    .scrollContentBackground(.hidden)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .disabled(busy)
                    .frame(height: 82)
                    .gravitonField(mono: true, height: nil, size: 14.5)
            }

            switch phase {
            case .idle:
                EmptyView()
            case .committing:
                GStatusLine(.working, "Snapshotting \(container.name)…")
            case let .done(image):
                GStatusLine(.success, "Committed \(image.repository):\(image.tag) · \(Format.bytes(image.sizeBytes))")
            case let .failed(reason):
                GStatusLine(.failure, reason)
            }
        }
        .onAppear { focused = true }
    }

    private func commit() {
        guard !busy, store.canManage else { return }
        let trimmedRef = reference.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedAuthor = author.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedMsg = message.trimmingCharacters(in: .whitespacesAndNewlines)
        let spec = CommitSpec(
            container: container.id,
            reference: trimmedRef,
            author: trimmedAuthor.isEmpty ? nil : trimmedAuthor,
            message: trimmedMsg.isEmpty ? nil : trimmedMsg,
            changes: parsedChanges
        )
        phase = .committing
        Task {
            do {
                let image = try await store.commit(spec)
                phase = .done(image)
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}
