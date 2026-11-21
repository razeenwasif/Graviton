import GravitonKit
import SwiftUI
import UIKit

// The phone's own chrome: a tab bar, screen headers and sheets in Graviton's
// design, in place of UIKit's bars.

// MARK: - Tab bar

enum AppTab: Int, CaseIterable, Identifiable {
    case containers, images, activity, hosts
    var id: Int { rawValue }

    var title: String {
        switch self {
        case .containers: "Containers"
        case .images: "Images"
        case .activity: "Activity"
        case .hosts: "Hosts"
        }
    }

    var symbol: String {
        switch self {
        case .containers: "shippingbox"
        case .images: "square.stack.3d.up"
        case .activity: "waveform.path.ecg"
        case .hosts: "desktopcomputer"
        }
    }

    /// Filled where SF Symbols has a filled version.
    var selectedSymbol: String {
        switch self {
        case .containers: "shippingbox.fill"
        case .images: "square.stack.3d.up.fill"
        case .activity, .hosts: symbol
        }
    }
}

struct GravitonTabBar: View {
    /// Above the home indicator. Screens keep this much clear at the bottom (`mobileScreen()`).
    static let height: CGFloat = 58
    @Binding var selection: AppTab
    var badge: (AppTab) -> Int? = { _ in nil }
    @Namespace private var glow

    var body: some View {
        HStack(spacing: 0) {
            ForEach(AppTab.allCases) { tab in
                let selected = selection == tab
                Button {
                    if selection != tab {
                        UISelectionFeedbackGenerator().selectionChanged()
                        withAnimation(.snappy(duration: 0.25)) { selection = tab }
                    }
                } label: {
                    VStack(spacing: 4) {
                        ZStack {
                            if selected {
                                Capsule()
                                    .fill(G.accent.opacity(0.16))
                                    .frame(width: 52, height: 30)
                                    .matchedGeometryEffect(id: "glow", in: glow)
                            }
                            Image(systemName: selected ? tab.selectedSymbol : tab.symbol)
                                .font(.system(size: 17, weight: selected ? .semibold : .regular))
                                .foregroundStyle(selected ? G.accent : G.muted)
                                .frame(height: 30)
                            if let n = badge(tab), n > 0 {
                                Text("\(n)")
                                    .font(.system(size: 10, weight: .bold))
                                    .foregroundStyle(G.onAccent)
                                    .padding(.horizontal, 5)
                                    .frame(minWidth: 17, minHeight: 17)
                                    .background(G.danger, in: Capsule())
                                    .offset(x: 16, y: -9)
                            }
                        }
                        Text(tab.title)
                            .font(.system(size: 10.5, weight: selected ? .semibold : .medium))
                            .foregroundStyle(selected ? G.text : G.faint)
                    }
                    .frame(maxWidth: .infinity)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(tab.title)
                .accessibilityAddTraits(selected ? .isSelected : [])
            }
        }
        .padding(.top, 6)
        .padding(.bottom, 4)
        .frame(height: Self.height, alignment: .top)
        .background {
            Rectangle()
                .fill(G.sidebar)
                .overlay(alignment: .top) { Rectangle().fill(G.borderStrong).frame(height: 0.5) }
                .ignoresSafeArea(edges: .bottom)
        }
    }
}

// MARK: - Headers

/// The top of a screen: a large title on a tab's first screen, or a back
/// button and a smaller title on the screens pushed after it.
struct MobileHeader<Actions: View>: View {
    var title: String
    var subtitle: String?
    var large: Bool
    @ViewBuilder var actions: Actions
    @Environment(\.dismiss) private var dismiss

    init(_ title: String, subtitle: String? = nil, large: Bool = false, @ViewBuilder actions: () -> Actions = { EmptyView() }) {
        self.title = title
        self.subtitle = subtitle
        self.large = large
        self.actions = actions()
    }

    var body: some View {
        HStack(alignment: large ? .lastTextBaseline : .center, spacing: 12) {
            if !large {
                Button { dismiss() } label: { Image(systemName: "chevron.left") }
                    .buttonStyle(CircleIconStyle())
                    .accessibilityLabel("Back")
            }
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                    .font(.system(size: large ? 30 : 17, weight: large ? .bold : .semibold))
                    .kerning(large ? -0.8 : -0.2)
                    .foregroundStyle(G.text)
                    .lineLimit(1)
                if let subtitle {
                    Text(subtitle).font(.system(size: large ? 13 : 12)).foregroundStyle(G.muted).lineLimit(1)
                }
            }
            Spacer(minLength: 8)
            HStack(spacing: 8) { actions }
        }
        .padding(.horizontal, 20)
        .padding(.top, large ? 10 : 6)
        .padding(.bottom, large ? 12 : 10)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A round icon button for headers.
struct CircleIconStyle: ButtonStyle {
    var prominent = false
    @Environment(\.isEnabled) private var enabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .labelStyle(.iconOnly)
            .font(.system(size: 15, weight: .semibold))
            .foregroundStyle(prominent ? G.onAccent : G.text)
            .frame(width: 38, height: 38)
            .background(prominent ? G.accent : G.control, in: Circle())
            .overlay(Circle().strokeBorder(prominent ? .clear : G.borderStrong))
            .scaleEffect(configuration.isPressed ? 0.92 : 1)
            .opacity(enabled ? 1 : 0.4)
            .animation(.snappy(duration: 0.15), value: configuration.isPressed)
            .contentShape(Circle())
    }
}

/// A search field that sits under a header.
struct MobileSearchField: View {
    var prompt: String
    @Binding var text: String

    var body: some View {
        HStack(spacing: 0) {
            GTextField(prompt, text: $text)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .submitLabel(.search)
            if !text.isEmpty {
                Button { text = "" } label: { Image(systemName: "xmark.circle.fill") }
                    .buttonStyle(.plain)
                    .foregroundStyle(G.faint)
                    .accessibilityLabel("Clear")
            }
        }
        .gravitonField(height: 40, icon: "magnifyingglass", size: 15.5)
    }
}

extension View {
    /// A screen in Graviton's chrome: no navigation bar, the app's background.
    /// `pushed`: a screen opened inside a tab. Those don't get the space the tab bar
    /// reserves (it's outside their navigation stack), so they keep it clear themselves.
    func mobileScreen(pushed: Bool = true) -> some View {
        toolbar(.hidden, for: .navigationBar)
            .safeAreaPadding(.bottom, pushed ? GravitonTabBar.height : 0)
            .background(G.background.ignoresSafeArea())
    }

    /// A rounded card on the phone.
    func mobileCard(_ fill: Color = G.card, radius: CGFloat = 16) -> some View {
        background(fill, in: RoundedRectangle(cornerRadius: radius, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: radius, style: .continuous).strokeBorder(G.border))
    }
}

/// A hairline between rows in a card.
struct RowDivider: View {
    var inset: CGFloat = 16
    var body: some View { Rectangle().fill(G.border).frame(height: 1).padding(.leading, inset) }
}

/// Hiding the navigation bar also turns off swipe-to-go-back; this keeps it.
extension UINavigationController: @retroactive UIGestureRecognizerDelegate {
    override open func viewDidLoad() {
        super.viewDidLoad()
        interactivePopGestureRecognizer?.delegate = self
    }

    public func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        viewControllers.count > 1
    }
}

// MARK: - Sheets

/// The frame every phone sheet shares: Cancel, a title, the main action, then the content.
struct MobileSheet<Content: View>: View {
    var title: String
    var subtitle: String?
    var cancelTitle: String
    var actionTitle: String?
    var actionEnabled: Bool
    var action: () -> Void
    @ViewBuilder var content: Content
    @Environment(\.dismiss) private var dismiss

    init(_ title: String, subtitle: String? = nil, cancelTitle: String = "Cancel", actionTitle: String? = nil, actionEnabled: Bool = true,
         action: @escaping () -> Void = {}, @ViewBuilder content: () -> Content) {
        self.title = title
        self.subtitle = subtitle
        self.cancelTitle = cancelTitle
        self.actionTitle = actionTitle
        self.actionEnabled = actionEnabled
        self.action = action
        self.content = content()
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Button(cancelTitle) { dismiss() }
                    .buttonStyle(.plain)
                    .font(.system(size: 15, weight: .medium))
                    .foregroundStyle(G.textSecondary)
                    .frame(minWidth: 70, alignment: .leading)
                Spacer()
                Text(title).font(.system(size: 16, weight: .semibold)).foregroundStyle(G.text)
                Spacer()
                Group {
                    if let actionTitle {
                        Button(actionTitle, action: action)
                            .buttonStyle(.g(.primary, size: .small))
                            .disabled(!actionEnabled)
                    } else {
                        Color.clear.frame(width: 1, height: 1)
                    }
                }
                .frame(minWidth: 70, alignment: .trailing)
            }
            .padding(.horizontal, 20)
            .padding(.top, 18)
            .padding(.bottom, 14)

            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    if let subtitle {
                        Text(subtitle).font(.system(size: 13.5)).foregroundStyle(G.muted).fixedSize(horizontal: false, vertical: true)
                    }
                    content
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)
            }
            .scrollDismissesKeyboard(.interactively)
        }
        .background(G.background.ignoresSafeArea())
        .presentationBackground(G.background)
        .presentationCornerRadius(26)
        .presentationDragIndicator(.visible)
        .preferredColorScheme(.dark)
        .tint(G.accent)
    }
}

/// A phone-sized text field with Graviton's chrome.
struct MobileField: View {
    var placeholder: String
    @Binding var text: String
    var mono = true
    var keyboard: UIKeyboardType = .asciiCapable

    var body: some View {
        GTextField(placeholder, text: $text)
            .keyboardType(keyboard)
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled()
            .gravitonField(mono: mono, height: 44, size: 15.5)
    }
}
