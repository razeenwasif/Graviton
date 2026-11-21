import GravitonKit
import SwiftUI

struct ImagesScreen: View {
    @Environment(EngineStore.self) private var store
    @Environment(Dialogs.self) private var dialogs
    @State private var showPull = false
    @State private var showBuild = false
    @State private var showCleanUp = false
    @State private var runImage: ImageInfo?
    @State private var search = ""

    private var totalSize: UInt64 { store.images.map(\.sizeBytes).reduce(0, +) }
    private var shown: [ImageInfo] {
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        return q.isEmpty ? store.images : store.images.filter { "\($0.repository):\($0.tag)".lowercased().contains(q) }
    }

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader("Images", subtitle: "\(store.images.count) images · \(Format.bytes(totalSize)) on disk") {
                HeaderSearch(prompt: "Search images", text: $search)
                Button {
                    showCleanUp = true
                } label: {
                    Label("Clean Up", systemImage: "sparkles")
                }
                .buttonStyle(.gSecondary)
                .help("Free disk space: the build cache, unused layers, images and stopped containers")
                .disabled(!store.canManage)
                Button {
                    showBuild = true
                } label: {
                    Label("Build", systemImage: "hammer")
                }
                .buttonStyle(.gSecondary)
                .keyboardShortcut("b")
                .help(store.canBuild ? "Build an image from a folder with a Dockerfile (⌘B)" : "Start gravitond (grav service install) to build images")
                Button {
                    showPull = true
                } label: {
                    Label("Pull image", systemImage: "arrow.down")
                }
                .buttonStyle(.gPrimary)
                .keyboardShortcut("n")
                .help(store.canManage ? "Pull an image from a registry (⌘N)" : "Start gravitond to pull images")
            }

            if store.images.isEmpty {
                GEmptyState("No images yet", symbol: "square.stack.3d.up",
                            detail: "Pull one from Docker Hub, GHCR or any OCI registry, or build one from a Dockerfile.",
                            actionTitle: store.canManage ? "Pull an image" : nil) { showPull = true }
            } else if shown.isEmpty {
                GEmptyState("Nothing matches", symbol: "magnifyingglass", detail: "No image's name contains “\(search)”.")
            } else {
                ScrollView {
                    LazyVGrid(columns: [GridItem(.adaptive(minimum: 260), spacing: 14)], spacing: 14) {
                        ForEach(shown) { image in
                            ImageCard(image: image, onRun: { runImage = $0 }, onRemove: confirmRemove)
                        }
                    }
                    .padding(.horizontal, 28)
                    .padding(.top, 6)
                    .padding(.bottom, 22)
                }
            }
        }
        .background(G.background)
        .sheet(isPresented: $showPull) { PullImageSheet() }
        .sheet(isPresented: $showBuild) { BuildImageSheet() }
        .sheet(isPresented: $showCleanUp) { CleanUpSheet() }
        .sheet(item: $runImage) { image in
            RunContainerSheet(initialImage: "\(image.repository):\(image.tag)")
        }
    }

    private func confirmRemove(_ image: ImageInfo) {
        dialogs.confirm("Remove \(image.repository):\(image.tag)?",
                        message: "Its layers go too, unless another image or the build cache uses them. You can pull it again later.",
                        confirm: "Remove") {
            Task {
                do { try await store.removeImage(image) } catch {
                    dialogs.alert("Couldn't remove the image", message: error.localizedDescription)
                }
            }
        }
    }
}

struct ImageCard: View {
    var image: ImageInfo
    var onRun: (ImageInfo) -> Void = { _ in }
    var onRemove: (ImageInfo) -> Void = { _ in }
    @State private var hovering = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 12) {
                Text(image.monogram)
                    .font(G.mono(13, weight: .medium))
                    .foregroundStyle(G.accent)
                    .frame(width: 36, height: 36)
                    .background(G.control, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                VStack(alignment: .leading, spacing: 1) {
                    Text(image.repository).fontWeight(.medium).lineLimit(1).truncationMode(.middle)
                    Text(image.tag).font(G.mono(12)).foregroundStyle(G.muted)
                }
                Spacer(minLength: 0)
                if hovering {
                    HStack(spacing: 2) {
                        Button { onRun(image) } label: { Image(systemName: "play.fill") }
                            .help("Run a container from it")
                        Button { onRemove(image) } label: { Image(systemName: "trash") }
                            .disabled(image.usedBy > 0)
                            .help(image.usedBy > 0 ? "A container uses it: remove the container first" : "Remove this image")
                    }
                    .buttonStyle(.gIcon)
                    .transition(.opacity)
                }
            }
            HStack(spacing: 14) {
                Text(Format.bytes(image.sizeBytes))
                Text(image.architecture == "amd64" ? "amd64 · Rosetta" : image.architecture)
                Spacer()
                if image.usedBy == 0 {
                    Text("Unused")
                } else {
                    GBadge("\(image.usedBy) container\(image.usedBy == 1 ? "" : "s")", color: G.running)
                }
            }
            .font(.system(size: 12))
            .foregroundStyle(G.muted)
        }
        .padding(16)
        .gravitonCard(hovering ? G.card : G.surface)
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(hovering ? Color.white.opacity(0.09) : .clear))
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hovering)
        .contextMenu {
            Button("Run…") { onRun(image) }
            Button("Copy Reference") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(image.reference, forType: .string)
            }
            Divider()
            Button("Remove…", role: .destructive) { onRemove(image) }
                .disabled(image.usedBy > 0)
                .help(image.usedBy > 0 ? "A container uses it: remove the container first" : "Remove this image")
        }
    }
}

/// Pulls an image through gravitond with streamed per-layer progress.
struct PullImageSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var reference = ""
    @State private var platform = ""
    @State private var phase: Phase = .idle
    @State private var progress = PullState()
    @FocusState private var focused: Bool

    private static let platformChoices: [(String, String)] = [
        ("", "Auto"),
        ("linux/arm64", "arm64"),
        ("linux/amd64", "amd64 (Rosetta)"),
    ]

    enum Phase: Equatable {
        case idle
        case pulling(since: Date)
        case done(ImageInfo)
        case failed(String)
    }

    private var pulling: Bool { if case .pulling = phase { true } else { false } }
    private var trimmed: String { reference.trimmingCharacters(in: .whitespaces) }

    var body: some View {
        GSheet("Pull image", subtitle: "From Docker Hub, GHCR or any OCI registry.", symbol: "arrow.down.circle") {
            GFormRow("Reference", hint: "A name and tag; Docker Hub is the default registry.") {
                GTextField("e.g. postgres:17-alpine", text: $reference)
                    .focused($focused)
                    .disabled(pulling)
                    .onSubmit(startPull)
                    .gravitonField(mono: true, height: 36)
            }
            if store.engine?.rosetta == true {
                GFormRow("Platform", hint: "Auto prefers arm64 and falls back to amd64 via Rosetta.") {
                    GSegmented(selection: $platform, options: Self.platformChoices)
                        .disabled(pulling)
                }
            }

            switch phase {
            case .idle:
                if !store.canManage {
                    GStatusLine(.warning, "Start gravitond to pull images: scripts/dev-gravitond.sh")
                }
            case let .pulling(since):
                GPullProgressView(reference: trimmed, since: since, state: progress)
            case let .done(image):
                GStatusLine(.success, "Pulled \(image.repository):\(image.tag) · \(Format.bytes(image.sizeBytes)) · \(image.architecture)")
            case let .failed(message):
                GStatusLine(.failure, message)
            }
        } footer: {
            if pulling {
                Text("You can close this; the pull carries on.").font(.system(size: 11.5)).foregroundStyle(G.faint)
            }
            Spacer()
            Button("Close") { dismiss() }
                .buttonStyle(.gSecondary)
                .keyboardShortcut(.cancelAction)
            Button("Pull", action: startPull)
                .buttonStyle(.gPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(pulling || trimmed.isEmpty || !store.canManage)
        }
        .onAppear { focused = true }
    }

    private func startPull() {
        guard !pulling, !trimmed.isEmpty, store.canManage else { return }
        let reference = trimmed
        let selectedPlatform = platform.isEmpty ? nil : platform
        progress = PullState()
        phase = .pulling(since: .now)
        Task {
            do {
                var finished: ImageInfo?
                for try await event in store.pullStream(reference, platform: selectedPlatform) {
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
