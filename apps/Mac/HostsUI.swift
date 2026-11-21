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
                    Button {
                        switchTo(mac)
                    } label: {
                        Label("\(mac.name)  ·  \(mac.address)", systemImage: hosts.selected == mac.name ? "checkmark" : "desktopcomputer")
                    }
                }
            }
            Divider()
            Button("Pair a Mac…", systemImage: "plus") { router.pending = .pairMac }
            if let current = hosts.current {
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

    enum Phase {
        case idle, pairing
        case confirm(PairingResult)
        case failed(String)
    }

    private var busy: Bool { if case .pairing = phase { true } else { false } }
    private var link: PairingTarget? { PairingTarget(link: address) }
    private var normalizedCode: String { (link?.code ?? code).uppercased().filter { $0.isLetter || $0.isNumber } }

    var body: some View {
        GSheet("Pair a Mac", subtitle: "On the other Mac, run grav service install --remote once, then grav pair for a code.", symbol: "desktopcomputer.and.arrow.down") {
            GFormRow("Address", hint: link != nil ? "A pairing link: its code and certificate fingerprint are filled in." : nil) {
                field("mini.local, 192.168.1.20, or the graviton://pair link", text: $address)
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
    }

    private func field(_ placeholder: String, text: Binding<String>) -> some View {
        GTextField(placeholder, text: text)
            .disabled(busy)
            .gravitonField(mono: true)
    }

    private func pair() {
        let target: PairingTarget
        if let link {
            target = link
        } else {
            let typed = address.trimmingCharacters(in: .whitespaces)
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
