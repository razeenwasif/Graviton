import GravitonKit
import SwiftUI

/// Free disk space: the build cache and unreferenced layers always, unused
/// images and stopped containers if you tick them. Shows what each frees
/// (a dry run) before anything goes.
struct CleanUpSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var usage: DiskUsage?
    @State private var images = false
    @State private var containers = false
    @State private var preview: PruneReport?
    @State private var phase: Phase = .idle

    enum Phase: Equatable {
        case idle, working
        case done(PruneReport)
        case failed(String)
    }

    private var working: Bool { phase == .working }
    private var spec: PruneSpec { PruneSpec(images: images, containers: containers) }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Clean up").font(.system(size: 16, weight: .semibold))
                Text("Free disk space on \(store.liveName ?? "this Mac"). Layers a container or an image you keep still needs always stay.")
                    .font(.system(size: 12.5)).foregroundStyle(G.muted)
            }

            if let u = usage {
                VStack(spacing: 0) {
                    row("Build cache and unreferenced layers", detail: "\(u.buildCacheSteps) cached build steps", bytes: u.alwaysReclaimable, toggle: nil)
                    Divider().overlay(G.border)
                    row("Unused images", detail: "\(u.unusedImages) of \(u.images) images no container uses", bytes: u.unusedImagesBytes, toggle: $images)
                    Divider().overlay(G.border)
                    row("Stopped containers", detail: "\(u.stoppedContainers) stopped, with the files on their disks", bytes: u.stoppedDisksBytes, toggle: $containers)
                }
                .background(G.background, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(G.border))

                Text("Images take \(Format.bytes(u.blobsBytes)); container disks \(Format.bytes(u.containerDisksBytes)).")
                    .font(.system(size: 12)).foregroundStyle(G.faint)
            } else if case .failed = phase {
            } else {
                ProgressView().controlSize(.small).frame(maxWidth: .infinity, minHeight: 120)
            }

            Group {
                switch phase {
                case .idle, .working:
                    if let p = preview {
                        Text(summary(p)).foregroundStyle(G.textSecondary)
                    }
                case let .done(r):
                    Label("Freed \(Format.bytes(r.freedBytes))" + (r.images.isEmpty && r.containers.isEmpty ? "" : ": \(removed(r))"), systemImage: "checkmark.circle.fill")
                        .foregroundStyle(G.running)
                case let .failed(message):
                    Label(message, systemImage: "xmark.octagon.fill").foregroundStyle(G.danger).textSelection(.enabled)
                }
            }
            .font(.system(size: 12.5))
            .fixedSize(horizontal: false, vertical: true)

            HStack {
                Spacer()
                Button("Close") { dismiss() }.keyboardShortcut(.cancelAction)
                Button(preview.map { "Free \(Format.bytes($0.freedBytes))" } ?? "Clean Up", action: clean)
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(working || usage == nil || !store.canManage || (preview?.freedBytes ?? 1) == 0 && preview?.containers.isEmpty != false && preview?.images.isEmpty != false)
            }
        }
        .padding(22)
        .frame(width: 520)
        .background(Color(hex: 0x15161A))
        .task { await load() }
        .onChange(of: spec) { Task { await refreshPreview() } }
    }

    private func row(_ title: String, detail: String, bytes: UInt64, toggle: Binding<Bool>?) -> some View {
        HStack(spacing: 12) {
            if let toggle {
                Toggle("", isOn: toggle).toggleStyle(.checkbox).labelsHidden().disabled(working)
            } else {
                Image(systemName: "checkmark").font(.system(size: 11, weight: .bold)).foregroundStyle(G.faint).frame(width: 14)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.system(size: 13, weight: .medium))
                Text(detail).font(.system(size: 11.5)).foregroundStyle(G.faint)
            }
            Spacer()
            Text(Format.bytes(bytes)).font(G.mono(12.5)).foregroundStyle(G.textSecondary)
        }
        .padding(.horizontal, 14)
        .frame(minHeight: 52)
    }

    private func summary(_ p: PruneReport) -> String {
        var parts = ["\(p.buildCacheSteps) cached build steps", "\(p.blobs) layer files"]
        if !p.images.isEmpty { parts.append("\(p.images.count) image\(p.images.count == 1 ? "" : "s")") }
        if !p.containers.isEmpty { parts.append("\(p.containers.count) container\(p.containers.count == 1 ? "" : "s")") }
        return "Removes " + ListFormatter.localizedString(byJoining: parts) + "."
    }

    private func removed(_ r: PruneReport) -> String {
        var parts: [String] = []
        if !r.images.isEmpty { parts.append(r.images.count == 1 ? "1 image" : "\(r.images.count) images") }
        if !r.containers.isEmpty { parts.append(r.containers.count == 1 ? "1 container" : "\(r.containers.count) containers") }
        return ListFormatter.localizedString(byJoining: parts) + " removed"
    }

    private func load() async {
        do {
            usage = try await store.diskUsage()
            await refreshPreview()
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    private func refreshPreview() async {
        var dry = spec
        dry.dryRun = true
        preview = try? await store.prune(dry)
    }

    private func clean() {
        phase = .working
        Task {
            do {
                let report = try await store.prune(spec)
                phase = .done(report)
                usage = try? await store.diskUsage()
                preview = nil
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}
