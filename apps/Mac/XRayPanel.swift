import GravitonKit
import SwiftUI

/// Live hypervisor X-Ray event waterfall, zero-bypass egress firewall editor, and switch chaos controls.
struct XRayPanel: View {
    @Environment(EngineStore.self) private var store
    var container: Container

    @State private var events: [XRayEvent] = []
    @State private var policy = ContainerNetworkPolicy()
    @State private var xrayEnabled = false

    @State private var filterKind: XRayEventKind?
    @State private var blockedOnly = false
    @State private var search = ""

    @State private var egressMode: EgressModeChoice = .unrestricted
    @State private var allowlistText = ""
    @State private var latencyMs = ""
    @State private var jitterMs = ""
    @State private var lossPercent = ""
    @State private var bandwidthKbps = ""

    @State private var savingPolicy = false
    @State private var loading = false
    @State private var problem: String?
    @State private var showPolicyEditor = true

    enum EgressModeChoice: String, CaseIterable, Identifiable {
        case unrestricted = "Unrestricted"
        case allowlist = "Allowlist"
        case offline = "Offline"
        var id: String { rawValue }
    }

    private var visibleEvents: [XRayEvent] {
        let q = search.trimmingCharacters(in: .whitespacesAndNewlines)
        return events.reversed().filter { ev in
            (filterKind == nil || ev.kind == filterKind)
                && (!blockedOnly || !ev.allowed)
                && (q.isEmpty
                    || ev.target.localizedCaseInsensitiveContains(q)
                    || ev.action.localizedCaseInsensitiveContains(q)
                    || (ev.detail?.localizedCaseInsensitiveContains(q) ?? false))
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            policyHeaderCard
            toolbar
            if let problem {
                GStatusLine(.failure, problem)
            }
            waterfall
        }
        .padding(.horizontal, 28)
        .padding(.vertical, 16)
        .task(id: "\(container.id)|\(container.isRunning)|\(store.connection)") {
            await loadStateAndStream()
        }
    }

    private var policyHeaderCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                Label("Egress Firewall & Chaos", systemImage: "shield.lefthalf.filled")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(G.textPrimary)

                if policy.hardwareIsolated {
                    badge("Hardware Isolated (No NAT)", tint: G.mint)
                } else {
                    badge(policy.egress.label, tint: policy.egress.isRestricted ? G.amber : G.slate)
                }
                if policy.chaos.isActive {
                    badge(chaosLabel(policy.chaos), tint: G.rose)
                }

                Spacer()

                Toggle(isOn: Binding(
                    get: { xrayEnabled },
                    set: { next in Task { await toggleXRay(next) } }
                )) {
                    HStack(spacing: 6) {
                        Circle()
                            .fill(xrayEnabled ? G.mint : G.slate)
                            .frame(width: 7, height: 7)
                        Text(xrayEnabled ? "Recording X-Ray" : "X-Ray Paused")
                            .font(.system(size: 12, weight: .medium))
                            .foregroundStyle(G.textSecondary)
                    }
                }
                .toggleStyle(.switch)
                .controlSize(.small)

                Button(showPolicyEditor ? "Hide Controls" : "Edit Policy") {
                    withAnimation(.easeInOut(duration: 0.15)) { showPolicyEditor.toggle() }
                }
                .buttonStyle(.g(.secondary, size: .small))
            }

            if showPolicyEditor {
                VStack(alignment: .leading, spacing: 10) {
                    HStack(spacing: 10) {
                        Text("Egress")
                            .font(.system(size: 12, weight: .medium))
                            .foregroundStyle(G.textMuted)
                            .frame(width: 58, alignment: .leading)
                        Picker("", selection: $egressMode) {
                            ForEach(EgressModeChoice.allCases) { choice in
                                Text(choice.rawValue).tag(choice)
                            }
                        }
                        .pickerStyle(.segmented)
                        .labelsHidden()
                        .frame(width: 260)

                        if egressMode == .allowlist {
                            GTextField("Hosts or CIDRs (e.g. api.github.com, 10.0.0.0/8)", text: $allowlistText)
                                .gravitonField(mono: true, height: 30)
                        }
                        Spacer()
                    }

                    HStack(spacing: 10) {
                        Text("Chaos")
                            .font(.system(size: 12, weight: .medium))
                            .foregroundStyle(G.textMuted)
                            .frame(width: 58, alignment: .leading)
                        labeledNumField("Latency ms", text: $latencyMs, width: 88)
                        labeledNumField("Jitter ms", text: $jitterMs, width: 82)
                        labeledNumField("Loss %", text: $lossPercent, width: 72)
                        labeledNumField("Cap Kbps", text: $bandwidthKbps, width: 92)
                        Spacer()
                        if policy.chaos.isActive {
                            Button("Clear Chaos") {
                                latencyMs = ""
                                jitterMs = ""
                                lossPercent = ""
                                bandwidthKbps = ""
                                Task { await applyPolicy(clearChaos: true) }
                            }
                            .buttonStyle(.g(.secondary, size: .small))
                            .disabled(savingPolicy)
                        }
                        Button(savingPolicy ? "Applying…" : "Apply Policy") {
                            Task { await applyPolicy(clearChaos: false) }
                        }
                        .buttonStyle(.g(.primary, size: .small))
                        .disabled(savingPolicy || !store.canManage)
                    }
                }
                .padding(.top, 2)
            }
        }
        .padding(14)
        .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }

    private var toolbar: some View {
        HStack(spacing: 8) {
            GFilterPillButton("All (\(events.count))", selected: filterKind == nil && !blockedOnly, accent: G.indigo) {
                filterKind = nil
                blockedOnly = false
            }
            ForEach(XRayEventKind.allCases, id: \.self) { kind in
                GFilterPillButton(kind.label, selected: filterKind == kind, accent: tint(for: kind)) {
                    filterKind = (filterKind == kind) ? nil : kind
                }
            }
            GFilterPillButton("Blocked", selected: blockedOnly, accent: G.rose) {
                blockedOnly.toggle()
            }

            GTextField("Filter targets or actions", text: $search)
                .gravitonField(mono: false, height: 30, icon: "magnifyingglass")
                .frame(maxWidth: 240)

            Spacer()

            Button("Clear", systemImage: "trash") { events.removeAll() }
                .buttonStyle(.g(.secondary, size: .small))
                .disabled(events.isEmpty)

            Button {
                Task { await refreshSnapshot() }
            } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
            }
            .buttonStyle(.g(.secondary, size: .small))
            .disabled(loading)
        }
    }

    @ViewBuilder
    private var waterfall: some View {
        if loading && events.isEmpty {
            GSpinner(size: 22).frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(G.console, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        } else if visibleEvents.isEmpty {
            GEmptyState(
                events.isEmpty ? (xrayEnabled ? "Waiting for events" : "X-Ray is paused") : "No matching events",
                symbol: "waveform.path.ecg",
                detail: events.isEmpty
                    ? (xrayEnabled
                        ? "DNS queries, outbound connections, process spawns, and file opens appear here live."
                        : "Toggle Recording X-Ray above to stream kernel, DNS, and network events.")
                    : "Adjust your kind filter or search query."
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(G.console, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        } else {
            ScrollView {
                LazyVStack(spacing: 3) {
                    ForEach(visibleEvents) { ev in
                        eventRow(ev)
                    }
                }
                .padding(10)
            }
            .background(G.console, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        }
    }

    private func eventRow(_ ev: XRayEvent) -> some View {
        HStack(spacing: 10) {
            Text(Format.clock(ev.date).prefix(12))
                .font(G.mono(11))
                .foregroundStyle(G.textTertiary)
                .frame(width: 82, alignment: .leading)

            Text(ev.kind.label.uppercased())
                .font(G.mono(10, weight: .bold))
                .foregroundStyle(tint(for: ev.kind))
                .frame(width: 44, height: 20)
                .background(tint(for: ev.kind).opacity(0.14), in: RoundedRectangle(cornerRadius: 5, style: .continuous))

            Text(ev.action)
                .font(G.mono(11.5, weight: .medium))
                .foregroundStyle(G.textSecondary)
                .frame(width: 72, alignment: .leading)

            Text(ev.target)
                .font(G.mono(12))
                .foregroundStyle(ev.allowed ? G.textPrimary : G.rose)
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)

            if let detail = ev.detail, !detail.isEmpty {
                Text(detail)
                    .font(G.mono(11))
                    .foregroundStyle(G.textMuted)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }

            Spacer(minLength: 8)

            if let lat = ev.latencyUs {
                Text(lat >= 1000 ? String(format: "%.1fms", Double(lat) / 1000) : "\(lat)us")
                    .font(G.mono(11))
                    .foregroundStyle(G.textTertiary)
            }
            if let bytes = ev.bytes, bytes > 0 {
                Text(Format.bytes(bytes))
                    .font(G.mono(11))
                    .foregroundStyle(G.textTertiary)
            }
            if let pid = ev.pid {
                Text("pid \(pid)")
                    .font(G.mono(10.5))
                    .foregroundStyle(G.textTertiary)
            }

            Text(ev.allowed ? "ALLOW" : "BLOCK")
                .font(G.mono(10, weight: .bold))
                .foregroundStyle(ev.allowed ? G.mint : G.rose)
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
                .background((ev.allowed ? G.mint : G.rose).opacity(0.14), in: Capsule())
        }
        .padding(.horizontal, 10)
        .frame(minHeight: 30)
        .background(
            (ev.allowed ? G.surfaceSubtle.opacity(0.45) : G.rose.opacity(0.08)),
            in: RoundedRectangle(cornerRadius: 7, style: .continuous)
        )
    }

    private func labeledNumField(_ placeholder: String, text: Binding<String>, width: CGFloat) -> some View {
        GTextField(placeholder, text: text)
            .gravitonField(mono: true, height: 30)
            .frame(width: width)
    }

    private func badge(_ text: String, tint: Color) -> some View {
        Text(text)
            .font(.system(size: 11, weight: .medium))
            .foregroundStyle(tint)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(tint.opacity(0.14), in: Capsule())
    }

    private func tint(for kind: XRayEventKind) -> Color {
        switch kind {
        case .dns: G.sky
        case .net: G.indigo
        case .file: G.amber
        case .proc: G.mint
        }
    }

    private func chaosLabel(_ chaos: ChaosProfile) -> String {
        var parts: [String] = []
        if chaos.latencyMs > 0 { parts.append("\(chaos.latencyMs)ms") }
        if chaos.jitterMs > 0 { parts.append("±\(chaos.jitterMs)ms") }
        if chaos.lossPercent > 0 { parts.append("\(chaos.lossPercent)% loss") }
        if let bw = chaos.bandwidthKbps { parts.append("\(bw) Kbps") }
        return parts.joined(separator: " · ")
    }

    private func syncPolicyInputs(_ p: ContainerNetworkPolicy) {
        policy = p
        switch p.egress {
        case .unrestricted:
            egressMode = .unrestricted
            allowlistText = ""
        case .offline:
            egressMode = .offline
            allowlistText = ""
        case let .allowlist(hosts, cidrs):
            egressMode = .allowlist
            allowlistText = (hosts + cidrs).joined(separator: ", ")
        }
        latencyMs = p.chaos.latencyMs > 0 ? "\(p.chaos.latencyMs)" : ""
        jitterMs = p.chaos.jitterMs > 0 ? "\(p.chaos.jitterMs)" : ""
        lossPercent = p.chaos.lossPercent > 0 ? "\(p.chaos.lossPercent)" : ""
        bandwidthKbps = p.chaos.bandwidthKbps.map { "\($0)" } ?? ""
    }

    private func loadStateAndStream() async {
        loading = true
        do {
            let details = try await store.inspect(container.id)
            xrayEnabled = details.xrayEnabled ?? false
            if let pol = details.networkPolicy {
                syncPolicyInputs(pol)
            } else {
                syncPolicyInputs(try await store.getNetworkPolicy(id: container.id))
            }
            events = try await store.xrayEvents(id: container.id, limit: 250)
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
        loading = false

        guard container.isRunning || xrayEnabled else { return }
        do {
            for try await ev in store.xrayStream(id: container.id, limit: 250) {
                if Task.isCancelled { break }
                if !events.contains(where: { $0.seq == ev.seq && ev.seq > 0 }) {
                    events.append(ev)
                    if events.count > 1000 { events.removeFirst(events.count - 1000) }
                }
            }
        } catch is CancellationError {
            return
        } catch {
            if !Task.isCancelled { problem = error.localizedDescription }
        }
    }

    private func refreshSnapshot() async {
        loading = true
        defer { loading = false }
        do {
            syncPolicyInputs(try await store.getNetworkPolicy(id: container.id))
            events = try await store.xrayEvents(id: container.id, limit: 250)
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }

    private func toggleXRay(_ enabled: Bool) async {
        do {
            xrayEnabled = try await store.setXRay(id: container.id, enabled: enabled)
            if enabled {
                await loadStateAndStream()
            }
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }

    private func applyPolicy(clearChaos: Bool) async {
        savingPolicy = true
        defer { savingPolicy = false }
        let nextEgress: EgressPolicy?
        switch egressMode {
        case .unrestricted:
            nextEgress = .unrestricted
        case .offline:
            nextEgress = .offline
        case .allowlist:
            guard let parsed = EgressPolicy.parse("allow:\(allowlistText)") else {
                problem = "Enter at least one hostname or CIDR for the allowlist."
                return
            }
            nextEgress = parsed
        }
        let nextChaos = clearChaos
            ? ChaosProfile()
            : ChaosProfile(
                latencyMs: UInt32(latencyMs.trimmingCharacters(in: .whitespaces)) ?? 0,
                jitterMs: UInt32(jitterMs.trimmingCharacters(in: .whitespaces)) ?? 0,
                lossPercent: UInt8(min(100, Int(lossPercent.trimmingCharacters(in: .whitespaces)) ?? 0)),
                bandwidthKbps: UInt32(bandwidthKbps.trimmingCharacters(in: .whitespaces))
            )
        do {
            let updated = try await store.setNetworkPolicy(id: container.id, egress: nextEgress, chaos: nextChaos)
            syncPolicyInputs(updated)
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }
}
