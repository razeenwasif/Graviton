import SwiftUI

// MARK: - Status

public struct StatusDot: View {
    public var status: ContainerStatus
    public var size: CGFloat
    public var pulse: Bool

    public init(running: Bool, size: CGFloat = 8, pulse: Bool = false) {
        self.status = running ? .running : .exited
        self.size = size
        self.pulse = pulse
    }

    public init(status: ContainerStatus, size: CGFloat = 8, pulse: Bool = false) {
        self.status = status
        self.size = size
        self.pulse = pulse
    }

    private var tint: Color {
        switch status {
        case .running: G.running
        case .paused: G.warning
        case .exited, .created: G.stopped
        }
    }

    private var label: String {
        switch status {
        case .running: "Running"
        case .paused: "Paused"
        case .exited, .created: "Stopped"
        }
    }

    public var body: some View {
        ZStack {
            if status == .running || status == .paused {
                Circle()
                    .fill(tint.opacity(pulse ? 0.24 : 0.18))
                    .frame(width: size + 7, height: size + 7)
            }
            Circle()
                .fill(tint)
                .frame(width: size, height: size)
        }
        .accessibilityLabel(label)
    }
}

public struct HealthBadge: View {
    public var status: HealthStatus

    public init(_ status: HealthStatus) {
        self.status = status
    }

    private var tint: Color {
        switch status {
        case .healthy: G.running
        case .starting: G.warning
        case .unhealthy: G.danger
        }
    }

    public var body: some View {
        HStack(spacing: 5) {
            Circle().fill(tint).frame(width: 5.5, height: 5.5)
            Text(status.label)
        }
        .font(.system(size: 11, weight: .medium))
        .foregroundStyle(tint)
        .padding(.horizontal, 8)
        .frame(height: 20)
        .background(tint.opacity(0.12), in: Capsule())
    }
}

public struct StatusPill: View {
    public var container: Container
    public var showUptime: Bool

    public init(_ container: Container, showUptime: Bool = false) {
        self.container = container
        self.showUptime = showUptime
    }

    public var body: some View {
        let tint: Color = switch (container.status, container.health) {
        case (.running, .unhealthy): G.danger
        case (.running, .starting): G.warning
        case (.running, _): G.running
        case (.paused, _): G.warning
        case (.exited, _), (.created, _): G.textSecondary
        }
        let label: String = switch container.status {
        case .running:
            showUptime
                ? "Running · \(container.statusText())\(container.health.map { " · \($0.label)" } ?? "")"
                : "Running\(container.health.map { " · \($0.label)" } ?? "")"
        case .paused:
            "Paused"
        case .exited, .created:
            container.statusText()
        }
        HStack(spacing: 6) {
            Circle().fill(tint).frame(width: 6, height: 6)
            Text(label)
        }
        .font(.system(size: 11.5, weight: .medium))
        .foregroundStyle(tint)
        .padding(.horizontal, 9)
        .frame(height: 22)
        .background(tint.opacity(0.12), in: Capsule())
    }
}

// MARK: - Stratus KPI & Tag Primitives

public struct GMetricStatColumn: View {
    public var label: String
    public var value: String
    public var subtitle: String?
    public var valueColor: Color

    public init(label: String, value: String, subtitle: String? = nil, valueColor: Color = G.textPrimary) {
        self.label = label
        self.value = value
        self.subtitle = subtitle
        self.valueColor = valueColor
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label.uppercased())
                .font(.system(size: 10.5, weight: .semibold))
                .tracking(0.65)
                .foregroundStyle(G.textMuted)
            Text(value)
                .font(G.heroKPI(26))
                .foregroundStyle(valueColor)
                .contentTransition(.numericText())
            if let subtitle {
                Text(subtitle)
                    .font(.system(size: 11.5))
                    .foregroundStyle(G.textTertiary)
                    .lineLimit(1)
            }
        }
    }
}

public struct GTagPill: View {
    public var text: String
    public var color: Color
    public var icon: String?

    public init(_ text: String, color: Color = G.textSecondary, icon: String? = nil) {
        self.text = text
        self.color = color
        self.icon = icon
    }

    public var body: some View {
        HStack(spacing: 4) {
            if let icon {
                Image(systemName: icon).font(.system(size: 9.5, weight: .semibold))
            }
            Text(text)
                .font(.system(size: 11, weight: .medium))
                .lineLimit(1)
        }
        .foregroundStyle(color)
        .padding(.horizontal, 8)
        .padding(.vertical, 3.5)
        .background(color.opacity(0.12), in: Capsule())
    }
}

public struct GActionPillButton: View {
    public var title: String
    public var icon: String?
    public var color: Color
    public var action: () -> Void
    @State private var hovering = false

    public init(_ title: String, icon: String? = nil, color: Color = G.textPrimary, action: @escaping () -> Void) {
        self.title = title
        self.icon = icon
        self.color = color
        self.action = action
    }

    public var body: some View {
        Button(action: action) {
            HStack(spacing: 5) {
                if let icon {
                    Image(systemName: icon).font(.system(size: 10.5, weight: .semibold))
                }
                Text(title).font(.system(size: 11.5, weight: .medium))
            }
            .foregroundStyle(color)
            .padding(.horizontal, 10)
            .frame(height: 26)
            .background(hovering ? G.controlHover : G.surfacePill, in: Capsule())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hovering)
    }
}

public struct GFilterPillButton: View {
    public var title: String
    public var count: Int?
    public var selected: Bool
    public var accent: Color
    public var action: () -> Void
    @State private var hovering = false

    public init(_ title: String, count: Int? = nil, selected: Bool, accent: Color = G.indigo, action: @escaping () -> Void) {
        self.title = title
        self.count = count
        self.selected = selected
        self.accent = accent
        self.action = action
    }

    public var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Text(title)
                    .font(.system(size: 12, weight: selected ? .semibold : .medium))
                if let count {
                    Text("\(count)")
                        .font(G.mono(10.5, weight: .semibold))
                        .foregroundStyle(selected ? accent : G.textMuted)
                }
            }
            .foregroundStyle(selected ? G.textPrimary : (hovering ? G.body : G.textSecondary))
            .padding(.horizontal, 11)
            .frame(height: 27)
            .background(
                selected ? accent.opacity(0.15) : (hovering ? G.surfaceHover : G.surfaceSubtle),
                in: Capsule()
            )
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hovering)
    }
}

public struct GBorderlessSearchField: View {
    public var placeholder: String
    @Binding public var text: String
    @FocusState private var focused: Bool

    public init(_ placeholder: String = "Filter…", text: Binding<String>) {
        self.placeholder = placeholder
        _text = text
    }

    public var body: some View {
        HStack(spacing: 7) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(focused ? G.indigo : G.textMuted)
            TextField("", text: $text, prompt: Text(placeholder).foregroundStyle(G.textTertiary))
                .textFieldStyle(.plain)
                .font(.system(size: 12.5))
                .foregroundStyle(G.textPrimary)
                .focused($focused)
            if !text.isEmpty {
                Button {
                    text = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 11.5))
                        .foregroundStyle(G.textMuted)
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.horizontal, 10)
        .frame(height: 30)
        .background(focused ? G.surfaceHover : G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .animation(.easeOut(duration: 0.12), value: focused)
    }
}

public struct GBarSegment: Identifiable, Sendable {
    public let id = UUID()
    public var label: String
    public var value: Double
    public var detail: String?
    public var color: Color

    public init(label: String, value: Double, detail: String? = nil, color: Color) {
        self.label = label
        self.value = value
        self.detail = detail
        self.color = color
    }
}

public struct GSegmentedBar: View {
    public var segments: [GBarSegment]
    public var height: CGFloat

    public init(segments: [GBarSegment], height: CGFloat = 7) {
        self.segments = segments
        self.height = height
    }

    public var body: some View {
        let active = segments.filter { $0.value > 0 }
        let total = max(active.reduce(0) { $0 + $1.value }, 0.001)
        VStack(alignment: .leading, spacing: 10) {
            GeometryReader { geo in
                let gap: CGFloat = active.count > 1 ? 2 : 0
                let available = max(0, geo.size.width - gap * CGFloat(max(0, active.count - 1)))
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.white.opacity(0.05))
                    if !active.isEmpty, available > 0 {
                        HStack(spacing: gap) {
                            ForEach(active) { seg in
                                Rectangle()
                                    .fill(seg.color)
                                    .frame(width: max(3, available * (seg.value / total)))
                            }
                        }
                        .frame(width: geo.size.width, alignment: .leading)
                        .clipShape(Capsule())
                    }
                }
            }
            .frame(height: height)

            GFlowWrapLayout(spacing: 14, lineSpacing: 6) {
                ForEach(active) { seg in
                    HStack(spacing: 6) {
                        Circle().fill(seg.color).frame(width: 7, height: 7)
                        Text(seg.label)
                            .font(.system(size: 11.5, weight: .medium))
                            .foregroundStyle(G.textSecondary)
                        if let detail = seg.detail {
                            Text(detail)
                                .font(G.mono(11))
                                .foregroundStyle(G.textMuted)
                        }
                    }
                }
            }
        }
    }
}

public struct GFlowWrapLayout: Layout {
    public var spacing: CGFloat
    public var lineSpacing: CGFloat

    public init(spacing: CGFloat = 8, lineSpacing: CGFloat = 6) {
        self.spacing = spacing
        self.lineSpacing = lineSpacing
    }

    public func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let maxWidth = proposal.width ?? .infinity
        var x: CGFloat = 0
        var y: CGFloat = 0
        var lineHeight: CGFloat = 0
        var maxUsed: CGFloat = 0

        for sub in subviews {
            let size = sub.sizeThatFits(.unspecified)
            if x > 0 && x + size.width > maxWidth {
                x = 0
                y += lineHeight + lineSpacing
                lineHeight = 0
            }
            maxUsed = max(maxUsed, x + size.width)
            x += size.width + spacing
            lineHeight = max(lineHeight, size.height)
        }
        return CGSize(width: maxWidth.isFinite ? maxWidth : maxUsed, height: y + lineHeight)
    }

    public func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var x = bounds.minX
        var y = bounds.minY
        var lineHeight: CGFloat = 0

        for sub in subviews {
            let size = sub.sizeThatFits(.unspecified)
            if x > bounds.minX && x + size.width > bounds.maxX {
                x = bounds.minX
                y += lineHeight + lineSpacing
                lineHeight = 0
            }
            sub.place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(size))
            x += size.width + spacing
            lineHeight = max(lineHeight, size.height)
        }
    }
}

public struct GKeyValueCard<Content: View>: View {
    public var title: String
    @ViewBuilder public var content: Content

    public init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(title.uppercased())
                .font(.system(size: 10.5, weight: .semibold))
                .tracking(0.65)
                .foregroundStyle(G.textMuted)
            VStack(alignment: .leading, spacing: 8) {
                content
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(G.surfaceSubtle, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        }
    }
}

public struct GKeyValueRow: View {
    public var key: String
    public var value: String
    public var mono: Bool

    public init(_ key: String, _ value: String, mono: Bool = true) {
        self.key = key
        self.value = value
        self.mono = mono
    }

    public var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(key)
                .font(.system(size: 12))
                .foregroundStyle(G.textMuted)
            Spacer(minLength: 8)
            Text(value)
                .font(mono ? G.mono(12) : .system(size: 12, weight: .medium))
                .foregroundStyle(G.body)
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)
        }
    }
}

public struct GToastBanner: View {
    public var message: String
    public var isError: Bool
    public var onDismiss: () -> Void

    public init(message: String, isError: Bool = false, onDismiss: @escaping () -> Void) {
        self.message = message
        self.isError = isError
        self.onDismiss = onDismiss
    }

    public var body: some View {
        HStack(spacing: 10) {
            Image(systemName: isError ? "exclamationmark.triangle.fill" : "checkmark.circle.fill")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(isError ? G.rose : G.mint)
            Text(message)
                .font(.system(size: 12.5, weight: .medium))
                .foregroundStyle(G.textPrimary)
                .lineLimit(2)
            Button(action: onDismiss) {
                Image(systemName: "xmark")
                    .font(.system(size: 10, weight: .bold))
                    .foregroundStyle(G.textMuted)
                    .frame(width: 20, height: 20)
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .background(G.surfacePill, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .shadow(color: .black.opacity(0.6), radius: 18, y: 6)
    }
}

// MARK: - Charts

/// A minimal line (and optional area) chart scaled to its own min/max.
public struct Sparkline: View {
    public var values: [Double]
    public var color: Color
    public var filled: Bool
    public var lineWidth: CGFloat

    public init(_ values: [Double], color: Color = G.accent, filled: Bool = false, lineWidth: CGFloat = 1.8) {
        self.values = values
        self.color = color
        self.filled = filled
        self.lineWidth = lineWidth
    }

    public var body: some View {
        GeometryReader { geo in
            let points = Self.points(values, in: geo.size)
            ZStack {
                if filled, points.count > 1 {
                    Path { p in
                        p.move(to: CGPoint(x: points[0].x, y: geo.size.height))
                        points.forEach { p.addLine(to: $0) }
                        p.addLine(to: CGPoint(x: points.last!.x, y: geo.size.height))
                        p.closeSubpath()
                    }
                    .fill(LinearGradient(colors: [color.opacity(0.22), color.opacity(0.01)], startPoint: .top, endPoint: .bottom))
                }
                Path { p in
                    guard let first = points.first else { return }
                    p.move(to: first)
                    points.dropFirst().forEach { p.addLine(to: $0) }
                }
                .stroke(color, style: StrokeStyle(lineWidth: lineWidth, lineCap: .round, lineJoin: .round))
            }
        }
        .accessibilityHidden(true)
    }

    static func points(_ values: [Double], in size: CGSize) -> [CGPoint] {
        guard values.count > 1 else { return [] }
        let lo = values.min()!, hi = values.max()!
        let span = max(hi - lo, max(abs(hi) * 0.1, 0.001))
        let inset: CGFloat = 3
        return values.enumerated().map { i, v in
            CGPoint(x: size.width * CGFloat(i) / CGFloat(values.count - 1),
                    y: inset + (size.height - inset * 2) * (1 - CGFloat((v - lo) / span)))
        }
    }
}

public struct MeterBar: View {
    public var fraction: Double
    public var color: Color
    public var height: CGFloat

    public init(_ fraction: Double, color: Color = G.accent, height: CGFloat = 4) {
        self.fraction = fraction
        self.color = color
        self.height = height
    }

    public var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule().fill(Color.white.opacity(0.06))
                Capsule().fill(color).frame(width: geo.size.width * min(max(fraction, 0), 1))
            }
        }
        .frame(height: height)
        .accessibilityHidden(true)
    }
}

public struct StackedMeterBar: View {
    public struct Segment: Sendable {
        public var fraction: Double
        public var color: Color

        public init(_ fraction: Double, color: Color) {
            self.fraction = fraction
            self.color = color
        }
    }

    public var segments: [Segment]
    public var height: CGFloat

    public init(_ segments: [Segment], height: CGFloat = 5) {
        self.segments = segments
        self.height = height
    }

    public var body: some View {
        GeometryReader { geo in
            let active = segments.filter { $0.fraction > 0 }
            let rawSum = active.reduce(0) { $0 + $1.fraction }
            let scale = rawSum > 1 ? 1.0 / rawSum : 1.0
            let gap: CGFloat = active.count > 1 ? 1.5 : 0
            let available = max(0, geo.size.width - gap * CGFloat(max(0, active.count - 1)))

            ZStack(alignment: .leading) {
                Capsule().fill(Color.white.opacity(0.06))
                if !active.isEmpty, available > 0 {
                    HStack(spacing: gap) {
                        ForEach(Array(active.enumerated()), id: \.offset) { _, seg in
                            Rectangle()
                                .fill(seg.color)
                                .frame(width: min(available, max(2, available * seg.fraction * scale)))
                        }
                    }
                    .frame(width: geo.size.width, alignment: .leading)
                    .clipShape(Capsule())
                }
            }
        }
        .frame(height: height)
        .accessibilityHidden(true)
    }
}

/// "12.4 %" with the unit dimmed.
public struct MetricValue: View {
    public var value: String
    public var unit: String
    public var size: CGFloat

    public init(_ value: String, unit: String, size: CGFloat = 20) {
        self.value = value
        self.unit = unit
        self.size = size
    }

    public var body: some View {
        (Text(value).font(.system(size: size, weight: .semibold).monospacedDigit())
            + Text(unit).font(.system(size: 13, weight: .medium)).foregroundStyle(G.muted))
            .kerning(-0.3)
            .foregroundStyle(G.text)
    }
}

// MARK: - Logs

public struct LogRow: View {
    public var line: LogLine
    public var showSequence: Bool
    public var showTimestamp: Bool
    public var compact: Bool

    public init(_ line: LogLine, showSequence: Bool = true, showTimestamp: Bool = true, compact: Bool = false) {
        self.line = line
        self.showSequence = showSequence
        self.showTimestamp = showTimestamp
        self.compact = compact
    }

    private var highlight: Color {
        switch line.level {
        case .error: G.rose.opacity(0.08)
        case .warn: G.amber.opacity(0.05)
        default: .clear
        }
    }

    public var body: some View {
        Group {
            if compact {
                VStack(alignment: .leading, spacing: 1) {
                    HStack(spacing: 8) {
                        if showTimestamp {
                            Text(Format.clock(line.date)).foregroundStyle(G.faint)
                        }
                        Text(line.level.label).foregroundStyle(G.color(for: line.level))
                    }
                    Text(line.message).foregroundStyle(G.body)
                }
                .font(G.mono(11.5))
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 14)
                .padding(.vertical, 4)
            } else {
                HStack(alignment: .firstTextBaseline, spacing: 16) {
                    if showSequence {
                        Text(String(line.seq)).foregroundStyle(G.textTertiary).frame(minWidth: 34, alignment: .trailing)
                    }
                    if showTimestamp {
                        Text(Format.clock(line.date)).foregroundStyle(G.faint)
                    }
                    Text(line.level.label).foregroundStyle(G.color(for: line.level)).frame(width: 44, alignment: .leading)
                    Text(line.message).foregroundStyle(G.body).textSelection(.enabled)
                    Spacer(minLength: 0)
                }
                .font(G.mono(12.5))
                .padding(.horizontal, 18)
                .padding(.vertical, 2.5)
            }
        }
        .background(highlight)
    }
}

// MARK: - Brand mark

/// The Graviton mark: sculpted container cube inside crossed spin-2 graviton orbits with a luminous core particle.
/// Matches `design/icon.svg` and `design/icon-macos.svg`.
public struct GravitonMark: View {
    public var size: CGFloat
    public var background: Bool

    public init(size: CGFloat = 26, background: Bool = true) {
        self.size = size
        self.background = background
    }

    public var body: some View {
        Canvas { ctx, canvas in
            let s = canvas.width / 1024
            func pt(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: x * s, y: y * s) }
            func poly(_ pts: [(CGFloat, CGFloat)]) -> Path {
                Path { p in
                    p.addLines(pts.map { pt($0.0, $0.1) })
                    p.closeSubpath()
                }
            }
            if background {
                let plate = Path(roundedRect: CGRect(origin: .zero, size: canvas), cornerRadius: 230 * s, style: .continuous)
                ctx.fill(plate, with: .color(Color(hex: 0x05060C)))
                ctx.fill(
                    Path(ellipseIn: CGRect(x: 80 * s, y: 80 * s, width: 864 * s, height: 864 * s)),
                    with: .radialGradient(
                        Gradient(colors: [G.indigo.opacity(0.20), .clear]),
                        center: pt(460, 420),
                        startRadius: 0,
                        endRadius: 430 * s
                    )
                )
            }

            let center = pt(512, 516)
            let tilt1 = CGAffineTransform(translationX: center.x, y: center.y)
                .rotated(by: -20 * .pi / 180)
                .translatedBy(x: -center.x, y: -center.y)
            let tilt2 = CGAffineTransform(translationX: center.x, y: center.y)
                .rotated(by: 28 * .pi / 180)
                .translatedBy(x: -center.x, y: -center.y)

            let orbit1 = Path(ellipseIn: CGRect(x: (512 - 374) * s, y: (516 - 128) * s, width: 748 * s, height: 256 * s)).applying(tilt1)
            let orbit2 = Path(ellipseIn: CGRect(x: (512 - 342) * s, y: (516 - 108) * s, width: 684 * s, height: 216 * s)).applying(tilt2)
            let back1 = Path(CGRect(x: -200 * s, y: -200 * s, width: 1424 * s, height: 716 * s)).applying(tilt1)
            let front1 = Path(CGRect(x: -200 * s, y: 516 * s, width: 1424 * s, height: 708 * s)).applying(tilt1)
            let back2 = Path(CGRect(x: -200 * s, y: -200 * s, width: 1424 * s, height: 716 * s)).applying(tilt2)
            let front2 = Path(CGRect(x: -200 * s, y: 516 * s, width: 1424 * s, height: 708 * s)).applying(tilt2)

            // Back orbits
            var b2 = ctx
            b2.clip(to: back2)
            b2.stroke(orbit2, with: .color(G.violet.opacity(0.28)), style: StrokeStyle(lineWidth: max(11 * s, 0.9)))
            var b1 = ctx
            b1.clip(to: back1)
            b1.stroke(orbit1, with: .color(G.indigo.opacity(0.35)), style: StrokeStyle(lineWidth: max(14 * s, 1.0)))

            // Cube faces
            ctx.fill(poly([(512, 300), (686, 400), (512, 500), (338, 400)]), with: .color(Color(hex: 0x3D4996)))
            ctx.fill(poly([(338, 400), (512, 500), (512, 702), (338, 602)]), with: .color(Color(hex: 0x1A204A)))
            ctx.fill(poly([(512, 500), (686, 400), (686, 602), (512, 702)]), with: .color(Color(hex: 0x0C0F26)))

            // Edge highlights
            let topEdge = Path { p in
                p.move(to: pt(338, 400))
                p.addLine(to: pt(512, 300))
                p.addLine(to: pt(686, 400))
            }
            ctx.stroke(topEdge, with: .color(Color(hex: 0xC7D2FE).opacity(0.75)), style: StrokeStyle(lineWidth: max(4 * s, 0.7), lineJoin: .round))
            let midEdges = Path { p in
                p.move(to: pt(338, 400))
                p.addLine(to: pt(512, 500))
                p.addLine(to: pt(686, 400))
                p.move(to: pt(512, 500))
                p.addLine(to: pt(512, 702))
            }
            ctx.stroke(midEdges, with: .color(G.sky.opacity(0.45)), style: StrokeStyle(lineWidth: max(3.5 * s, 0.6), lineJoin: .round))

            // Front orbits
            var f2 = ctx
            f2.clip(to: front2)
            f2.stroke(orbit2, with: .color(G.violet.opacity(0.75)), style: StrokeStyle(lineWidth: max(11 * s, 0.9)))
            let p2 = pt(772, 586).applying(tilt2)
            let r2 = max(13 * s, 0.9)
            ctx.fill(Path(ellipseIn: CGRect(x: p2.x - r2 * 2.2, y: p2.y - r2 * 2.2, width: r2 * 4.4, height: r2 * 4.4)),
                     with: .color(G.violet.opacity(0.32)))
            ctx.fill(Path(ellipseIn: CGRect(x: p2.x - r2, y: p2.y - r2, width: r2 * 2, height: r2 * 2)),
                     with: .color(.white))

            var f1 = ctx
            f1.clip(to: front1)
            f1.stroke(orbit1, with: .linearGradient(
                Gradient(colors: [G.sky, G.indigo, G.violet]),
                startPoint: pt(140, 540),
                endPoint: pt(880, 540)
            ), style: StrokeStyle(lineWidth: max(15 * s, 1.1)))

            // Primary graviton particle
            let particle = pt(204, 588).applying(tilt1)
            let r = max(24 * s, 1.3)
            ctx.fill(Path(ellipseIn: CGRect(x: particle.x - r * 2.8, y: particle.y - r * 2.8, width: r * 5.6, height: r * 5.6)),
                     with: .color(G.sky.opacity(0.32)))
            ctx.fill(Path(ellipseIn: CGRect(x: particle.x - r, y: particle.y - r, width: r * 2, height: r * 2)),
                     with: .color(.white))
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

// MARK: - Cards

public extension View {
    /// Borderless tonal elevation matching Stratus.
    func gravitonCard(_ fill: Color = G.surface, radius: CGFloat = 12) -> some View {
        background(fill, in: RoundedRectangle(cornerRadius: radius, style: .continuous))
    }
}
