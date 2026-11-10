import GravitonKit
import Observation
import SwiftTerm
import SwiftUI
#if os(macOS)
import AppKit
typealias PlatformColor = NSColor
typealias PlatformFont = NSFont
#else
import UIKit
typealias PlatformColor = UIColor
typealias PlatformFont = UIFont
#endif

/// A shell in a container, drawn by SwiftTerm. It outlives the view showing
/// it, so switching tabs (or leaving the screen) doesn't end the shell.
@MainActor
@Observable
final class ShellSession: NSObject {
    enum Phase: Equatable {
        case connecting
        case running
        case exited(Int32)
        case failed(String)
    }

    let containerID: String
    let command: [String]
    private(set) var phase: Phase = .connecting
    /// The window's title as set by the program (`\e]0;…\a`), if any.
    private(set) var title: String?

    @ObservationIgnored let view: TerminalView
    @ObservationIgnored private var session: (any TerminalSession)?
    @ObservationIgnored private var pump: Task<Void, Never>?
    @ObservationIgnored private var size: (rows: UInt16, cols: UInt16)?

    init(containerID: String, command: [String] = ShellSession.defaultCommand) {
        self.containerID = containerID
        self.command = command
        view = TerminalView(frame: CGRect(x: 0, y: 0, width: 640, height: 400))
        super.init()
        view.terminalDelegate = self
        style(view)
    }

    /// bash when the image has it, else sh.
    static let defaultCommand = ["/bin/sh", "-c", "if command -v bash >/dev/null 2>&1; then exec bash -l; else exec sh -l; fi"]

    var isLive: Bool { phase == .connecting || phase == .running }

    func start(using store: EngineStore) {
        close()
        phase = .connecting
        title = nil
        view.getTerminal().resetToInitialState()
        let terminal = view.getTerminal()
        let rows = UInt16(clamping: terminal.rows), cols = UInt16(clamping: terminal.cols)
        let spec = ExecSpec(id: containerID, command: command, rows: max(rows, 2), cols: max(cols, 10))
        pump = Task { [weak self] in
            do {
                let session = try await store.openTerminal(spec)
                guard let self, !Task.isCancelled else { session.close(); return }
                self.session = session
                self.phase = .running
                // The view may have been laid out while connecting.
                if let size = self.size, size != (spec.rows, spec.cols) { session.resize(rows: size.rows, cols: size.cols) }
                for await event in session.events {
                    switch event {
                    case let .output(bytes):
                        self.view.feed(byteArray: bytes[...])
                    case let .exited(code):
                        self.phase = .exited(code)
                        self.note("Process exited with code \(code)")
                    case let .failed(message):
                        self.phase = .failed(message)
                        self.note(message)
                    }
                }
                if self.phase == .running { self.phase = .failed("The connection closed.") }
            } catch {
                guard let self, !Task.isCancelled else { return }
                self.phase = .failed(error.localizedDescription)
                self.note(error.localizedDescription)
            }
        }
    }

    func close() {
        pump?.cancel()
        pump = nil
        session?.close()
        session = nil
    }

    /// A dim line from Graviton itself, after whatever the program drew.
    private func note(_ text: String) {
        view.feed(text: "\r\n\u{1b}[2m[\(text)]\u{1b}[0m\r\n")
    }

    private func style(_ view: TerminalView) {
        #if os(macOS)
        view.font = NSFont.monospacedSystemFont(ofSize: 12.5, weight: .regular)
        view.optionAsMetaKey = true
        #else
        view.font = UIFont.monospacedSystemFont(ofSize: 11.5, weight: .regular)
        #endif
        view.nativeBackgroundColor = PlatformColor(hex: 0x08090B)
        view.nativeForegroundColor = PlatformColor(hex: 0xECEDEF)
        view.caretColor = PlatformColor(hex: 0x7C8CFF)
        view.installColors(Self.palette)
    }

    /// ANSI colours tuned for the near-black console (stock blue is unreadable on it).
    static let palette: [SwiftTerm.Color] = [
        0x1A1C21, 0xFF7A7A, 0x3DD68C, 0xF5B547, 0x7C8CFF, 0xC792EA, 0x5CC8D8, 0xC9CDD4,
        0x5A606B, 0xFF9E9E, 0x6BE3A8, 0xFFCB6B, 0xA3AEFF, 0xDDB3F5, 0x8BDDE8, 0xF4F5F7,
    ].map { hex in
        let c = { (shift: UInt32) in UInt16((hex >> shift) & 0xff) * 257 }
        return SwiftTerm.Color(red: c(16), green: c(8), blue: c(0))
    }
}

extension ShellSession: @preconcurrency TerminalViewDelegate {
    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        session?.send(Array(data))
    }

    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        let size = (rows: UInt16(clamping: newRows), cols: UInt16(clamping: newCols))
        self.size = size
        session?.resize(rows: size.rows, cols: size.cols)
    }

    func setTerminalTitle(source: TerminalView, title: String) {
        self.title = title.isEmpty ? nil : title
    }

    /// OSC 52: a program in the container copies text to this device's clipboard.
    func clipboardCopy(source: TerminalView, content: Data) {
        guard let text = String(data: content, encoding: .utf8) else { return }
        #if os(macOS)
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        #else
        UIPasteboard.general.string = text
        #endif
    }

    /// A link the user clicked in the output (OSC 8 or a detected URL): web links only.
    func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
        guard let url = URL(string: link), ["http", "https"].contains(url.scheme?.lowercased() ?? "") else { return }
        #if os(macOS)
        NSWorkspace.shared.open(url)
        #else
        UIApplication.shared.open(url)
        #endif
    }

    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    func scrolled(source: TerminalView, position: Double) {}
    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
}

/// Shells by container, kept while the app runs.
@MainActor
@Observable
final class ShellSessions {
    private var sessions: [String: ShellSession] = [:]

    /// The container's shell, started if there isn't a live one.
    func shell(for containerID: String, store: EngineStore) -> ShellSession {
        if let existing = sessions[containerID] { return existing }
        let session = ShellSession(containerID: containerID)
        sessions[containerID] = session
        session.start(using: store)
        return session
    }

    func existing(_ containerID: String) -> ShellSession? { sessions[containerID] }

    /// Drop every shell (the engine changed: another Mac, or demo data).
    func reset() {
        sessions.values.forEach { $0.close() }
        sessions = [:]
    }
}

/// Shows a session's terminal view.
#if os(macOS)
struct TerminalHost: NSViewRepresentable {
    let session: ShellSession

    func makeNSView(context: Context) -> NSView {
        let container = NSView()
        attach(to: container)
        return container
    }

    func updateNSView(_ container: NSView, context: Context) {
        if session.view.superview !== container { attach(to: container) }
    }

    private func attach(to container: NSView) {
        let view = session.view
        view.removeFromSuperview()
        view.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(view)
        NSLayoutConstraint.activate([
            view.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            view.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            view.topAnchor.constraint(equalTo: container.topAnchor),
            view.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        ])
        DispatchQueue.main.async { view.window?.makeFirstResponder(view) }
    }
}
#else
struct TerminalHost: UIViewRepresentable {
    let session: ShellSession

    func makeUIView(context: Context) -> UIView {
        let container = UIView()
        attach(to: container)
        return container
    }

    func updateUIView(_ container: UIView, context: Context) {
        if session.view.superview !== container { attach(to: container) }
    }

    private func attach(to container: UIView) {
        let view = session.view
        view.removeFromSuperview()
        view.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(view)
        NSLayoutConstraint.activate([
            view.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            view.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            view.topAnchor.constraint(equalTo: container.topAnchor),
            view.bottomAnchor.constraint(equalTo: container.keyboardLayoutGuide.topAnchor),
        ])
        DispatchQueue.main.async { _ = view.becomeFirstResponder() }
    }
}
#endif

extension PlatformColor {
    convenience init(hex: UInt32) {
        self.init(red: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255, blue: CGFloat(hex & 0xff) / 255, alpha: 1)
    }
}
