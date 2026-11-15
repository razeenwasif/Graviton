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
            List {
                Picker("Show", selection: $problemsOnly) {
                    Text("All").tag(false)
                    Text("Problems").tag(true)
                }
                .pickerStyle(.segmented)
                .listRowBackground(Color.clear)
                .listRowInsets(EdgeInsets())

                ForEach(days, id: \.0) { day, events in
                    Section(dayTitle(day)) {
                        ForEach(events) { event in
                            if let id = event.containerId, store.container(id) != nil {
                                NavigationLink(value: id) { EventRow(event: event) }
                            } else {
                                EventRow(event: event)
                            }
                        }
                    }
                    .listRowBackground(G.card)
                }
            }
            .scrollContentBackground(.hidden)
            .background(G.background)
            .overlay {
                if shown.isEmpty {
                    ContentUnavailableView(problemsOnly ? "No problems" : "Nothing yet", systemImage: "waveform.path.ecg",
                                           description: Text(problemsOnly ? "Failed containers and builds show up here." : "Containers starting and stopping, pulls and builds show up here."))
                }
            }
            .navigationTitle("Activity")
            .navigationDestination(for: String.self) { ContainerDetailView(containerID: $0) }
            .refreshable { await store.refresh() }
        }
    }
}

struct EventRow: View {
    var event: EngineEvent

    private var symbol: (String, Color) {
        switch event.kind {
        case .started: ("play.circle.fill", G.running)
        case .stopped: ("stop.circle", G.muted)
        case .failed: ("exclamationmark.triangle.fill", G.danger)
        case .pulled: ("arrow.down.circle", G.accent)
        case .built: ("hammer.fill", G.accent)
        case .buildFailed: ("hammer.fill", G.danger)
        case .removed: ("trash", G.muted)
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: symbol.0).foregroundStyle(symbol.1).frame(width: 22).padding(.top, 2)
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
        .padding(.vertical, 3)
        .accessibilityElement(children: .combine)
    }
}
