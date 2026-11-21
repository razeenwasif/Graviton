import AppKit
import GravitonKit
import SwiftUI

/// The top of every screen, in place of the window's title bar: a title, a
/// subtitle, and the screen's actions. Dragging it moves the window and a
/// double-click zooms it, as a title bar would.
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
                    .foregroundStyle(G.muted)
                Image(systemName: "chevron.right").font(.system(size: 10, weight: .semibold)).foregroundStyle(G.faint)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.system(size: 19, weight: .semibold))
                    .kerning(-0.4)
                    .foregroundStyle(G.text)
                    .lineLimit(1)
                if let subtitle {
                    Text(subtitle).font(.system(size: 12)).foregroundStyle(G.muted).lineLimit(1)
                }
            }
            Spacer(minLength: 16)
            HStack(spacing: 8) { actions }
        }
        .padding(.horizontal, 28)
        .padding(.top, 18)
        .padding(.bottom, 14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(G.background)
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

/// A search field for a screen header.
struct HeaderSearch: View {
    var prompt: String
    @Binding var text: String

    var body: some View {
        HStack(spacing: 0) {
            GTextField(prompt, text: $text)
            if !text.isEmpty {
                Button { text = "" } label: { Image(systemName: "xmark.circle.fill") }
                    .buttonStyle(.plain)
                    .foregroundStyle(G.faint)
                    .help("Clear")
            }
        }
        .gravitonField(height: 32, icon: "magnifyingglass")
        .frame(width: 220)
    }
}
