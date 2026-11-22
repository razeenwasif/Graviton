import AppKit
import GravitonKit
import SwiftUI

/// Show another Mac (or this one, for `nil`). Shells belong to the Mac they were opened on, so they close.
@MainActor
func switchHost(to mac: PairedMac?, store: EngineStore, hosts: SharedHosts, shells: ShellSessions) {
    if mac?.name == hosts.selected && (mac != nil || store.isLocal) { return } // already showing it
    shells.reset()
    hosts.select(mac?.name)
    if let mac {
        store.use(RemoteEngine(host: mac.remoteHost, token: mac.token), named: mac.name)
    } else {
        store.useLocal()
    }
}

/// The Host menu in the menu bar: the same choices, with ⌥⌘0–9.
struct HostCommands: Commands {
    let store: EngineStore
    let hosts: SharedHosts
    let shells: ShellSessions
    let router: AppRouter

    var body: some Commands {
        CommandMenu("Host") {
            Toggle("This Mac", isOn: Binding(get: { hosts.selected == nil }, set: { if $0 { switchHost(to: nil, store: store, hosts: hosts, shells: shells) } }))
                .keyboardShortcut("0", modifiers: [.command, .option])
            ForEach(Array(hosts.hosts.enumerated()), id: \.element.id) { i, mac in
                let toggle = Toggle(mac.name, isOn: Binding(get: { hosts.selected == mac.name }, set: { if $0 { switchHost(to: mac, store: store, hosts: hosts, shells: shells) } }))
                if i < 9 {
                    toggle.keyboardShortcut(KeyEquivalent(Character("\(i + 1)")), modifiers: [.command, .option])
                } else {
                    toggle
                }
            }
            Divider()
            Button("Pair a Mac…") { router.pending = .pairMac }
            Button("Reload Paired Macs") { hosts.reload() }
        }
    }
}

/// Which Mac the window shows: this one, or one paired with `grav hosts add` or the Pair sheet.
struct HostMenu: View {
    @Environment(EngineStore.self) private var store
    @Environment(SharedHosts.self) private var hosts
    @Environment(ShellSessions.self) private var shells
    @Environment(AppRouter.self) private var router
    @Environment(Dialogs.self) private var dialogs
    @State private var editingHost: PairedMac?

    var body: some View {
        Menu {
            Button {
                switchTo(nil)
            } label: {
                Label("This Mac", systemImage: hosts.selected == nil ? "checkmark" : "laptopcomputer")
            }
            if !hosts.hosts.isEmpty {
                Divider()
                ForEach(hosts.hosts) { mac in
                    let addrs = mac.allAddresses
                    let extra = addrs.count > 1 ? " (+\(addrs.count - 1))" : ""
                    Button {
                        switchTo(mac)
                    } label: {
                        Label("\(mac.name)  ·  \(mac.address)\(extra)", systemImage: hosts.selected == mac.name ? "checkmark" : "desktopcomputer")
                    }
                }
            }
            Divider()
            Button("Pair a Mac…", systemImage: "plus") { router.pending = .pairMac }
            if let current = hosts.current {
                Button("Edit Addresses…", systemImage: "network") { editingHost = current }
                Button("Forget \(current.name)…", systemImage: "trash") { confirmForget(current) }
            }
        } label: {
            GDropdownLabel(hosts.current?.name ?? "This Mac", icon: hosts.selected == nil ? "laptopcomputer" : "desktopcomputer")
                .frame(maxWidth: .infinity)
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .help("Choose the Mac this window shows")
        .onAppear(perform: hosts.reload)
        .sheet(item: $editingHost) { mac in
            EditHostAddressesSheet(mac: mac) { updated in
                if hosts.selected == updated.name {
                    store.use(RemoteEngine(host: updated.remoteHost, token: updated.token), named: updated.name)
                }
            }
        }
    }

    private func confirmForget(_ mac: PairedMac) {
        dialogs.confirm("Forget \(mac.name)?",
                        message: "This Mac and grav stop using it. On \(mac.hostName), grav devices revoke also cancels its token.",
                        confirm: "Forget") {
            try? hosts.forget(mac.name)
            switchTo(nil)
        }
    }

    private func switchTo(_ mac: PairedMac?) {
        switchHost(to: mac, store: store, hosts: hosts, shells: shells)
    }
}

/// Pair with a Mac running `gravitond --remote`, using the code `grav pair` shows on it.
struct PairMacSheet: View {
    @Environment(SharedHosts.self) private var hosts
    @Environment(\.dismiss) private var dismiss
    var onPaired: (PairedMac) -> Void
    @State private var address = ""
    @State private var code = ""
    @State private var name = ""
    @State private var phase: Phase = .idle
    @State private var browser = HostBrowser()
    @State private var pickedBonjour: HostBrowser.Found?

    enum Phase {
        case idle, pairing
        case confirm(PairingResult)
        case failed(String)
    }

    private var busy: Bool { if case .pairing = phase { true } else { false } }
    private var link: PairingTarget? { PairingTarget(link: address) }
    private var normalizedCode: String { (link?.code ?? code).uppercased().filter { $0.isLetter || $0.isNumber } }
    private var discoveredUnpaired: [HostBrowser.Found] {
        browser.found.filter { found in
            !hosts.hosts.contains { mac in
                mac.serviceName == found.name
                    || mac.name == found.name
                    || (found.fingerprint != nil && found.fingerprint?.caseInsensitiveCompare(mac.fingerprint) == .orderedSame)
            }
        }
    }

    var body: some View {
        GSheet("Pair a Mac", subtitle: "On the other Mac, run grav service install --remote once, then grav pair for a code.", symbol: "desktopcomputer.and.arrow.down") {
            if !discoveredUnpaired.isEmpty {
                GFormRow("Nearby on this network") {
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 8) {
                            ForEach(discoveredUnpaired) { found in
                                Button {
                                    pickedBonjour = found
                                    address = found.name
                                    if name.isEmpty { name = PairedMac.shortName(found.name) }
                                } label: {
                                    Label(found.name, systemImage: "bonjour")
                                }
                                .buttonStyle(.g(pickedBonjour?.name == found.name ? .primary : .secondary, size: .small))
                                .disabled(busy)
                            }
                        }
                    }
                }
            }
            GFormRow("Address", hint: link != nil ? "A pairing link: its addresses, code, and certificate fingerprint are filled in." : nil) {
                field("mini.local, 192.168.1.20, or the graviton://pair link", text: $address)
                    .onChange(of: address) { _, next in
                        if next != pickedBonjour?.name { pickedBonjour = nil }
                    }
            }
            HStack(alignment: .top, spacing: 12) {
                if link == nil {
                    GFormRow("Code") { field("XXXX-XXXX", text: $code) }.frame(width: 170)
                }
                GFormRow("Name", hint: "Optional; used with grav --host") { field("e.g. mini", text: $name) }
            }

            switch phase {
            case .idle:
                EmptyView()
            case .pairing:
                GStatusLine(.working, "Pairing…")
            case let .confirm(result):
                VStack(alignment: .leading, spacing: 10) {
                    GStatusLine(.success, "Paired with \(result.host.name)")
                    Text("Check this matches the fingerprint grav pair showed on it:").font(.system(size: 12.5)).foregroundStyle(G.muted)
                    Text(result.host.shortFingerprint)
                        .font(G.mono(18, weight: .semibold))
                        .kerning(1)
                        .foregroundStyle(G.text)
                        .padding(.horizontal, 16)
                        .padding(.vertical, 10)
                        .frame(maxWidth: .infinity)
                        .background(G.background, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(G.accent.opacity(0.4), style: StrokeStyle(lineWidth: 1, dash: [4, 3])))
                }
            case let .failed(message):
                GStatusLine(.failure, message)
            }
        } footer: {
            Spacer()
            Button("Cancel") { dismiss() }.buttonStyle(.gSecondary).keyboardShortcut(.cancelAction)
            if case let .confirm(result) = phase {
                Button("It matches") { save(result) }
                    .buttonStyle(.gPrimary)
                    .keyboardShortcut(.defaultAction)
            } else {
                Button("Pair", action: pair)
                    .buttonStyle(.gPrimary)
                    .keyboardShortcut(.defaultAction)
                    .disabled(busy || address.trimmingCharacters(in: .whitespaces).isEmpty || normalizedCode.count != 8)
            }
        }
        .onAppear { browser.start() }
        .onDisappear { browser.stop() }
    }

    private func field(_ placeholder: String, text: Binding<String>) -> some View {
        GTextField(placeholder, text: text)
            .disabled(busy)
            .gravitonField(mono: true)
    }

    private func pair() {
        let target: PairingTarget
        let typed = address.trimmingCharacters(in: .whitespaces)
        if let link {
            target = link
        } else if let picked = pickedBonjour, typed == picked.name {
            target = PairingTarget(name: picked.name, serviceName: picked.name, fingerprint: picked.fingerprint)
        } else {
            let parts = typed.split(separator: ":")
            let port = parts.count == 2 ? UInt16(parts[1]) ?? RemoteHost.defaultPort : RemoteHost.defaultPort
            target = PairingTarget(name: String(parts.first ?? ""), address: String(parts.first ?? ""), port: port)
        }
        let code = normalizedCode
        phase = .pairing
        Task {
            do {
                let device = (Host.current().localizedName ?? "Mac") + " (Graviton)"
                let result = try await Pairing.pair(target, code: code, deviceName: device)
                // With a link the fingerprint was pinned already; otherwise ask.
                if target.fingerprint != nil { save(result) } else { phase = .confirm(result) }
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }

    private func save(_ result: PairingResult) {
        do {
            let trimmed = name.trimmingCharacters(in: .whitespaces)
            let mac = try hosts.add(result, name: trimmed.isEmpty ? nil : trimmed)
            onPaired(mac)
            dismiss()
        } catch {
            phase = .failed("Couldn't save the pairing: \(error.localizedDescription)")
        }
    }
}

/// Edit fallback addresses (e.g. LAN + Tailscale), port, and Bonjour service name for a paired Mac.
struct EditHostAddressesSheet: View {
    @Environment(SharedHosts.self) private var hosts
    @Environment(\.dismiss) private var dismiss
    var mac: PairedMac
    var onSaved: (PairedMac) -> Void

    @State private var addressesText: String
    @State private var portText: String
    @State private var serviceName: String
    @State private var problem: String?

    init(mac: PairedMac, onSaved: @escaping (PairedMac) -> Void) {
        self.mac = mac
        self.onSaved = onSaved
        _addressesText = State(initialValue: mac.allAddresses.joined(separator: "\n"))
        _portText = State(initialValue: String(mac.port))
        _serviceName = State(initialValue: mac.serviceName ?? mac.hostName)
    }

    private var parsedAddresses: [String] {
        RemoteHost.normalize(addressesText.split(whereSeparator: { $0.isNewline || $0 == "," }).map(String.init))
    }

    private var validPort: UInt16? {
        UInt16(portText.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    var body: some View {
        GSheet("Host routing for \(mac.name)",
               subtitle: "Graviton races all addresses alongside Bonjour (_graviton._tcp) and connects over whichever verifies your pinned certificate first.",
               symbol: "network") {
            GFormRow("Addresses", hint: "One IP or hostname per line (e.g. LAN IP + Tailscale IP).") {
                TextEditor(text: $addressesText)
                    .scrollContentBackground(.hidden)
                    .frame(height: 82)
                    .gravitonField(mono: true, height: nil)
            }
            HStack(alignment: .top, spacing: 12) {
                GFormRow("Bonjour service", hint: "Matched automatically via certificate fingerprint.") {
                    GTextField(mac.hostName, text: $serviceName)
                        .gravitonField(mono: true)
                }
                GFormRow("Port") {
                    GTextField("7843", text: $portText)
                        .gravitonField(mono: true)
                }
                .frame(width: 110)
            }
            if let problem {
                GStatusLine(.failure, problem)
            }
        } footer: {
            Spacer()
            Button("Cancel") { dismiss() }.buttonStyle(.gSecondary).keyboardShortcut(.cancelAction)
            Button("Save", action: save)
                .buttonStyle(.gPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(validPort == nil || (parsedAddresses.isEmpty && serviceName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty))
        }
    }

    private func save() {
        guard let port = validPort else {
            problem = "Enter a valid TCP port (1–65535)."
            return
        }
        do {
            let updated = try hosts.update(mac.name, addresses: parsedAddresses, port: port, serviceName: serviceName)
            onSaved(updated)
            dismiss()
        } catch {
            problem = error.localizedDescription
        }
    }
}
