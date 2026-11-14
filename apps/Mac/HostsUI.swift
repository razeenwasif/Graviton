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
    @State private var forgetting: PairedMac?

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
                Button("Forget \(current.name)…", systemImage: "trash") { forgetting = current }
            }
        } label: {
            HStack(spacing: 6) {
                Image(systemName: hosts.selected == nil ? "laptopcomputer" : "desktopcomputer").foregroundStyle(G.accent)
                Text(hosts.current?.name ?? "This Mac").fontWeight(.medium).lineLimit(1)
                Image(systemName: "chevron.up.chevron.down").font(.system(size: 9, weight: .semibold)).foregroundStyle(G.faint)
            }
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("Choose the Mac this window shows")
        .onAppear(perform: hosts.reload)
        .confirmationDialog("Forget \(forgetting?.name ?? "")?", isPresented: Binding(get: { forgetting != nil }, set: { if !$0 { forgetting = nil } })) {
            Button("Forget", role: .destructive) {
                if let mac = forgetting { try? hosts.forget(mac.name) }
                switchTo(nil)
            }
        } message: {
            Text("This Mac and grav stop using it. On \(forgetting?.hostName ?? "that Mac"), `grav devices revoke` also cancels its token.")
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
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Pair a Mac").font(.system(size: 16, weight: .semibold))
                Text("On the other Mac, run `grav service install --remote` once, then `grav pair` for a code.")
                    .font(.system(size: 12.5)).foregroundStyle(G.muted)
            }
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 10) {
                GridRow {
                    label("Address")
                    field("mini.local, 192.168.1.20, or the graviton://pair link", text: $address)
                }
                if link == nil {
                    GridRow {
                        label("Code")
                        field("XXXX-XXXX", text: $code)
                    }
                }
                GridRow {
                    label("Name")
                    field("Optional, e.g. mini (used with grav --host)", text: $name)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            Group {
                switch phase {
                case .idle:
                    if link != nil {
                        Label("A pairing link: its code and certificate fingerprint are filled in.", systemImage: "link").foregroundStyle(G.muted)
                    }
                case .pairing:
                    HStack(spacing: 8) { ProgressView().controlSize(.small); Text("Pairing…") }.foregroundStyle(G.textSecondary)
                case let .confirm(result):
                    VStack(alignment: .leading, spacing: 6) {
                        Label("Paired with \(result.host.name)", systemImage: "checkmark.circle.fill").foregroundStyle(G.running)
                        Text("Check this matches the fingerprint `grav pair` showed on it:").foregroundStyle(G.muted)
                        Text(result.host.shortFingerprint).font(G.mono(16, weight: .semibold))
                    }
                case let .failed(message):
                    Label(message, systemImage: "xmark.octagon.fill").foregroundStyle(G.danger).textSelection(.enabled)
                }
            }
            .font(.system(size: 12.5))
            .fixedSize(horizontal: false, vertical: true)

            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                if case let .confirm(result) = phase {
                    Button("It matches") { save(result) }
                        .buttonStyle(.borderedProminent)
                        .keyboardShortcut(.defaultAction)
                } else {
                    Button("Pair", action: pair)
                        .buttonStyle(.borderedProminent)
                        .keyboardShortcut(.defaultAction)
                        .disabled(busy || address.trimmingCharacters(in: .whitespaces).isEmpty || normalizedCode.count != 8)
                }
            }
        }
        .padding(22)
        .frame(width: 540)
        .background(Color(hex: 0x15161A))
    }

    private func label(_ text: String) -> some View {
        Text(text).font(.system(size: 12, weight: .medium)).foregroundStyle(G.textSecondary).gridColumnAlignment(.trailing)
    }

    private func field(_ placeholder: String, text: Binding<String>) -> some View {
        TextField(placeholder, text: text)
            .textFieldStyle(.plain)
            .font(G.mono(12.5))
            .padding(.horizontal, 10)
            .frame(maxWidth: .infinity, minHeight: 30, maxHeight: 30)
            .background(G.background, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .disabled(busy)
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
