import GravitonKit
import SwiftUI

struct ImagesScreen: View {
    @Environment(EngineStore.self) private var store
    @State private var showPull = false

    private var totalSize: UInt64 { store.images.map(\.sizeBytes).reduce(0, +) }

    var body: some View {
        ScrollView {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 260), spacing: 14)], spacing: 14) {
                ForEach(store.images) { ImageCard(image: $0) }
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 22)
        }
        .background(G.background)
        .navigationTitle("Images")
        .navigationSubtitle("\(store.images.count) images · \(Format.bytes(totalSize)) on disk")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button("Build…") {}
                    .disabled(true)
                    .help("Image builds arrive with milestone 5")
                Button {
                    showPull = true
                } label: {
                    Label("Pull image", systemImage: "arrow.down.circle")
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .sheet(isPresented: $showPull) { PullImageSheet() }
    }
}

struct ImageCard: View {
    var image: ImageInfo

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
            Button("Copy Reference") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(image.reference, forType: .string)
            }
        }
    }
}

/// Pull sheet. Registry pulls arrive with milestone 2; until then the
/// progress is simulated so the flow can be reviewed.
struct PullImageSheet: View {
    @Environment(\.dismiss) private var dismiss
    @State private var reference = "nginx:1.27-alpine"
    @State private var layers: [Layer] = []
    @State private var pulling = false

    struct Layer: Identifiable {
        let id = UUID()
        var digest: String
        var sizeMB: Double
        var progress: Double
    }

    private var downloaded: Double { layers.map { $0.sizeMB * $0.progress }.reduce(0, +) }
    private var total: Double { layers.map(\.sizeMB).reduce(0, +) }
    private var done: Bool { !layers.isEmpty && layers.allSatisfy { $0.progress >= 1 } }

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
                    .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(G.accent.opacity(0.9)))
                    .disabled(pulling)
                    .onSubmit(startPull)
            }

            if !layers.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    HStack {
                        Text("Pulling \(layers.count) layers · linux/arm64")
                        Spacer()
                        Text(String(format: "%.1f / %.1f MB", downloaded, total)).font(G.mono(12))
                    }
                    .font(.system(size: 12))
                    .foregroundStyle(G.muted)
                    ForEach(layers) { layer in
                        HStack(spacing: 12) {
                            Text(layer.digest).foregroundStyle(G.muted).frame(width: 96, alignment: .leading)
                            MeterBar(layer.progress, color: layer.progress >= 1 ? G.running : G.accent)
                            Text(layer.progress >= 1 ? "Done" : "\(Int(layer.progress * 100))%")
                                .foregroundStyle(layer.progress >= 1 ? G.running : G.text)
                                .frame(width: 44, alignment: .trailing)
                        }
                        .font(G.mono(11.5))
                    }
                    Text("Simulated: registry pulls arrive with milestone 2.").font(.system(size: 11.5)).foregroundStyle(G.faint)
                }
            }

            HStack {
                Spacer()
                Button(done ? "Close" : "Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Pull", action: startPull)
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(pulling || reference.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(22)
        .frame(width: 520)
        .background(Color(hex: 0x15161A))
    }

    private func startPull() {
        guard !pulling else { return }
        pulling = true
        layers = [("sha256:6e7…", 3.6), ("sha256:a91…", 1.9), ("sha256:0c4…", 11.4), ("sha256:f2d…", 4.7)]
            .map { Layer(digest: $0.0, sizeMB: $0.1, progress: 0) }
        Task {
            while !done {
                try? await Task.sleep(for: .milliseconds(120))
                for i in layers.indices where layers[i].progress < 1 {
                    layers[i].progress = min(1, layers[i].progress + Double.random(in: 0.02...0.09) / Double(i + 1))
                }
            }
            pulling = false
        }
    }
}
