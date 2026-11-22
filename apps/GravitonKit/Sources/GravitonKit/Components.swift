import SwiftUI

// MARK: - Status

public struct StatusDot: View {
    public var running: Bool
    public var size: CGFloat

    public init(running: Bool, size: CGFloat = 8) {
        self.running = running
        self.size = size
    }

    public var body: some View {
        Circle()
            .fill(running ? G.running : G.stopped)
            .frame(width: size, height: size)
            .background {
                if running { Circle().fill(G.running.opacity(0.18)).frame(width: size + 6, height: size + 6) }
            }
            .accessibilityLabel(running ? "Running" : "Stopped")
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
        let running = container.isRunning
        let tint = running ? G.running : G.muted
        HStack(spacing: 6) {
            Circle().fill(tint).frame(width: 6, height: 6)
            Text(running ? (showUptime ? "Running · \(container.statusText())" : "Running") : container.statusText())
        }
        .font(.system(size: 12, weight: .medium))
        .foregroundStyle(tint)
        .padding(.horizontal, 9)
        .frame(height: 22)
        .background(tint.opacity(0.12), in: Capsule())
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
                    .fill(color.opacity(0.12))
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
        (Text(value).font(.system(size: size, weight: .semibold))
            + Text(unit).font(.system(size: 13, weight: .medium)).foregroundStyle(G.muted))
            .kerning(-0.3)
            .foregroundStyle(G.text)
    }
}

// MARK: - Logs

public struct LogRow: View {
    public var line: LogLine
    public var showSequence: Bool
    public var compact: Bool

    public init(_ line: LogLine, showSequence: Bool = true, compact: Bool = false) {
        self.line = line
        self.showSequence = showSequence
        self.compact = compact
    }

    private var highlight: Color {
        switch line.level {
        case .error: G.danger.opacity(0.08)
        case .warn: G.warning.opacity(0.06)
        default: .clear
        }
    }

    public var body: some View {
        Group {
            if compact {
                VStack(alignment: .leading, spacing: 1) {
                    HStack(spacing: 8) {
                        Text(Format.clock(line.date)).foregroundStyle(G.faint)
                        Text(line.level.label).foregroundStyle(G.color(for: line.level))
                    }
                    Text(line.message).foregroundStyle(Color(hex: 0xC9CDD4))
                }
                .font(G.mono(11.5))
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 14)
                .padding(.vertical, 4)
            } else {
                HStack(alignment: .firstTextBaseline, spacing: 16) {
                    if showSequence {
                        Text(String(line.seq)).foregroundStyle(Color(hex: 0x5F6672)).frame(minWidth: 34, alignment: .trailing)
                    }
                    Text(Format.clock(line.date)).foregroundStyle(G.faint)
                    Text(line.level.label).foregroundStyle(G.color(for: line.level)).frame(width: 44, alignment: .leading)
                    Text(line.message).foregroundStyle(Color(hex: 0xC9CDD4)).textSelection(.enabled)
                    Spacer(minLength: 0)
                }
                .font(G.mono(12.5))
                .padding(.horizontal, 18)
                .padding(.vertical, 2)
            }
        }
        .background(highlight)
    }
}

// MARK: - Brand mark

/// The Graviton mark: a container cube inside a tilted orbit, with a particle.
/// Same geometry as `design/icon.svg`, drawn natively so it stays crisp.
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
                ctx.fill(Path(roundedRect: CGRect(origin: .zero, size: canvas), cornerRadius: 230 * s, style: .continuous),
                         with: .color(Color(hex: 0x14161F)))
            }
            let center = pt(512, 540)
            let tilt = CGAffineTransform(translationX: center.x, y: center.y).rotated(by: -18 * .pi / 180).translatedBy(x: -center.x, y: -center.y)
            let orbit = Path(ellipseIn: CGRect(x: (512 - 340) * s, y: (540 - 118) * s, width: 680 * s, height: 236 * s)).applying(tilt)
            let stroke = StrokeStyle(lineWidth: max(20 * s, 1.2))
            let back = Path(CGRect(x: 0, y: 0, width: 1024 * s, height: 540 * s)).applying(tilt)
            let front = Path(CGRect(x: 0, y: 540 * s, width: 1024 * s, height: 484 * s)).applying(tilt)

            var behind = ctx
            behind.clip(to: back)
            behind.stroke(orbit, with: .color(G.accent.opacity(0.4)), style: stroke)

            ctx.fill(poly([(512, 330), (677, 425), (512, 520), (347, 425)]), with: .color(Color(hex: 0xB4BDFF)))
            ctx.fill(poly([(347, 425), (512, 520), (512, 710), (347, 615)]), with: .color(Color(hex: 0x5B6BF0)))
            ctx.fill(poly([(512, 520), (677, 425), (677, 615), (512, 710)]), with: .color(Color(hex: 0x323DA6)))

            var infront = ctx
            infront.clip(to: front)
            infront.stroke(orbit, with: .color(G.accent), style: stroke)

            let particle = CGPoint(x: 218 * s, y: 599 * s).applying(tilt)
            let r = max(22 * s, 1.2)
            ctx.fill(Path(ellipseIn: CGRect(x: particle.x - r * 2.4, y: particle.y - r * 2.4, width: r * 4.8, height: r * 4.8)),
                     with: .color(G.accent.opacity(0.25)))
            ctx.fill(Path(ellipseIn: CGRect(x: particle.x - r, y: particle.y - r, width: r * 2, height: r * 2)), with: .color(.white))
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

// MARK: - Cards

public extension View {
    /// Rounded surface with the hairline border used throughout the mockups.
    func gravitonCard(_ fill: Color = G.surface, radius: CGFloat = 12) -> some View {
        background(fill, in: RoundedRectangle(cornerRadius: radius, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: radius, style: .continuous).strokeBorder(G.border))
    }
}
