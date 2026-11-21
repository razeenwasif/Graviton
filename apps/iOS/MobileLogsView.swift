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
                    MobileSearchField(prompt: "Filter lines", text: $query)
                    GSegmented(selection: $level, options: [(nil, "All"), (.info, "Info"), (.warn, "Warn"), (.error, "Error")], stretch: true)
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(lines) { LogRow($0, compact: true) }
                    }
                    .padding(.vertical, 10)
                    .gravitonCard(G.console, radius: 16)
                    // The scroll target when following.
                    Color.clear.frame(height: 1).id("end")
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                MobileHeader("Logs", subtitle: store.container(containerID)?.name).background(G.background)
            }
            .safeAreaInset(edge: .bottom, spacing: 0) {
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
                .padding(.vertical, 12)
                .frame(maxWidth: .infinity)
            }
            .mobileScreen()
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
