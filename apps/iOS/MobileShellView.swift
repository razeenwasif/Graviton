import GravitonKit
import SwiftUI

/// A shell in a container, full screen, with SwiftTerm's key bar (esc, ctrl, tab, arrows).
struct MobileShellView: View {
    @Environment(EngineStore.self) private var store
    @Environment(ShellSessions.self) private var shells
    var container: Container
    @State private var shell: ShellSession?

    var body: some View {
        Group {
            if let shell {
                TerminalHost(session: shell)
                    .padding(.horizontal, 6)
                    .background(G.console)
            } else {
                GSpinner(size: 22).frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .background(G.console.ignoresSafeArea())
        .safeAreaInset(edge: .top, spacing: 0) {
            MobileHeader(container.name, subtitle: shell.map(status)) {
                if let shell, !shell.isLive {
                    Button("New shell") { shell.start(using: store) }
                        .buttonStyle(.g(.primary, size: .small))
                        .disabled(!container.isRunning)
                }
            }
            .background(G.background)
        }
        .toolbar(.hidden, for: .navigationBar)
        .safeAreaPadding(.bottom, GravitonTabBar.height)
        .task(id: container.id) { shell = shells.shell(for: container.id, store: store) }
    }

    private func status(_ shell: ShellSession) -> String {
        switch shell.phase {
        case .connecting: "Connecting…"
        case .running: store.liveName.map { "Shell · on \($0)" } ?? "Shell"
        case let .exited(code): "Exited (\(code))"
        case let .failed(why): why
        }
    }
}
