import GravitonKit
import SwiftUI

struct MobileLogsView: View {
    @Environment(EngineStore.self) private var store
    var containerID: String
    @State private var query = ""
    @State private var level: LogLevel?
    @State private var follow = true

    private var lines: [LogLine] {
        (store.logs[containerID] ?? []).filter {
            (level == nil || $0.level == level) && (query.isEmpty || $0.message.localizedCaseInsensitiveContains(query))
        }
    }

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    HStack(spacing: 8) {
                        LevelChip(title: "All", tint: G.text, selected: level == nil) { level = nil }
                        LevelChip(title: "Info", tint: G.textSecondary, selected: level == .info) { level = .info }
                        LevelChip(title: "Warn", tint: G.warning, selected: level == .warn) { level = .warn }
                        LevelChip(title: "Error", tint: G.danger, selected: level == .error) { level = .error }
                    }
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(lines) { LogRow($0, compact: true) }
                    }
                    .padding(.vertical, 10)
                    .gravitonCard(G.console, radius: 16)
                    // Room for the floating Follow button, and the scroll target when following.
                    Color.clear.frame(height: 64).id("end")
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
            }
            .background(G.background)
            .navigationTitle("Logs")
            .searchable(text: $query, prompt: "Filter")
            .overlay(alignment: .bottom) {
                Button {
                    follow.toggle()
                    if follow { proxy.scrollTo("end", anchor: .bottom) }
                } label: {
                    HStack(spacing: 8) {
                        Circle().fill(follow ? G.onAccent : G.muted).frame(width: 7, height: 7)
                        Text(follow ? "Following live" : "Paused")
                    }
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(follow ? G.onAccent : G.text)
                    .padding(.horizontal, 18)
                    .frame(height: 44)
                    .background(follow ? G.accent : G.control, in: Capsule())
                    .shadow(color: .black.opacity(0.5), radius: 15, y: 10)
                }
                .padding(.bottom, 16)
            }
            .onChange(of: lines.last?.id) {
                if follow { withAnimation { proxy.scrollTo("end", anchor: .bottom) } }
            }
        }
        .task(id: containerID) {
            while !Task.isCancelled {
                if follow || store.logs[containerID] == nil { await store.loadLogs(for: containerID) }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }
}

struct LevelChip: View {
    var title: String
    var tint: Color
    var selected: Bool
    var action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(.system(size: 13, weight: selected ? .semibold : .regular))
                .foregroundStyle(selected ? G.background : tint)
                .padding(.horizontal, 14)
                .frame(height: 32)
                .background(selected ? G.text : .clear, in: Capsule())
                .overlay(Capsule().strokeBorder(selected ? .clear : tint.opacity(0.3)))
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
