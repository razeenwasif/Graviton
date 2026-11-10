import GravitonKit
import SwiftUI

extension PairingTarget: @retroactive Identifiable {
    public var id: String { "\(name)|\(serviceName ?? "")|\(address ?? ""):\(port)" }
}

/// The Hosts tab: which Mac this phone shows, paired Macs, and Macs on the network.
struct HostsView: View {
    @Environment(EngineStore.self) private var store
    @Environment(HostStore.self) private var hosts
    @Environment(HostBrowser.self) private var browser
    @Binding var pairTarget: PairingTarget?
    @State private var askAddress = false
    @State private var address = ""
    @State private var linkProblem: String?

    private var unpaired: [HostBrowser.Found] {
        browser.found.filter { found in !hosts.hosts.contains { $0.serviceName == found.name || $0.name == found.name } }
    }

    var body: some View {
        NavigationStack {
            List {
                Section("Showing") {
                    HostRow(title: "Demo data", detail: "The sample containers from the design", symbol: "sparkles",
                            selected: hosts.selectedID == nil) { choose(nil) }
                    ForEach(hosts.hosts) { host in
                        HostRow(title: host.name, detail: detail(for: host), symbol: "desktopcomputer",
                                selected: hosts.selectedID == host.id) { choose(host) }
                            .swipeActions {
                                Button("Forget", role: .destructive) { forget(host) }
                            }
                    }
                }

                Section {
                    if unpaired.isEmpty {
                        HStack(spacing: 10) {
                            ProgressView()
                            Text(browser.problem ?? "Looking for Macs running gravitond --remote…")
                                .foregroundStyle(G.muted)
                        }
                    }
                    ForEach(unpaired) { found in
                        Button {
                            pairTarget = PairingTarget(name: found.name, serviceName: found.name)
                        } label: {
                            HStack {
                                Label(found.name, systemImage: "desktopcomputer")
                                    .foregroundStyle(G.text)
                                Spacer()
                                Text("Pair").foregroundStyle(G.accent).fontWeight(.semibold)
                            }
                        }
                    }
                    Button("Add a Mac by address…", systemImage: "number") { askAddress = true }
                    Button("Paste a pairing link", systemImage: "doc.on.clipboard") { pasteLink() }
                } header: {
                    Text("On this network")
                } footer: {
                    Text("On the Mac, start gravitond with --remote, then run grav pair for a code. Or scan its QR code with the Camera.")
                }
            }
            .scrollContentBackground(.hidden)
            .background(G.background)
            .navigationTitle("Hosts")
            .onAppear { browser.start() }
            .alert("Add a Mac", isPresented: $askAddress) {
                TextField("192.168.1.20", text: $address)
                    .keyboardType(.numbersAndPunctuation)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                Button("Cancel", role: .cancel) {}
                Button("Next") {
                    let trimmed = address.trimmingCharacters(in: .whitespaces)
                    guard !trimmed.isEmpty else { return }
                    let parts = trimmed.split(separator: ":")
                    let port = parts.count == 2 ? UInt16(parts[1]) ?? RemoteHost.defaultPort : RemoteHost.defaultPort
                    pairTarget = PairingTarget(name: String(parts[0]), address: String(parts[0]), port: port)
                }
            } message: {
                Text("Its address on your network, shown by grav pair.")
            }
            .alert("That isn't a pairing link", isPresented: Binding(get: { linkProblem != nil }, set: { if !$0 { linkProblem = nil } })) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(linkProblem ?? "")
            }
        }
    }

    private func detail(for host: RemoteHost) -> String {
        let place = host.serviceName != nil ? "on this network" : "\(host.address ?? "?"):\(host.port)"
        return "\(place) · \(host.shortFingerprint)"
    }

    private func choose(_ host: RemoteHost?) {
        hosts.select(host?.id)
        store.use(hosts.engineForSelection(), named: hosts.selected?.name)
    }

    private func forget(_ host: RemoteHost) {
        let wasSelected = hosts.selectedID == host.id
        hosts.forget(host.id)
        if wasSelected { store.use(nil, named: nil) }
    }

    private func pasteLink() {
        guard let text = UIPasteboard.general.string, let target = PairingTarget(link: text) else {
            linkProblem = "Copy the line starting graviton://pair that grav pair prints, then try again."
            return
        }
        pairTarget = target
    }
}

private struct HostRow: View {
    var title: String
    var detail: String
    var symbol: String
    var selected: Bool
    var action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 12) {
                Image(systemName: symbol).foregroundStyle(G.accent).frame(width: 24)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).foregroundStyle(G.text)
                    Text(detail).font(G.mono(11)).foregroundStyle(G.muted)
                }
                Spacer()
                if selected {
                    Image(systemName: "checkmark").foregroundStyle(G.accent).fontWeight(.semibold)
                }
            }
        }
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// Trade a code from `grav pair` for a token, then switch to that Mac.
struct PairSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(HostStore.self) private var hosts
    @Environment(\.dismiss) private var dismiss
    let target: PairingTarget
    @State private var code: String
    @State private var phase: Phase = .idle
    @FocusState private var focused: Bool

    enum Phase {
        case idle, pairing
        case failed(String)
        case done(RemoteHost)
    }

    init(target: PairingTarget) {
        self.target = target
        _code = State(initialValue: target.code ?? "")
    }

    private var normalized: String { code.uppercased().filter { $0.isLetter || $0.isNumber } }
    private var busy: Bool { if case .pairing = phase { true } else { false } }
    private var finished: RemoteHost? { if case let .done(host) = phase { host } else { nil } }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    LabeledContent("Mac", value: target.name)
                    LabeledContent("Where", value: target.serviceName != nil ? "Found on this network" : "\(target.address ?? "?"):\(target.port)")
                }
                if finished == nil {
                    Section {
                        TextField("XXXX-XXXX", text: $code)
                            .font(G.mono(22, weight: .medium))
                            .textInputAutocapitalization(.characters)
                            .autocorrectionDisabled()
                            .focused($focused)
                            .disabled(busy)
                            .onSubmit(pair)
                    } header: {
                        Text("Code")
                    } footer: {
                        Text(target.fingerprint == nil
                             ? "Run grav pair on the Mac. The code works once, for five minutes."
                             : "From the Mac's pairing link, which also pins its certificate (\(RemoteHost.short(target.fingerprint!))).")
                    }
                }
                Section {
                    switch phase {
                    case .idle:
                        EmptyView()
                    case .pairing:
                        HStack(spacing: 10) { ProgressView(); Text("Pairing…") }
                    case let .failed(message):
                        Label(message, systemImage: "xmark.octagon.fill").foregroundStyle(G.danger)
                    case let .done(host):
                        VStack(alignment: .leading, spacing: 8) {
                            Label("Paired with \(host.name)", systemImage: "checkmark.circle.fill").foregroundStyle(G.running)
                            Text("Check this matches the fingerprint grav pair showed:").font(.footnote).foregroundStyle(G.muted)
                            Text(host.shortFingerprint).font(G.mono(17, weight: .semibold))
                        }
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(G.background)
            .navigationTitle("Pair a Mac")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                if finished != nil {
                    ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
                } else {
                    ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() }.disabled(busy) }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Pair", action: pair).disabled(busy || normalized.count != 8)
                    }
                }
            }
            .onAppear { focused = target.code == nil }
        }
    }

    private func pair() {
        guard !busy, normalized.count == 8 else { return }
        phase = .pairing
        let code = normalized
        Task {
            do {
                let result = try await Pairing.pair(target, code: code, deviceName: UIDevice.current.name)
                try hosts.add(result)
                store.use(hosts.engineForSelection(), named: result.host.name)
                phase = .done(result.host)
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }
}
