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
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .background(G.console)
        .navigationTitle(container.name)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 1) {
                    Text(container.name).font(.system(size: 15, weight: .semibold))
                    if let shell { Text(status(shell)).font(.system(size: 11)).foregroundStyle(G.muted) }
                }
            }
            if let shell, !shell.isLive {
                ToolbarItem(placement: .primaryAction) {
                    Button("New shell") { shell.start(using: store) }.disabled(!container.isRunning)
                }
            }
        }
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
