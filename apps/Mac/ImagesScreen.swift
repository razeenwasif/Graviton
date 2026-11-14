import GravitonKit
import SwiftUI

struct ImagesScreen: View {
    @Environment(EngineStore.self) private var store
    @State private var showPull = false
    @State private var showBuild = false
    @State private var runImage: ImageInfo?

    private var totalSize: UInt64 { store.images.map(\.sizeBytes).reduce(0, +) }

    var body: some View {
        ScrollView {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 260), spacing: 14)], spacing: 14) {
                ForEach(store.images) { image in
                    ImageCard(image: image) { runImage = $0 }
                }
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 22)
        }
        .background(G.background)
        .navigationTitle("Images")
        .navigationSubtitle("\(store.images.count) images · \(Format.bytes(totalSize)) on disk")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button("Build…") { showBuild = true }
                    .help(store.canBuild ? "Build an image from a folder with a Dockerfile" : "Start gravitond (grav service install) to build images")
                Button {
                    showPull = true
                } label: {
                    Label("Pull image", systemImage: "arrow.down.circle")
                }
                .buttonStyle(.borderedProminent)
                .help(store.canManage ? "Pull an image from a registry" : "Start gravitond to pull images")
            }
        }
        .sheet(isPresented: $showPull) { PullImageSheet() }
        .sheet(isPresented: $showBuild) { BuildImageSheet() }
        .sheet(item: $runImage) { image in
            RunContainerSheet(initialImage: "\(image.repository):\(image.tag)")
        }
    }
}

struct ImageCard: View {
    var image: ImageInfo
    var onRun: (ImageInfo) -> Void = { _ in }

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
            }
            HStack(spacing: 14) {
                Text(Format.bytes(image.sizeBytes))
                Text(image.architecture == "amd64" ? "amd64 · Rosetta" : image.architecture)
                Spacer()
                Text(image.usedBy == 0 ? "Unused" : "\(image.usedBy) container\(image.usedBy == 1 ? "" : "s")")
            }
            .font(.system(size: 12))
            .foregroundStyle(G.muted)
        }
        .padding(16)
        .gravitonCard()
        .contextMenu {
            Button("Run…") { onRun(image) }
            Button("Copy Reference") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(image.reference, forType: .string)
            }
        }
    }
}

/// Pulls an image through gravitond. The daemon logs per-layer progress;
/// the sheet shows that a pull is running and how it ended.
struct PullImageSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var reference = ""
    @State private var phase: Phase = .idle
    @FocusState private var focused: Bool

    enum Phase: Equatable {
        case idle
        case pulling(since: Date)
        case done(ImageInfo)
        case failed(String)
    }

    private var pulling: Bool { if case .pulling = phase { true } else { false } }
    private var trimmed: String { reference.trimmingCharacters(in: .whitespaces) }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Pull image").font(.system(size: 16, weight: .semibold))
                Text("From Docker Hub, GHCR or any OCI registry.").font(.system(size: 12.5)).foregroundStyle(G.muted)
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Reference").font(.system(size: 12, weight: .medium)).foregroundStyle(G.textSecondary)
                TextField("e.g. postgres:17-alpine", text: $reference)
                    .textFieldStyle(.plain)
                    .font(G.mono(13))
                    .padding(.horizontal, 12)
                    .frame(height: 38)
                    .background(G.background, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(G.accent.opacity(focused ? 0.9 : 0.35)))
                    .focused($focused)
                    .disabled(pulling)
                    .onSubmit(startPull)
            }

            Group {
                switch phase {
                case .idle:
                    if !store.canManage {
                        Label("Start gravitond to pull images: scripts/dev-gravitond.sh", systemImage: "exclamationmark.triangle")
                            .foregroundStyle(G.warning)
                    }
                case let .pulling(since):
                    HStack(spacing: 10) {
                        ProgressView().controlSize(.small)
                        TimelineView(.periodic(from: since, by: 1)) { context in
                            Text("Pulling \(trimmed)… \(Int(context.date.timeIntervalSince(since))) s")
                        }
                    }
                    .foregroundStyle(G.textSecondary)
                case let .done(image):
                    Label("Pulled \(image.repository):\(image.tag) · \(Format.bytes(image.sizeBytes)) · \(image.architecture)", systemImage: "checkmark.circle.fill")
                        .foregroundStyle(G.running)
                case let .failed(message):
                    Label(message, systemImage: "xmark.octagon.fill")
                        .foregroundStyle(G.danger)
                        .textSelection(.enabled)
                }
            }
            .font(.system(size: 12.5))
            .fixedSize(horizontal: false, vertical: true)

            HStack {
                if pulling {
                    Text("You can close this; the pull carries on.").font(.system(size: 11.5)).foregroundStyle(G.faint)
                }
                Spacer()
                Button("Close") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Pull", action: startPull)
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(pulling || trimmed.isEmpty || !store.canManage)
            }
        }
        .padding(22)
        .frame(width: 520)
        .background(Color(hex: 0x15161A))
        .onAppear { focused = true }
    }

    private func startPull() {
        guard !pulling, !trimmed.isEmpty, store.canManage else { return }
        let reference = trimmed
        phase = .pulling(since: .now)
        Task {
            do {
                phase = .done(try await store.pull(reference))
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}
