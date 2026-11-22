import GravitonKit
import SwiftUI

/// What happened on the Mac this phone shows: containers starting, stopping
/// and failing, and images pulled and built. Newest first.
struct ActivityView: View {
    @Environment(EngineStore.self) private var store
    @State private var problemsOnly = false

    private var shown: [EngineEvent] {
        store.events.reversed().filter { !problemsOnly || $0.isProblem }
    }

    private var days: [(Date, [EngineEvent])] {
        let groups = Dictionary(grouping: shown) { Calendar.current.startOfDay(for: $0.date) }
        return groups.keys.sorted(by: >).map { ($0, groups[$0]!) }
    }

    private func dayTitle(_ day: Date) -> String {
        if Calendar.current.isDateInToday(day) { return "Today" }
        if Calendar.current.isDateInYesterday(day) { return "Yesterday" }
        return day.formatted(.dateTime.weekday(.wide).day().month(.wide))
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    GSegmented(selection: $problemsOnly, options: [(false, "All"), (true, "Problems")], stretch: true)

                    if shown.isEmpty {
                        GEmptyState(problemsOnly ? "No problems" : "Nothing yet", symbol: problemsOnly ? "checkmark.shield" : "waveform.path.ecg",
                                    detail: problemsOnly ? "Failed containers and builds show up here." : "Containers starting and stopping, pulls and builds show up here.")
                            .frame(minHeight: 420)
                    }

                    ForEach(days, id: \.0) { day, events in
                        VStack(alignment: .leading, spacing: 8) {
                            GCaption(dayTitle(day)).padding(.horizontal, 4)
                            VStack(spacing: 0) {
                                ForEach(Array(events.enumerated()), id: \.element.id) { i, event in
                                    if i > 0 { RowDivider(inset: 52) }
                                    if let id = event.containerId, store.container(id) != nil {
                                        NavigationLink(value: id) {
                                            HStack {
                                                EventRow(event: event)
                                                Image(systemName: "chevron.right").font(.system(size: 11, weight: .semibold)).foregroundStyle(Color(hex: 0x5F6672))
                                            }
                                            .padding(.horizontal, 14)
                                            .contentShape(Rectangle())
                                        }
                                        .buttonStyle(PressRowStyle())
                                    } else {
                                        EventRow(event: event).padding(.horizontal, 14)
                                    }
                                }
                            }
                            .mobileCard()
                        }
                    }
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                MobileHeader("Activity", subtitle: store.liveName.map { "On \($0)" }, large: true).background(G.background)
            }
            .refreshable { await store.refresh() }
            .mobileScreen(pushed: false)
            .navigationDestination(for: String.self) { ContainerDetailView(containerID: $0) }
        }
    }
}

struct EventRow: View {
    var event: EngineEvent

    private var symbol: (String, Color) {
        switch event.kind {
        case .started: ("play.circle.fill", G.running)
        case .stopped: ("stop.circle", G.muted)
        case .paused: ("pause.circle.fill", G.warning)
        case .unpaused: ("play.circle.fill", G.running)
        case .failed: ("exclamationmark.triangle.fill", G.danger)
        case .healthy: ("heart.circle.fill", G.running)
        case .unhealthy: ("heart.slash.circle.fill", G.danger)
        case .pulled: ("arrow.down.circle", G.accent)
        case .pushed: ("arrow.up.circle", G.accent)
        case .built: ("hammer.fill", G.accent)
        case .buildFailed: ("hammer.fill", G.danger)
        case .removed: ("trash", G.muted)
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: symbol.0)
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(symbol.1)
                .frame(width: 28, height: 28)
                .background(symbol.1.opacity(0.13), in: Circle())
            VStack(alignment: .leading, spacing: 3) {
                Text(event.message).font(.system(size: 14.5)).foregroundStyle(G.text).lineLimit(3)
                HStack(spacing: 6) {
                    Text(event.date, format: .dateTime.hour().minute())
                    if let image = event.image, event.container != nil {
                        Text("·")
                        Text(image).font(G.mono(11)).lineLimit(1)
                    }
                }
                .font(.system(size: 12))
                .foregroundStyle(G.faint)
            }
        }
        .padding(.vertical, 11)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }
}
