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
    @Environment(Dialogs.self) private var dialogs
    @Binding var pairTarget: PairingTarget?
    @State private var askAddress = false
    @State private var editingHost: RemoteHost?

    private var unpaired: [HostBrowser.Found] {
        browser.found.filter { found in
            !hosts.hosts.contains {
                $0.serviceName == found.name
                    || $0.name == found.name
                    || (found.fingerprint != nil && found.fingerprint?.caseInsensitiveCompare($0.fingerprint) == .orderedSame)
            }
        }
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    VStack(alignment: .leading, spacing: 8) {
                        GCaption("Showing").padding(.horizontal, 4)
                        VStack(spacing: 0) {
                            HostRow(title: "Demo data", detail: "The sample containers from the design", symbol: "sparkles",
                                    selected: hosts.selectedID == nil) { choose(nil) }
                            ForEach(hosts.hosts) { host in
                                RowDivider(inset: 62)
                                HostRow(title: host.name, detail: detail(for: host), symbol: "desktopcomputer",
                                        selected: hosts.selectedID == host.id) { choose(host) }
                                    .contextMenu {
                                        Button("Edit Addresses…", systemImage: "network") { editingHost = host }
                                        Button("Forget \(host.name)", systemImage: "trash", role: .destructive) { confirmForget(host) }
                                    }
                            }
                        }
                        .mobileCard()
                        if !hosts.hosts.isEmpty {
                            Text("Touch and hold a Mac to forget it.").font(.system(size: 12)).foregroundStyle(G.faint).padding(.horizontal, 4)
                        }
                    }

                    if store.connection == .live, let engine = store.engine, engine.memoryBytes > 0 {
                        VStack(alignment: .leading, spacing: 8) {
                            GCaption(store.liveName ?? "Connected Mac").padding(.horizontal, 4)
                            MobileEngineMemoryCard(engine: engine)
                        }
                    }

                    VStack(alignment: .leading, spacing: 8) {
                        GCaption("On this network").padding(.horizontal, 4)
                        VStack(spacing: 0) {
                            if unpaired.isEmpty {
                                HStack(spacing: 12) {
                                    GSpinner(size: 16).frame(width: 34)
                                    Text(browser.problem ?? "Looking for Macs running gravitond --remote…")
                                        .font(.system(size: 14)).foregroundStyle(G.muted)
                                }
                                .padding(.horizontal, 14)
                                .frame(maxWidth: .infinity, minHeight: 58, alignment: .leading)
                            }
                            ForEach(Array(unpaired.enumerated()), id: \.element.id) { i, found in
                                if i > 0 { RowDivider(inset: 62) }
                                HStack(spacing: 12) {
                                    HostIcon(symbol: "desktopcomputer", selected: false)
                                    Text(found.name).font(.system(size: 15, weight: .medium)).foregroundStyle(G.text)
                                    Spacer()
                                    Button("Pair") { pairTarget = PairingTarget(name: found.name, serviceName: found.name) }
                                        .buttonStyle(.g(.primary, size: .small))
                                }
                                .padding(.horizontal, 14)
                                .frame(minHeight: 58)
                            }
                        }
                        .mobileCard()
                    }

                    VStack(spacing: 10) {
                        Button { askAddress = true } label: { Label("Add a Mac by address", systemImage: "number") }
                            .buttonStyle(.g(.secondary, size: .touch, fullWidth: true))
                        Button { pasteLink() } label: { Label("Paste a pairing link", systemImage: "doc.on.clipboard") }
                            .buttonStyle(.g(.secondary, size: .touch, fullWidth: true))
                    }

                    HStack(alignment: .top, spacing: 10) {
                        Image(systemName: "info.circle").foregroundStyle(G.accent)
                        Text("On the Mac, start gravitond with --remote, then run grav pair for a code. Or scan its QR code with the Camera.")
                            .font(.system(size: 13)).foregroundStyle(G.muted).fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(14)
                    .mobileCard(G.surface, radius: 14)
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                MobileHeader("Hosts", subtitle: "The Macs this phone runs containers on", large: true).background(G.background)
            }
            .mobileScreen(pushed: false)
            .onAppear { browser.start() }
            .onChange(of: browser.found) { _, found in
                hosts.syncBonjour(found)
            }
            .sheet(item: $editingHost) { host in
                MobileEditHostSheet(host: host) {
                    if hosts.selectedID == host.id {
                        store.use(hosts.engineForSelection(), named: host.name)
                    }
                }
            }
            .sheet(isPresented: $askAddress) {
                AddressSheet { target in
                    askAddress = false
                    // Let this sheet go before the pairing one comes up.
                    Task {
                        try? await Task.sleep(for: .milliseconds(450))
                        pairTarget = target
                    }
                }
                .presentationDetents([.height(300)])
            }
        }
    }

    private func confirmForget(_ host: RemoteHost) {
        dialogs.confirm("Forget \(host.name)?", message: "This phone stops showing it. Pair again with a new code from grav pair.", confirm: "Forget") {
            forget(host)
        }
    }

    private func detail(for host: RemoteHost) -> String {
        let addrs = host.allAddresses
        let place: String
        if host.serviceName != nil {
            place = addrs.isEmpty ? "on this network" : "Bonjour + \(addrs.count) \(addrs.count == 1 ? "address" : "addresses")"
        } else if addrs.count > 1 {
            place = "\(addrs[0]):\(host.port) (+\(addrs.count - 1))"
        } else {
            place = "\(addrs.first ?? "?"):\(host.port)"
        }
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
            dialogs.alert("That isn't a pairing link", message: "Copy the line starting graviton://pair that grav pair prints, then try again.", symbol: "link")
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
                HostIcon(symbol: symbol, selected: selected)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).font(.system(size: 15, weight: .medium)).foregroundStyle(G.text)
                    Text(detail).font(G.mono(11)).foregroundStyle(G.muted).lineLimit(1)
                }
                Spacer()
                ZStack {
                    Circle().strokeBorder(selected ? G.accent : G.faint.opacity(0.6), lineWidth: 1.5)
                    if selected { Circle().fill(G.accent).padding(5) }
                }
                .frame(width: 22, height: 22)
            }
            .padding(.horizontal, 14)
            .frame(minHeight: 62)
            .contentShape(Rectangle())
        }
        .buttonStyle(PressRowStyle())
        .animation(.snappy(duration: 0.2), value: selected)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

private struct HostIcon: View {
    var symbol: String
    var selected: Bool

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: 15, weight: .medium))
            .foregroundStyle(selected ? G.accent : G.muted)
            .frame(width: 36, height: 36)
            .background((selected ? G.accent : G.muted).opacity(0.12), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }
}

/// Type a Mac's address, in place of a system alert with a field.
private struct AddressSheet: View {
    var next: (PairingTarget) -> Void
    @State private var address = ""
    @FocusState private var focused: Bool

    private var target: PairingTarget? {
        let trimmed = address.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { return nil }
        let parts = trimmed.split(separator: ":")
        let port = parts.count == 2 ? UInt16(parts[1]) ?? RemoteHost.defaultPort : RemoteHost.defaultPort
        return PairingTarget(name: String(parts[0]), address: String(parts[0]), port: port)
    }

    var body: some View {
        MobileSheet("Add a Mac", actionTitle: "Next", actionEnabled: target != nil, action: { if let target { next(target) } }) {
            GFormRow("Address", hint: "Its address on your network, shown by grav pair.") {
                MobileField(placeholder: "192.168.1.20", text: $address, keyboard: .numbersAndPunctuation)
                    .focused($focused)
                    .onSubmit { if let target { next(target) } }
            }
        }
        .onAppear { focused = true }
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
        MobileSheet("Pair a Mac", cancelTitle: finished == nil ? "Cancel" : "Done",
                    actionTitle: finished == nil ? "Pair" : nil, actionEnabled: !busy && normalized.count == 8, action: pair) {
            HStack(spacing: 12) {
                Image(systemName: "desktopcomputer")
                    .font(.system(size: 17, weight: .medium))
                    .foregroundStyle(G.accent)
                    .frame(width: 44, height: 44)
                    .background(G.accent.opacity(0.13), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                VStack(alignment: .leading, spacing: 2) {
                    Text(target.name).font(.system(size: 16, weight: .semibold)).foregroundStyle(G.text)
                    Text(target.serviceName != nil ? "Found on this network" : "\(target.address ?? "?"):\(target.port)")
                        .font(G.mono(12)).foregroundStyle(G.muted)
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .mobileCard()

            if finished == nil {
                GFormRow("Code", hint: target.fingerprint == nil
                         ? "Run grav pair on the Mac. The code works once, for five minutes."
                         : "From the Mac's pairing link, which also pins its certificate (\(RemoteHost.short(target.fingerprint!))).") {
                    GTextField("XXXX-XXXX", text: $code)
                        .textInputAutocapitalization(.characters)
                        .autocorrectionDisabled()
                        .focused($focused)
                        .disabled(busy)
                        .onSubmit(pair)
                        .gravitonField(mono: true, height: 58, size: 24)
                }
            }

            switch phase {
            case .idle:
                EmptyView()
            case .pairing:
                GStatusLine(.working, "Pairing…")
            case let .failed(message):
                GStatusLine(.failure, message)
            case let .done(host):
                VStack(alignment: .leading, spacing: 10) {
                    GStatusLine(.success, "Paired with \(host.name)")
                    Text("Check this matches the fingerprint grav pair showed:").font(.system(size: 13)).foregroundStyle(G.muted)
                    Text(host.shortFingerprint)
                        .font(G.mono(19, weight: .semibold))
                        .kerning(1)
                        .foregroundStyle(G.text)
                        .frame(maxWidth: .infinity, minHeight: 56)
                        .background(G.surface, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(G.accent.opacity(0.4), style: StrokeStyle(lineWidth: 1, dash: [4, 3])))
                }
            }
        }
        .interactiveDismissDisabled(busy)
        .onAppear { focused = target.code == nil }
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

/// Edit fallback addresses (e.g. LAN + Tailscale), port, and Bonjour service name on iOS.
private struct MobileEditHostSheet: View {
    @Environment(HostStore.self) private var hosts
    @Environment(\.dismiss) private var dismiss
    let host: RemoteHost
    var onSaved: () -> Void

    @State private var addressesText: String
    @State private var portText: String
    @State private var serviceName: String

    init(host: RemoteHost, onSaved: @escaping () -> Void) {
        self.host = host
        self.onSaved = onSaved
        _addressesText = State(initialValue: host.allAddresses.joined(separator: "\n"))
        _portText = State(initialValue: String(host.port))
        _serviceName = State(initialValue: host.serviceName ?? "")
    }

    private var parsedAddresses: [String] {
        RemoteHost.normalize(addressesText.split(whereSeparator: { $0.isNewline || $0 == "," }).map(String.init))
    }

    private var validPort: UInt16? {
        UInt16(portText.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    var body: some View {
        MobileSheet("Edit \(host.name)",
                    subtitle: "Add LAN and Tailscale addresses; Graviton tries whichever connects fastest alongside Bonjour.",
                    actionTitle: "Save",
                    actionEnabled: validPort != nil && (!parsedAddresses.isEmpty || !serviceName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty),
                    action: save) {
            GFormRow("Addresses", hint: "One IP or hostname per line.") {
                TextEditor(text: $addressesText)
                    .scrollContentBackground(.hidden)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .frame(height: 92)
                    .gravitonField(mono: true, height: nil, size: 15)
            }
            GFormRow("Bonjour service") {
                MobileField(placeholder: host.name, text: $serviceName, mono: false)
            }
            GFormRow("Port") {
                MobileField(placeholder: "7843", text: $portText, keyboard: .numberPad)
            }
        }
    }

    private func save() {
        guard let port = validPort else { return }
        hosts.updateAddresses(for: host.id, addresses: parsedAddresses, port: port, serviceName: serviceName)
        onSaved()
        dismiss()
    }
}
