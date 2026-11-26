import AppKit
import GravitonKit
import SwiftUI
import UniformTypeIdentifiers

struct ImagesScreen: View {
    @Environment(EngineStore.self) private var store
    @Environment(Dialogs.self) private var dialogs
    @State private var showPull = false
    @State private var showBuild = false
    @State private var showCleanUp = false
    @State private var runImage: ImageInfo?
    @State private var search = ""
    @State private var filter: ImageFilter = .all
    @State private var transferring: String?
    @State private var transferBanner: (tone: GStatusLine.Tone, text: String)?

    enum ImageFilter: String, CaseIterable, Identifiable {
        case all = "All", inUse = "In Use", unused = "Unused"
        var id: String { rawValue }
    }

    private var totalSize: UInt64 { store.images.map(\.sizeBytes).reduce(0, +) }
    private var inUseCount: Int { store.images.filter { $0.usedBy > 0 }.count }
    private var unusedCount: Int { store.images.count - inUseCount }

    private var shown: [ImageInfo] {
        let base: [ImageInfo] = switch filter {
        case .all: store.images
        case .inUse: store.images.filter { $0.usedBy > 0 }
        case .unused: store.images.filter { $0.usedBy == 0 }
        }
        let q = search.trimmingCharacters(in: .whitespaces).lowercased()
        return q.isEmpty ? base : base.filter { "\($0.repository):\($0.tag)".lowercased().contains(q) }
    }

    private var diskSegments: [GBarSegment] {
        let palette: [Color] = [G.indigo, G.sky, G.mint, G.violet, G.amber]
        let sorted = store.images.sorted { $0.sizeBytes > $1.sizeBytes }
        let top = Array(sorted.prefix(4))
        let restBytes = sorted.dropFirst(4).reduce(UInt64(0)) { $0 + $1.sizeBytes }
        var segs = top.enumerated().map { idx, img in
            GBarSegment(
                label: "\(img.repository):\(img.tag)",
                value: Double(img.sizeBytes),
                detail: Format.bytes(img.sizeBytes),
                color: palette[idx % palette.count]
            )
        }
        if restBytes > 0 {
            segs.append(GBarSegment(
                label: "Other (\(sorted.count - 4))",
                value: Double(restBytes),
                detail: Format.bytes(restBytes),
                color: G.slate
            ))
        }
        return segs
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
                    pickAndImport()
                } label: {
                    Label("Import…", systemImage: "square.and.arrow.down")
                }
                .buttonStyle(.gSecondary)
                .help(store.canTransferFiles ? "Import image(s) from a .tar or .tar.gz archive" : "Start gravitond to import images")
                .disabled(!store.canTransferFiles || transferring != nil)
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

            if let transferring {
                GStatusLine(.working, transferring)
                    .padding(.horizontal, 28)
                    .padding(.top, 6)
                    .padding(.bottom, 8)
            } else if let banner = transferBanner {
                GStatusLine(banner.tone, banner.text)
                    .padding(.horizontal, 28)
                    .padding(.top, 6)
                    .padding(.bottom, 8)
            }

            VStack(alignment: .leading, spacing: 18) {
                kpiStrip

                HStack(spacing: 8) {
                    ForEach(ImageFilter.allCases) { f in
                        let cnt: Int = switch f {
                        case .all: store.images.count
                        case .inUse: inUseCount
                        case .unused: unusedCount
                        }
                        GFilterPillButton(f.rawValue, count: cnt, selected: filter == f, accent: G.sky) {
                            withAnimation(.snappy(duration: 0.18)) { filter = f }
                        }
                    }
                    Spacer()
                }

                if store.images.isEmpty {
                    GEmptyState("No images yet", symbol: "square.stack.3d.up",
                                detail: "Pull one from Docker Hub, GHCR or any OCI registry, or build one from a Dockerfile.",
                                actionTitle: store.canManage ? "Pull an image" : nil) { showPull = true }
                        .gravitonCard(G.surfaceSubtle)
                } else if shown.isEmpty {
                    GEmptyState("Nothing matches", symbol: "magnifyingglass",
                                detail: search.isEmpty ? "No \(filter.rawValue.lowercased()) images." : "No image's name contains “\(search)”.")
                        .gravitonCard(G.surfaceSubtle)
                } else {
                    ScrollView {
                        LazyVGrid(columns: [GridItem(.adaptive(minimum: 270), spacing: 14)], spacing: 14) {
                            ForEach(shown) { image in
                                ImageCard(
                                    image: image,
                                    canExport: store.canTransferFiles && transferring == nil,
                                    onRun: { runImage = $0 },
                                    onExport: pickAndExport,
                                    onRemove: confirmRemove
                                )
                            }
                        }
                        .padding(.bottom, 22)
                    }
                }
            }
            .padding(.horizontal, 28)
            .padding(.top, 4)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        }
        .background(G.bg)
        .sheet(isPresented: $showPull) { PullImageSheet() }
        .sheet(isPresented: $showBuild) { BuildImageSheet() }
        .sheet(isPresented: $showCleanUp) { CleanUpSheet() }
        .sheet(item: $runImage) { image in
            RunContainerSheet(initialImage: "\(image.repository):\(image.tag)")
        }
    }

    private var kpiStrip: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 36) {
                GMetricStatColumn(
                    label: "Total Images",
                    value: "\(store.images.count)",
                    subtitle: "\(inUseCount) attached · \(unusedCount) unused",
                    valueColor: G.sky
                )
                GMetricStatColumn(
                    label: "Disk Usage",
                    value: Format.bytes(totalSize),
                    subtitle: "OCI rootfs & layers",
                    valueColor: G.indigo
                )
                GMetricStatColumn(
                    label: "In Use",
                    value: "\(inUseCount)",
                    subtitle: "active references",
                    valueColor: inUseCount > 0 ? G.mint : G.textPrimary
                )
                Spacer()
            }
            if !diskSegments.isEmpty && totalSize > 0 {
                GSegmentedBar(segments: diskSegments, height: 6)
            }
        }
        .padding(.vertical, 14)
        .padding(.horizontal, 18)
        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }

    private func pickAndImport() {
        guard store.canTransferFiles, transferring == nil else { return }
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        var types: [UTType] = [.gzip]
        if let tar = UTType(filenameExtension: "tar") { types.append(tar) }
        if let tgz = UTType(filenameExtension: "tgz") { types.append(tgz) }
        panel.allowedContentTypes = types
        panel.prompt = "Import"
        panel.message = "Choose a Docker/OCI .tar or .tar.gz image archive to load into \(store.liveName ?? "this Mac")"
        panel.begin { response in
            guard response == .OK, let source = panel.url else { return }
            Task { await importArchive(source) }
        }
    }

    private func importArchive(_ url: URL) async {
        transferBanner = nil
        transferring = "Importing \(url.lastPathComponent)…"
        defer { transferring = nil }
        do {
            let loaded = try await store.loadImage(from: url)
            let summary = loaded.map { "\($0.repository):\($0.tag)" }.joined(separator: ", ")
            let label = loaded.isEmpty ? "Loaded archive \(url.lastPathComponent)." : "Imported \(summary)."
            transferBanner = (.success, label)
        } catch {
            dialogs.alert("Couldn't import image archive", message: error.localizedDescription)
        }
    }

    private func pickAndExport(_ image: ImageInfo) {
        guard store.canTransferFiles, transferring == nil else { return }
        let panel = NSSavePanel()
        let safeRepo = image.repository.replacingOccurrences(of: "/", with: "-")
        let safeTag = image.tag.replacingOccurrences(of: "/", with: "-")
        panel.nameFieldStringValue = "\(safeRepo)-\(safeTag).tar"
        panel.canCreateDirectories = true
        panel.prompt = "Export"
        panel.message = "Export \(image.repository):\(image.tag) as a .tar image archive"
        panel.begin { response in
            guard response == .OK, let destination = panel.url else { return }
            Task { await exportArchive(image, to: destination) }
        }
    }

    private func exportArchive(_ image: ImageInfo, to destination: URL) async {
        let ref = "\(image.repository):\(image.tag)"
        transferBanner = nil
        transferring = "Exporting \(ref)…"
        defer { transferring = nil }
        do {
            try await store.saveImage(references: [ref], to: destination)
            transferBanner = (.success, "Exported \(ref) → \(destination.lastPathComponent).")
        } catch {
            dialogs.alert("Couldn't export \(ref)", message: error.localizedDescription)
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
    var canExport = false
    var onRun: (ImageInfo) -> Void = { _ in }
    var onExport: (ImageInfo) -> Void = { _ in }
    var onRemove: (ImageInfo) -> Void = { _ in }
    @State private var hovering = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 12) {
                Text(image.monogram)
                    .font(G.mono(13, weight: .semibold))
                    .foregroundStyle(G.sky)
                    .frame(width: 36, height: 36)
                    .background(G.sky.opacity(0.12), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                VStack(alignment: .leading, spacing: 2) {
                    Text(image.repository)
                        .font(.system(size: 13.5, weight: .semibold))
                        .foregroundStyle(G.textPrimary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Text(image.tag)
                        .font(G.mono(11.5))
                        .foregroundStyle(G.textMuted)
                }
                Spacer(minLength: 0)
                if hovering {
                    HStack(spacing: 3) {
                        Button { onRun(image) } label: { Image(systemName: "play.fill") }
                            .help("Run a container from it")
                        Button { onExport(image) } label: { Image(systemName: "square.and.arrow.up") }
                            .disabled(!canExport)
                            .help(canExport ? "Export this image as a .tar archive" : "Start gravitond to export images")
                        Button { onRemove(image) } label: { Image(systemName: "trash") }
                            .disabled(image.usedBy > 0)
                            .help(image.usedBy > 0 ? "A container uses it: remove the container first" : "Remove this image")
                    }
                    .buttonStyle(.gIcon)
                    .transition(.opacity)
                }
            }
            HStack(spacing: 10) {
                Text(Format.bytes(image.sizeBytes))
                    .font(G.mono(12, weight: .medium))
                    .foregroundStyle(G.textSecondary)
                GTagPill(
                    image.architecture == "amd64" ? "amd64 · Rosetta" : image.architecture.replacingOccurrences(of: ",", with: " · "),
                    color: image.architecture == "amd64" ? G.amber : G.textSecondary
                )
                Spacer()
                if image.usedBy == 0 {
                    Text("Unused")
                        .font(.system(size: 11.5))
                        .foregroundStyle(G.textTertiary)
                } else {
                    GTagPill("\(image.usedBy) in use", color: G.mint)
                }
            }
        }
        .padding(16)
        .background(hovering ? G.surfaceHover : G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hovering)
        .contextMenu {
            Button("Run…") { onRun(image) }
            Button("Export (.tar)…") { onExport(image) }
                .disabled(!canExport)
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
