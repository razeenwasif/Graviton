import GravitonKit
import SwiftUI

@main
struct GravitonIOSApp: App {
    // iOS can't reach the Mac's Unix socket; remote hosts arrive in milestone 4.
    @State private var store = EngineStore(useDaemon: false)

    var body: some Scene {
        WindowGroup {
            TabView {
                Tab("Containers", systemImage: "shippingbox") { ContainerListView() }
                Tab("Images", systemImage: "square.stack.3d.up") { ImageListView() }
                Tab("Activity", systemImage: "waveform.path.ecg") {
                    Placeholder(title: "Activity", symbol: "waveform.path.ecg",
                                detail: "Events and alerts from your hosts will show up here.")
                }
                Tab("Hosts", systemImage: "desktopcomputer") {
                    Placeholder(title: "Hosts", symbol: "desktopcomputer",
                                detail: "Pair with gravitond on your Mac to control it from here. Until then the app shows demo data.")
                }
            }
            .environment(store)
            .preferredColorScheme(.dark)
            .tint(G.accent)
            .task { await store.run() }
        }
    }
}

struct Placeholder: View {
    var title: String
    var symbol: String
    var detail: String

    var body: some View {
        NavigationStack {
            ContentUnavailableView(title, systemImage: symbol, description: Text(detail))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(G.background)
                .navigationTitle(title)
        }
    }
}

struct ImageListView: View {
    @Environment(EngineStore.self) private var store

    var body: some View {
        NavigationStack {
            List(store.images) { image in
                HStack(spacing: 12) {
                    Text(image.monogram)
                        .font(G.mono(13, weight: .medium))
                        .foregroundStyle(G.accent)
                        .frame(width: 36, height: 36)
                        .background(G.control, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                    VStack(alignment: .leading, spacing: 1) {
                        Text(image.repository).font(.system(size: 15, weight: .medium)).lineLimit(1)
                        Text(image.tag).font(G.mono(11.5)).foregroundStyle(G.muted)
                    }
                    Spacer()
                    Text(Format.bytes(image.sizeBytes)).font(G.mono(12)).foregroundStyle(G.muted)
                }
                .listRowBackground(G.card)
            }
            .scrollContentBackground(.hidden)
            .background(G.background)
            .navigationTitle("Images")
        }
    }
}
