import AppKit
import GravitonKit
import SwiftUI

/// Configures the NSWindow so pitch-black sidebars and canvases extend cleanly
/// beneath transparent macOS titlebars and traffic lights, matching Stratus.
struct WindowAccessor: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async { [weak view] in
            guard let window = view?.window else { return }
            configure(window)
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        DispatchQueue.main.async { [weak nsView] in
            guard let window = nsView?.window else { return }
            configure(window)
        }
    }

    @MainActor
    private func configure(_ window: NSWindow) {
        window.backgroundColor = .black
        window.appearance = NSAppearance(named: .darkAqua)
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.styleMask.insert(.fullSizeContentView)
        window.isMovableByWindowBackground = true
    }
}

/// Top header of every screen: Stratus page title, muted subtitle, and right-aligned actions.
struct ScreenHeader<Actions: View>: View {
    var title: String
    var subtitle: String?
    var back: (title: String, action: () -> Void)?
    @ViewBuilder var actions: Actions

    init(_ title: String, subtitle: String? = nil, back: (title: String, action: () -> Void)? = nil, @ViewBuilder actions: () -> Actions = { EmptyView() }) {
        self.title = title
        self.subtitle = subtitle
        self.back = back
        self.actions = actions()
    }

    var body: some View {
        HStack(alignment: .center, spacing: 14) {
            if let back {
                Button(action: back.action) {
                    Image(systemName: "chevron.left")
                }
                .buttonStyle(GIconButtonStyle(size: 30, bordered: true))
                .keyboardShortcut("[", modifiers: .command)
                .help("Back to \(back.title) (⌘[)")
                Button(back.title, action: back.action)
                    .buttonStyle(.plain)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(G.textSecondary)
                Image(systemName: "chevron.right").font(.system(size: 10, weight: .semibold)).foregroundStyle(G.faint)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(G.pageTitle())
                    .kerning(-0.4)
                    .foregroundStyle(G.textPrimary)
                    .lineLimit(1)
                if let subtitle {
                    Text(subtitle)
                        .font(.system(size: 12))
                        .foregroundStyle(G.textMuted)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 16)
            HStack(spacing: 8) { actions }
        }
        .padding(.horizontal, 28)
        .padding(.top, 20)
        .padding(.bottom, 14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(G.bg)
        .windowDraggable()
    }
}

extension View {
    /// Dragging moves the window; a double-click zooms it, like a title bar.
    func windowDraggable() -> some View {
        contentShape(Rectangle())
            .gesture(WindowDragGesture())
            .simultaneousGesture(TapGesture(count: 2).onEnded { NSApp.keyWindow?.zoom(nil) })
            .allowsWindowActivationEvents(true)
    }
}

/// Borderless search field for a screen header.
struct HeaderSearch: View {
    var prompt: String
    @Binding var text: String

    var body: some View {
        GBorderlessSearchField(prompt, text: $text)
            .frame(width: 224)
    }
}
