import SwiftUI

// Graviton's own controls, drawn from the design tokens in `G` instead of the
// system's. One accent, hairline borders, no materials.

// MARK: - Buttons

public enum GButtonKind: Sendable {
    /// The one main action on a screen or sheet.
    case primary
    /// Everything else.
    case secondary
    /// No chrome until hovered: toolbars, rows.
    case ghost
    /// Something that destroys or stops.
    case danger
    /// The confirm button of a destructive dialog.
    case dangerSolid
}

public enum GButtonSize: Sendable {
    case small, regular, large

    var height: CGFloat { switch self { case .small: 26; case .regular: 32; case .large: 38 } }
    var font: CGFloat { switch self { case .small: 12; case .regular: 13; case .large: 13.5 } }
    var padding: CGFloat { switch self { case .small: 10; case .regular: 13; case .large: 16 } }
}

public struct GButtonStyle: ButtonStyle {
    public var kind: GButtonKind
    public var size: GButtonSize
    public var fullWidth: Bool

    public init(_ kind: GButtonKind = .secondary, size: GButtonSize = .regular, fullWidth: Bool = false) {
        self.kind = kind
        self.size = size
        self.fullWidth = fullWidth
    }

    public func makeBody(configuration: Configuration) -> some View {
        GButtonBody(configuration: configuration, kind: kind, size: size, fullWidth: fullWidth)
    }
}

private struct GButtonBody: View {
    let configuration: ButtonStyleConfiguration
    let kind: GButtonKind
    let size: GButtonSize
    let fullWidth: Bool
    @Environment(\.isEnabled) private var enabled
    @State private var hovering = false

    var body: some View {
        let pressed = configuration.isPressed
        configuration.label
            .labelStyle(GLabelStyle())
            .font(.system(size: size.font, weight: kind == .primary || kind == .dangerSolid ? .semibold : .medium))
            .foregroundStyle(foreground)
            .lineLimit(1)
            .padding(.horizontal, size.padding)
            .frame(maxWidth: fullWidth ? .infinity : nil, minHeight: size.height)
            .background(background(pressed: pressed), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(border))
            .overlay(alignment: .top) {
                // A faint top highlight gives solid buttons some depth.
                if kind == .primary || kind == .dangerSolid {
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .strokeBorder(LinearGradient(colors: [.white.opacity(0.28), .clear], startPoint: .top, endPoint: .center), lineWidth: 1)
                }
            }
            .scaleEffect(pressed && enabled ? 0.98 : 1)
            .opacity(enabled ? 1 : 0.45)
            .contentShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
            .onHover { hovering = $0 }
            .animation(.easeOut(duration: 0.12), value: hovering)
            .animation(.easeOut(duration: 0.08), value: pressed)
    }

    private var active: Bool { hovering && enabled }

    private var foreground: Color {
        switch kind {
        case .primary, .dangerSolid: G.onAccent
        case .secondary: G.text
        case .ghost: active ? G.text : G.textSecondary
        case .danger: G.danger
        }
    }

    private func background(pressed: Bool) -> Color {
        switch kind {
        case .primary: pressed ? G.accent.opacity(0.8) : (active ? Color(hex: 0x8F9CFF) : G.accent)
        case .dangerSolid: pressed ? G.danger.opacity(0.8) : (active ? Color(hex: 0xFF9C9C) : G.danger)
        case .secondary: pressed ? G.raised : (active ? Color(hex: 0x20232A) : G.control)
        case .ghost: pressed ? Color.white.opacity(0.08) : (active ? Color.white.opacity(0.05) : .clear)
        case .danger: pressed ? G.danger.opacity(0.18) : (active ? G.danger.opacity(0.12) : G.control)
        }
    }

    private var border: Color {
        switch kind {
        case .primary, .dangerSolid, .ghost: .clear
        case .secondary: active ? G.borderStrong.opacity(1.6) : G.borderStrong
        case .danger: G.danger.opacity(active ? 0.35 : 0.18)
        }
    }
}

/// Square button for a single symbol.
public struct GIconButtonStyle: ButtonStyle {
    public var size: CGFloat
    public var bordered: Bool

    public init(size: CGFloat = 28, bordered: Bool = false) {
        self.size = size
        self.bordered = bordered
    }

    public func makeBody(configuration: Configuration) -> some View {
        GIconButtonBody(configuration: configuration, size: size, bordered: bordered)
    }
}

private struct GIconButtonBody: View {
    let configuration: ButtonStyleConfiguration
    let size: CGFloat
    let bordered: Bool
    @Environment(\.isEnabled) private var enabled
    @State private var hovering = false

    var body: some View {
        configuration.label
            .labelStyle(.iconOnly)
            .font(.system(size: size * 0.43, weight: .semibold))
            .foregroundStyle(hovering && enabled ? G.text : G.muted)
            .frame(width: size, height: size)
            .background(fill, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 7, style: .continuous).strokeBorder(bordered ? G.borderStrong : .clear))
            .opacity(enabled ? 1 : 0.4)
            .contentShape(Rectangle())
            .onHover { hovering = $0 }
            .animation(.easeOut(duration: 0.12), value: hovering)
    }

    private var fill: Color {
        if configuration.isPressed { return Color.white.opacity(0.1) }
        if hovering && enabled { return Color.white.opacity(0.06) }
        return bordered ? G.control : .clear
    }
}

public struct GLabelStyle: LabelStyle {
    public init() {}
    public func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 6) {
            configuration.icon.font(.system(size: 11.5, weight: .semibold))
            configuration.title
        }
    }
}

public extension ButtonStyle where Self == GButtonStyle {
    static var gPrimary: GButtonStyle { GButtonStyle(.primary) }
    static var gSecondary: GButtonStyle { GButtonStyle(.secondary) }
    static var gGhost: GButtonStyle { GButtonStyle(.ghost) }
    static var gDanger: GButtonStyle { GButtonStyle(.danger) }
    static func g(_ kind: GButtonKind, size: GButtonSize = .regular, fullWidth: Bool = false) -> GButtonStyle {
        GButtonStyle(kind, size: size, fullWidth: fullWidth)
    }
}

public extension ButtonStyle where Self == GIconButtonStyle {
    static var gIcon: GIconButtonStyle { GIconButtonStyle() }
    static var gIconBordered: GIconButtonStyle { GIconButtonStyle(size: 32, bordered: true) }
}

// MARK: - Toggles

public struct GSwitchStyle: ToggleStyle {
    public init() {}

    public func makeBody(configuration: Configuration) -> some View {
        GSwitchBody(configuration: configuration)
    }
}

private struct GSwitchBody: View {
    let configuration: ToggleStyleConfiguration
    @Environment(\.isEnabled) private var enabled

    var body: some View {
        Button {
            withAnimation(.snappy(duration: 0.18)) { configuration.isOn.toggle() }
        } label: {
            HStack(spacing: 8) {
                configuration.label
                    .font(.system(size: 12.5, weight: .medium))
                    .foregroundStyle(G.textSecondary)
                ZStack(alignment: configuration.isOn ? .trailing : .leading) {
                    Capsule()
                        .fill(configuration.isOn ? G.accent : G.control)
                        .overlay(Capsule().strokeBorder(configuration.isOn ? .clear : G.borderStrong))
                    Circle()
                        .fill(configuration.isOn ? G.onAccent : G.muted)
                        .padding(3)
                }
                .frame(width: 30, height: 18)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .opacity(enabled ? 1 : 0.45)
        .accessibilityAddTraits(configuration.isOn ? .isSelected : [])
    }
}

public struct GCheckboxStyle: ToggleStyle {
    public init() {}

    public func makeBody(configuration: Configuration) -> some View {
        GCheckboxBody(configuration: configuration)
    }
}

private struct GCheckboxBody: View {
    let configuration: ToggleStyleConfiguration
    @Environment(\.isEnabled) private var enabled
    @State private var hovering = false

    var body: some View {
        Button {
            withAnimation(.snappy(duration: 0.15)) { configuration.isOn.toggle() }
        } label: {
            HStack(spacing: 9) {
                ZStack {
                    RoundedRectangle(cornerRadius: 5, style: .continuous)
                        .fill(configuration.isOn ? G.accent : G.background)
                    RoundedRectangle(cornerRadius: 5, style: .continuous)
                        .strokeBorder(configuration.isOn ? .clear : (hovering ? G.muted : G.faint.opacity(0.7)), lineWidth: 1.2)
                    if configuration.isOn {
                        Image(systemName: "checkmark")
                            .font(.system(size: 9.5, weight: .heavy))
                            .foregroundStyle(G.onAccent)
                            .transition(.scale(scale: 0.5).combined(with: .opacity))
                    }
                }
                .frame(width: 16, height: 16)
                configuration.label
                    .font(.system(size: 13))
                    .foregroundStyle(G.textSecondary)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .opacity(enabled ? 1 : 0.45)
        .onHover { hovering = $0 }
        .accessibilityAddTraits(configuration.isOn ? .isSelected : [])
    }
}

public extension ToggleStyle where Self == GSwitchStyle {
    static var gSwitch: GSwitchStyle { GSwitchStyle() }
}

public extension ToggleStyle where Self == GCheckboxStyle {
    static var gCheckbox: GCheckboxStyle { GCheckboxStyle() }
}

// MARK: - Text fields

public struct GFieldModifier: ViewModifier {
    var mono: Bool
    var height: CGFloat?
    var invalid: Bool
    var icon: String?
    @FocusState private var focused: Bool
    @State private var hovering = false
    @Environment(\.isEnabled) private var enabled

    public func body(content: Content) -> some View {
        HStack(spacing: 8) {
            if let icon {
                Image(systemName: icon).font(.system(size: 12, weight: .medium)).foregroundStyle(focused ? G.accent : G.faint)
            }
            content
                .textFieldStyle(.plain)
                .font(mono ? G.mono(12.5) : .system(size: 13))
                .foregroundStyle(G.text)
                .focused($focused)
        }
        .padding(.horizontal, 11)
        .padding(.vertical, height == nil ? 8 : 0)
        .frame(maxWidth: .infinity, minHeight: height, maxHeight: height)
        .background(G.background, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(stroke, lineWidth: focused ? 1.4 : 1))
        .background {
            if focused {
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill((invalid ? G.danger : G.accent).opacity(0.14))
                    .padding(-3)
            }
        }
        .opacity(enabled ? 1 : 0.55)
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: focused)
    }

    private var stroke: Color {
        if invalid { return G.danger.opacity(0.85) }
        if focused { return G.accent.opacity(0.9) }
        return hovering ? Color.white.opacity(0.14) : G.borderStrong
    }
}

public extension View {
    /// Graviton's field chrome for a `TextField` (or `TextEditor`, with `height: nil`).
    func gravitonField(mono: Bool = false, height: CGFloat? = 32, invalid: Bool = false, icon: String? = nil) -> some View {
        modifier(GFieldModifier(mono: mono, height: height, invalid: invalid, icon: icon))
    }
}

/// A `TextField` whose placeholder is drawn in Graviton's faint grey.
public func GTextField(_ placeholder: String, text: Binding<String>) -> TextField<Text> {
    TextField("", text: text, prompt: Text(placeholder).foregroundStyle(G.faint))
}

/// A label above a field, with an optional hint or error below.
public struct GFormRow<Content: View>: View {
    var label: String
    var hint: String?
    var error: String?
    @ViewBuilder var content: Content

    public init(_ label: String, hint: String? = nil, error: String? = nil, @ViewBuilder content: () -> Content) {
        self.label = label
        self.hint = hint
        self.error = error
        self.content = content()
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label.uppercased())
                .font(.system(size: 10.5, weight: .semibold))
                .kerning(0.6)
                .foregroundStyle(G.faint)
            content
            if let error {
                Text(error).font(.system(size: 11.5)).foregroundStyle(G.danger)
            } else if let hint {
                Text(hint).font(.system(size: 11.5)).foregroundStyle(G.faint)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

// MARK: - Segmented control

public struct GSegmented<Value: Hashable>: View {
    @Binding var selection: Value
    var options: [(value: Value, label: String)]
    @Namespace private var pill

    public init(selection: Binding<Value>, options: [(value: Value, label: String)]) {
        _selection = selection
        self.options = options
    }

    public var body: some View {
        HStack(spacing: 2) {
            ForEach(Array(options.enumerated()), id: \.offset) { _, option in
                let selected = option.value == selection
                Button {
                    withAnimation(.snappy(duration: 0.2)) { selection = option.value }
                } label: {
                    Text(option.label)
                        .font(.system(size: 12, weight: selected ? .semibold : .medium))
                        .foregroundStyle(selected ? G.text : G.muted)
                        .padding(.horizontal, 11)
                        .frame(height: 24)
                        .background {
                            if selected {
                                RoundedRectangle(cornerRadius: 6, style: .continuous)
                                    .fill(Color(hex: 0x262932))
                                    .overlay(RoundedRectangle(cornerRadius: 6, style: .continuous).strokeBorder(Color.white.opacity(0.07)))
                                    .shadow(color: .black.opacity(0.35), radius: 2, y: 1)
                                    .matchedGeometryEffect(id: "pill", in: pill)
                            }
                        }
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityAddTraits(selected ? .isSelected : [])
            }
        }
        .padding(3)
        .background(G.control, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 9, style: .continuous).strokeBorder(G.border))
        .fixedSize()
    }
}

// MARK: - Stepper

public struct GStepper: View {
    @Binding var value: Int
    var range: ClosedRange<Int>
    var format: (Int) -> String

    public init(value: Binding<Int>, in range: ClosedRange<Int>, format: @escaping (Int) -> String = { "\($0)" }) {
        _value = value
        self.range = range
        self.format = format
    }

    public var body: some View {
        HStack(spacing: 0) {
            step("minus", enabled: value > range.lowerBound) { value -= 1 }
            Text(format(value))
                .font(.system(size: 12.5, weight: .medium))
                .monospacedDigit()
                .foregroundStyle(G.text)
                .frame(minWidth: 64)
                .contentTransition(.numericText(value: Double(value)))
            step("plus", enabled: value < range.upperBound) { value += 1 }
        }
        .frame(height: 30)
        .background(G.background, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(G.borderStrong))
        .fixedSize()
    }

    private func step(_ symbol: String, enabled: Bool, action: @escaping () -> Void) -> some View {
        Button {
            withAnimation(.snappy(duration: 0.15)) { action() }
        } label: {
            Image(systemName: symbol)
        }
        .buttonStyle(GIconButtonStyle(size: 28))
        .disabled(!enabled)
    }
}

// MARK: - Dropdown trigger

/// The face of a dropdown: the current value and a chevron, in field chrome.
/// Wrap it in a `Menu` with `.menuStyle(.button)` and `.buttonStyle(.plain)`.
public struct GDropdownLabel: View {
    var text: String
    var icon: String?
    var mono: Bool
    @State private var hovering = false

    public init(_ text: String, icon: String? = nil, mono: Bool = false) {
        self.text = text
        self.icon = icon
        self.mono = mono
    }

    public var body: some View {
        HStack(spacing: 8) {
            if let icon { Image(systemName: icon).font(.system(size: 12)).foregroundStyle(G.accent) }
            Text(text).font(mono ? G.mono(12.5) : .system(size: 12.5, weight: .medium)).foregroundStyle(G.text).lineLimit(1)
            Spacer(minLength: 4)
            Image(systemName: "chevron.up.chevron.down").font(.system(size: 9, weight: .bold)).foregroundStyle(G.faint)
        }
        .padding(.horizontal, 11)
        .frame(height: 30)
        .background(hovering ? Color(hex: 0x111216) : G.background, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(hovering ? Color.white.opacity(0.14) : G.borderStrong))
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
    }
}

public extension View {
    /// A `Menu` that draws its label exactly as given.
    func gravitonMenu() -> some View {
        menuStyle(.button).buttonStyle(.plain).menuIndicator(.hidden).fixedSize()
    }
}

// MARK: - Progress

/// A spinning arc, in place of the system's spinner.
public struct GSpinner: View {
    var size: CGFloat
    var color: Color

    public init(size: CGFloat = 16, color: Color = G.accent) {
        self.size = size
        self.color = color
    }

    public var body: some View {
        TimelineView(.animation) { context in
            let t = context.date.timeIntervalSinceReferenceDate
            ZStack {
                Circle().stroke(color.opacity(0.18), lineWidth: max(1.5, size / 9))
                Circle()
                    .trim(from: 0, to: 0.28)
                    .stroke(color, style: StrokeStyle(lineWidth: max(1.5, size / 9), lineCap: .round))
                    .rotationEffect(.degrees(t.truncatingRemainder(dividingBy: 0.9) / 0.9 * 360))
            }
        }
        .frame(width: size, height: size)
        .accessibilityLabel("Loading")
    }
}

/// A status line: a spinner, a check, or a cross, then the message.
public struct GStatusLine: View {
    public enum Tone: Sendable { case working, success, failure, warning, info }
    var tone: Tone
    var text: String

    public init(_ tone: Tone, _ text: String) {
        self.tone = tone
        self.text = text
    }

    public var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 9) {
            Group {
                switch tone {
                case .working: GSpinner(size: 12)
                case .success: badge("checkmark", G.running)
                case .failure: badge("xmark", G.danger)
                case .warning: badge("exclamationmark", G.warning)
                case .info: badge("info", G.accent)
                }
            }
            .alignmentGuide(.firstTextBaseline) { $0[VerticalAlignment.center] + 4 }
            Text(text)
                .font(.system(size: 12.5))
                .foregroundStyle(color)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(tint.opacity(0.08), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(tint.opacity(0.18)))
    }

    private var tint: Color {
        switch tone {
        case .working, .info: G.accent
        case .success: G.running
        case .failure: G.danger
        case .warning: G.warning
        }
    }

    private var color: Color {
        switch tone {
        case .working, .info: G.textSecondary
        case .success: G.running
        case .failure: G.danger
        case .warning: G.warning
        }
    }

    private func badge(_ symbol: String, _ color: Color) -> some View {
        Image(systemName: symbol)
            .font(.system(size: 7.5, weight: .heavy))
            .foregroundStyle(G.onAccent)
            .frame(width: 13, height: 13)
            .background(color, in: Circle())
    }
}

// MARK: - Empty states

public struct GEmptyState: View {
    var title: String
    var symbol: String
    var detail: String?
    var actionTitle: String?
    var action: (() -> Void)?

    public init(_ title: String, symbol: String, detail: String? = nil, actionTitle: String? = nil, action: (() -> Void)? = nil) {
        self.title = title
        self.symbol = symbol
        self.detail = detail
        self.actionTitle = actionTitle
        self.action = action
    }

    public var body: some View {
        VStack(spacing: 14) {
            ZStack {
                Circle().fill(G.accent.opacity(0.07)).frame(width: 84, height: 84)
                RoundedRectangle(cornerRadius: 14, style: .continuous)
                    .fill(LinearGradient(colors: [Color(hex: 0x1D2029), G.card], startPoint: .top, endPoint: .bottom))
                    .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).strokeBorder(G.borderStrong))
                    .frame(width: 52, height: 52)
                Image(systemName: symbol).font(.system(size: 21, weight: .medium)).foregroundStyle(G.accent)
            }
            VStack(spacing: 5) {
                Text(title).font(.system(size: 15, weight: .semibold)).foregroundStyle(G.text)
                if let detail {
                    Text(detail)
                        .font(.system(size: 12.5))
                        .foregroundStyle(G.muted)
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 340)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            if let actionTitle, let action {
                Button(actionTitle, action: action).buttonStyle(.gPrimary).padding(.top, 4)
            }
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

// MARK: - Badges

public struct GBadge: View {
    var text: String
    var color: Color

    public init(_ text: String, color: Color = G.muted) {
        self.text = text
        self.color = color
    }

    public var body: some View {
        Text(text)
            .font(.system(size: 10.5, weight: .semibold))
            .monospacedDigit()
            .foregroundStyle(color)
            .padding(.horizontal, 6)
            .frame(minWidth: 18, minHeight: 17)
            .background(color.opacity(0.13), in: Capsule())
    }
}

/// Uppercase section caption.
public struct GCaption: View {
    var text: String
    public init(_ text: String) { self.text = text }
    public var body: some View {
        Text(text.uppercased())
            .font(.system(size: 10.5, weight: .semibold))
            .kerning(0.7)
            .foregroundStyle(G.faint)
    }
}

// MARK: - Dialogs

/// In-window dialogs (confirmations and alerts), in place of the system's.
/// Put `.gravitonDialogs(_:)` on the window's root view and call `confirm` or `alert`.
@MainActor @Observable
public final class Dialogs {
    public struct Request: Identifiable {
        public let id = UUID()
        public var title: String
        public var message: String
        public var symbol: String
        public var destructive: Bool
        public var confirmTitle: String?
        public var cancelTitle: String
        public var action: (() -> Void)?
    }

    public var current: Request?

    public init() {}

    public func confirm(_ title: String, message: String, confirm: String, destructive: Bool = true, symbol: String? = nil, action: @escaping () -> Void) {
        current = Request(title: title, message: message, symbol: symbol ?? (destructive ? "trash" : "questionmark"),
                          destructive: destructive, confirmTitle: confirm, cancelTitle: "Cancel", action: action)
    }

    public func alert(_ title: String, message: String, symbol: String = "exclamationmark.triangle") {
        current = Request(title: title, message: message, symbol: symbol, destructive: false, confirmTitle: nil, cancelTitle: "OK", action: nil)
    }

    public func dismiss() { current = nil }
}

private struct DialogHost: ViewModifier {
    @Bindable var dialogs: Dialogs

    func body(content: Content) -> some View {
        content.overlay {
            ZStack {
                if let request = dialogs.current {
                    Color.black.opacity(0.5)
                        .ignoresSafeArea()
                        .onTapGesture { dialogs.dismiss() }
                        .transition(.opacity)
                    DialogCard(request: request, dialogs: dialogs)
                        .transition(.scale(scale: 0.94).combined(with: .opacity))
                }
            }
            .animation(.snappy(duration: 0.2), value: dialogs.current?.id)
        }
    }
}

private struct DialogCard: View {
    let request: Dialogs.Request
    let dialogs: Dialogs

    var body: some View {
        let tint = request.destructive ? G.danger : (request.action == nil ? G.warning : G.accent)
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 14) {
                Image(systemName: request.symbol)
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(tint)
                    .frame(width: 36, height: 36)
                    .background(tint.opacity(0.13), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                VStack(alignment: .leading, spacing: 5) {
                    Text(request.title).font(.system(size: 14.5, weight: .semibold)).foregroundStyle(G.text)
                    Text(request.message)
                        .font(.system(size: 12.5))
                        .foregroundStyle(G.muted)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            HStack(spacing: 8) {
                Spacer()
                if let confirm = request.confirmTitle {
                    Button(request.cancelTitle) { dialogs.dismiss() }
                        .buttonStyle(.gSecondary)
                        .keyboardShortcut(.cancelAction)
                    Button(confirm) {
                        let action = request.action
                        dialogs.dismiss()
                        action?()
                    }
                    .buttonStyle(.g(request.destructive ? .dangerSolid : .primary))
                    .keyboardShortcut(.defaultAction)
                } else {
                    Button(request.cancelTitle) { dialogs.dismiss() }
                        .buttonStyle(.gPrimary)
                        .keyboardShortcut(.defaultAction)
                }
            }
        }
        .padding(20)
        .frame(width: 400)
        .background(Color(hex: 0x17181D), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).strokeBorder(G.borderStrong))
        .shadow(color: .black.opacity(0.5), radius: 30, y: 12)
    }
}

public extension View {
    func gravitonDialogs(_ dialogs: Dialogs) -> some View {
        modifier(DialogHost(dialogs: dialogs))
    }
}

// MARK: - Sheets

/// The frame every sheet shares: a title, a subtitle, the content, and a footer of buttons.
public struct GSheet<Content: View, Footer: View>: View {
    var title: String
    var subtitle: String?
    var symbol: String
    var width: CGFloat
    @ViewBuilder var content: Content
    @ViewBuilder var footer: Footer

    public init(_ title: String, subtitle: String? = nil, symbol: String, width: CGFloat = 540,
                @ViewBuilder content: () -> Content, @ViewBuilder footer: () -> Footer) {
        self.title = title
        self.subtitle = subtitle
        self.symbol = symbol
        self.width = width
        self.content = content()
        self.footer = footer()
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top, spacing: 14) {
                Image(systemName: symbol)
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(G.accent)
                    .frame(width: 36, height: 36)
                    .background(G.accent.opacity(0.12), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(G.accent.opacity(0.18)))
                VStack(alignment: .leading, spacing: 3) {
                    Text(title).font(.system(size: 16, weight: .semibold)).kerning(-0.2).foregroundStyle(G.text)
                    if let subtitle {
                        Text(subtitle).font(.system(size: 12.5)).foregroundStyle(G.muted).fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            .padding(.horizontal, 22)
            .padding(.top, 22)
            .padding(.bottom, 18)

            VStack(alignment: .leading, spacing: 16) { content }
                .padding(.horizontal, 22)
                .padding(.bottom, 20)

            HStack(spacing: 8) { footer }
                .padding(.horizontal, 22)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity)
                .background(Color(hex: 0x111216))
                .overlay(alignment: .top) { Rectangle().fill(G.border).frame(height: 1) }
        }
        .frame(width: width)
        .background(Color(hex: 0x16171C))
        .preferredColorScheme(.dark)
        .tint(G.accent)
    }
}

// MARK: - Tables

/// A sortable column heading for Graviton's own tables.
public struct GColumnHeader<Key: Hashable>: View {
    var title: String
    var key: Key?
    @Binding var sort: Key
    @Binding var ascending: Bool
    var alignment: Alignment

    public init(_ title: String, key: Key?, sort: Binding<Key>, ascending: Binding<Bool>, alignment: Alignment = .leading) {
        self.title = title
        self.key = key
        _sort = sort
        _ascending = ascending
        self.alignment = alignment
    }

    public var body: some View {
        let active = key != nil && key == sort
        Button {
            guard let key else { return }
            if sort == key { ascending.toggle() } else { sort = key; ascending = true }
        } label: {
            HStack(spacing: 4) {
                Text(title.uppercased())
                    .font(.system(size: 10.5, weight: .semibold))
                    .kerning(0.6)
                if active {
                    Image(systemName: ascending ? "arrow.up" : "arrow.down").font(.system(size: 8.5, weight: .bold))
                }
            }
            .foregroundStyle(active ? G.textSecondary : G.faint)
            .frame(maxWidth: .infinity, alignment: alignment)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(key == nil)
    }
}

/// Row background for Graviton's tables: hover and selection.
public struct GRowBackground: ViewModifier {
    var selected: Bool
    @State private var hovering = false

    public func body(content: Content) -> some View {
        content
            .background {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(selected ? G.accent.opacity(0.11) : (hovering ? Color.white.opacity(0.035) : .clear))
                    .overlay(alignment: .leading) {
                        if selected {
                            Capsule().fill(G.accent).frame(width: 3).padding(.vertical, 9)
                        }
                    }
            }
            .contentShape(Rectangle())
            .onHover { hovering = $0 }
    }
}

public extension View {
    func gravitonRow(selected: Bool) -> some View { modifier(GRowBackground(selected: selected)) }
}
