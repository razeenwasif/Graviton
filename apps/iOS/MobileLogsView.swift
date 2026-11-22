import GravitonKit
import SwiftUI

struct MobileLogsView: View {
    @Environment(EngineStore.self) private var store
    var containerID: String
    @State private var query = ""
    @State private var level: LogLevel?
    @State private var follow = true
    @State private var showTimestamps = true
    @State private var window: LogTimeWindow = .all
    @State private var loadedWindow: LogTimeWindow?

    private struct StreamKey: Equatable {
        var id: String
        var follow: Bool
        var window: LogTimeWindow
        var active: Bool
        var connection: EngineStore.Connection
    }

    private var containerActive: Bool {
        store.container(containerID)?.isActive ?? false
    }

    private var streamKey: StreamKey {
        StreamKey(
            id: containerID,
            follow: follow,
            window: window,
            active: containerActive,
            connection: store.connection
        )
    }

    private var lines: [LogLine] {
        (store.logs[containerID] ?? []).filter {
            (level == nil || $0.level == level) && (query.isEmpty || $0.message.localizedCaseInsensitiveContains(query))
        }
    }

    private var followPillText: String {
        if !follow { return "Paused" }
        return containerActive ? "Live stream" : "End of logs"
    }

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    MobileSearchField(prompt: "Filter lines", text: $query)
                    GSegmented(selection: $level, options: [(nil, "All"), (.info, "Info"), (.warn, "Warn"), (.error, "Error")], stretch: true)
                    GSegmented(selection: $window, options: LogTimeWindow.allCases.map { ($0, $0.rawValue) }, stretch: true)
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(lines) { LogRow($0, showTimestamp: showTimestamps, compact: true) }
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
                MobileHeader("Logs", subtitle: store.container(containerID)?.name) {
                    Button {
                        showTimestamps.toggle()
                    } label: {
                        Label("Timestamps", systemImage: showTimestamps ? "clock.fill" : "clock")
                    }
                    .buttonStyle(CircleIconStyle(prominent: showTimestamps))
                    .accessibilityLabel(showTimestamps ? "Hide timestamps" : "Show timestamps")
                }
                .background(G.background)
            }
            .safeAreaInset(edge: .bottom, spacing: 0) {
                Button {
                    follow.toggle()
                    if follow { proxy.scrollTo("end", anchor: .bottom) }
                } label: {
                    HStack(spacing: 8) {
                        Circle().fill(follow ? G.onAccent : G.muted).frame(width: 7, height: 7)
                        Text(followPillText)
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
        .task(id: streamKey) {
            if follow {
                loadedWindow = window
                await store.followLogs(for: containerID, tail: 500, sinceMs: window.sinceMs())
            } else if store.logs[containerID] == nil || loadedWindow != window {
                loadedWindow = window
                await store.loadLogs(for: containerID, tail: 500, sinceMs: window.sinceMs())
            }
        }
    }
}
