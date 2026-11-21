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
        GSheet("Clean up", subtitle: "Free disk space on \(store.liveName ?? "this Mac"). Layers a container or an image you keep still needs always stay.",
               symbol: "sparkles") {
            if let u = usage {
                VStack(spacing: 0) {
                    let snapCount = u.snapshots ?? 0
                    let cacheDetail = snapCount > 0
                        ? "\(u.buildCacheSteps) cached build steps · \(snapCount) VM snapshot\(snapCount == 1 ? "" : "s")"
                        : "\(u.buildCacheSteps) cached build steps"
                    row("Build cache, VM snapshots and unreferenced layers", detail: cacheDetail, bytes: u.alwaysReclaimable, toggle: nil)
                    Rectangle().fill(G.border).frame(height: 1)
                    row("Unused images", detail: "\(u.unusedImages) of \(u.images) images no container uses", bytes: u.unusedImagesBytes, toggle: $images)
                    Rectangle().fill(G.border).frame(height: 1)
                    row("Stopped containers", detail: "\(u.stoppedContainers) stopped, with the files on their disks", bytes: u.stoppedDisksBytes, toggle: $containers)
                }
                .background(G.background, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(G.border))

                usageBar(u)
            } else if case .failed = phase {
            } else {
                GSpinner(size: 20).frame(maxWidth: .infinity, minHeight: 120)
            }

            switch phase {
            case .idle, .working:
                if let p = preview {
                    GStatusLine(working ? .working : .info, working ? "Cleaning up…" : summary(p))
                }
            case let .done(r):
                GStatusLine(.success, "Freed \(Format.bytes(r.freedBytes))" + (r.images.isEmpty && r.containers.isEmpty ? "" : ": \(removed(r))"))
            case let .failed(message):
                GStatusLine(.failure, message)
            }
        } footer: {
            Spacer()
            Button("Close") { dismiss() }
                .buttonStyle(.gSecondary)
                .keyboardShortcut(.cancelAction)
            Button(preview.map { "Free \(Format.bytes($0.freedBytes))" } ?? "Clean Up", action: clean)
                .buttonStyle(.gPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(working || usage == nil || !store.canManage || (preview?.freedBytes ?? 1) == 0 && preview?.containers.isEmpty != false && preview?.images.isEmpty != false)
        }
        .task { await load() }
        .onChange(of: spec) { Task { await refreshPreview() } }
    }

    /// What's on disk, and how much of it this clean-up frees.
    private func usageBar(_ u: DiskUsage) -> some View {
        let used = u.blobsBytes + u.containerDisksBytes + (u.snapshotsBytes ?? 0)
        let total = Double(max(used, 1))
        let freeing = Double(preview?.freedBytes ?? 0)
        return VStack(alignment: .leading, spacing: 8) {
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.white.opacity(0.06))
                    Capsule().fill(G.accent.opacity(0.35)).frame(width: geo.size.width * Double(used) / total)
                    Capsule().fill(G.accent).frame(width: max(geo.size.width * min(freeing / total, 1), freeing > 0 ? 4 : 0))
                }
            }
            .frame(height: 6)
            .animation(.snappy(duration: 0.25), value: preview?.freedBytes)
            HStack(spacing: 14) {
                legend(G.accent, "Frees \(Format.bytes(preview?.freedBytes ?? 0))")
                legend(G.accent.opacity(0.35), "Images \(Format.bytes(u.blobsBytes)) · container disks \(Format.bytes(u.containerDisksBytes))")
            }
        }
    }

    private func legend(_ color: Color, _ text: String) -> some View {
        HStack(spacing: 6) {
            Circle().fill(color).frame(width: 7, height: 7)
            Text(text).font(.system(size: 11.5)).foregroundStyle(G.muted)
        }
    }

    private func row(_ title: String, detail: String, bytes: UInt64, toggle: Binding<Bool>?) -> some View {
        HStack(spacing: 12) {
            if let toggle {
                Toggle("", isOn: toggle).toggleStyle(.gCheckbox).labelsHidden().disabled(working)
            } else {
                Image(systemName: "lock.fill").font(.system(size: 9.5, weight: .bold)).foregroundStyle(G.faint).frame(width: 16).help("Always cleaned")
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
        var parts = ["\(p.buildCacheSteps) cached build steps"]
        if let s = p.snapshots, s > 0 { parts.append("\(s) VM snapshot\(s == 1 ? "" : "s")") }
        parts.append("\(p.blobs) layer files")
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
