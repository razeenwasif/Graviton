import SwiftUI

/// Stratus-inspired pitch-black precision design tokens for Graviton.
public enum G {
    // Surfaces, back to front (pitch black -> subtle tonal elevation, zero borders).
    public static let bg = Color(hex: 0x000000)
    public static let background = Color(hex: 0x000000)
    public static let sidebar = Color(hex: 0x030304)
    public static let inspector = Color(hex: 0x060608)
    public static let sheet = Color(hex: 0x050507)
    public static let palette = Color(hex: 0x0B0B0F)
    public static let surface = Color(hex: 0x0B0B0E)
    public static let surfaceSubtle = Color(hex: 0x0B0B0E)
    public static let card = Color(hex: 0x0B0B0E)
    public static let surfaceHover = Color(hex: 0x111114)
    public static let raised = Color(hex: 0x111114)
    public static let surfaceActive = Color(hex: 0x16161B)
    public static let surfacePill = Color(hex: 0x151519)
    public static let control = Color(hex: 0x151519)
    public static let controlHover = Color(hex: 0x1C1C22)
    public static let console = Color(hex: 0x000000)
    public static let border = Color.white.opacity(0.05)
    public static let borderStrong = Color.white.opacity(0.09)

    // Text hierarchy.
    public static let text = Color(hex: 0xF4F4F6)
    public static let textPrimary = Color(hex: 0xF4F4F6)
    public static let body = Color(hex: 0xD4D4DC)
    public static let textSecondary = Color(hex: 0x8E8E99)
    public static let muted = Color(hex: 0x6E6E77)
    public static let textMuted = Color(hex: 0x6E6E77)
    public static let faint = Color(hex: 0x585862)
    public static let textTertiary = Color(hex: 0x585862)

    // Semantic accents.
    public static let accent = Color(hex: 0x7A89FF)
    public static let indigo = Color(hex: 0x7A89FF)
    public static let indigoBright = Color(hex: 0x8F9CFF)
    public static let sky = Color(hex: 0x4BC3F7)
    public static let mint = Color(hex: 0x34D399)
    public static let amber = Color(hex: 0xFBBF24)
    public static let rose = Color(hex: 0xF87171)
    public static let coral = Color(hex: 0xE88255)
    public static let violet = Color(hex: 0xA78BFA)
    public static let slate = Color(hex: 0x71717A)

    public static let onAccent = Color.white
    public static let running = Color(hex: 0x34D399)
    public static let warning = Color(hex: 0xFBBF24)
    public static let danger = Color(hex: 0xF87171)
    public static let stopped = Color(hex: 0x71717A)

    /// Chart series colors on obsidian surface.
    public static let chartPrimary = Color(hex: 0x7A89FF)
    public static let chartSecondary = Color(hex: 0x4BC3F7)

    // Typography.
    public static func heroKPI(_ size: CGFloat = 34) -> Font {
        .system(size: size, weight: .bold).monospacedDigit()
    }

    public static func pageTitle() -> Font {
        .system(size: 22, weight: .semibold)
    }

    public static func sectionTitle() -> Font {
        .system(size: 13, weight: .semibold)
    }

    public static func statValue(_ size: CGFloat = 20) -> Font {
        .system(size: size, weight: .semibold).monospacedDigit()
    }

    public static func mono(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight, design: .monospaced).monospacedDigit()
    }

    public static func color(for level: LogLevel) -> Color {
        switch level {
        case .debug: faint
        case .info: sky
        case .warn: amber
        case .error: rose
        }
    }
}

public extension Color {
    init(hex: UInt32, opacity: Double = 1) {
        self.init(.sRGB,
                  red: Double((hex >> 16) & 0xFF) / 255,
                  green: Double((hex >> 8) & 0xFF) / 255,
                  blue: Double(hex & 0xFF) / 255,
                  opacity: opacity)
    }
}

public enum Format {
    public static func bytes(_ n: UInt64) -> String {
        let units = ["B", "KB", "MB", "GB", "TB"]
        var value = Double(n)
        var unit = 0
        while value >= 1024, unit < units.count - 1 {
            value /= 1024
            unit += 1
        }
        if unit == 0 { return "\(n) B" }
        return value >= 100 ? String(format: "%.0f %@", value, units[unit]) : String(format: "%.1f %@", value, units[unit])
    }

    /// "45s", "41m", "3h 12m", "2d 4h".
    public static func uptime(_ seconds: TimeInterval) -> String {
        let s = max(0, Int(seconds))
        switch s {
        case ..<60: return "\(s)s"
        case ..<3600: return "\(s / 60)m"
        case ..<86_400: return "\(s / 3600)h \((s % 3600) / 60)m"
        default: return "\(s / 86_400)d \((s % 86_400) / 3600)h"
        }
    }

    public static func percent(_ v: Double?) -> String {
        guard let v else { return "—" }
        return String(format: "%.1f%%", v)
    }

    public static func clock(_ date: Date) -> String {
        date.formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute(.twoDigits).second(.twoDigits)) +
            String(format: ".%03d", Int(date.timeIntervalSince1970 * 1000) % 1000)
    }

    /// Parse relative durations (`30s`, `5m`, `2h`, `1d`), Unix epoch seconds/ms, or RFC 3339 / ISO 8601 timestamps into Unix milliseconds.
    public static func parseLogTimestamp(_ input: String, now: Date = .now) -> UInt64? {
        let trimmed = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }

        if let unit = trimmed.last?.lowercased().first, "smhd".contains(unit), trimmed.count >= 2 {
            let numPart = String(trimmed.dropLast())
            if let value = Double(numPart), value.isFinite, value >= 0 {
                let multiplier: TimeInterval = switch unit {
                case "s": 1
                case "m": 60
                case "h": 3600
                case "d": 86_400
                default: 0
                }
                return UInt64(max(0, (now.timeIntervalSince1970 - value * multiplier) * 1000))
            }
        }

        if trimmed.allSatisfy(\.isASCII) && trimmed.allSatisfy(\.isNumber) {
            guard let raw = UInt64(trimmed) else { return nil }
            if trimmed.count >= 12 { return raw }
            return raw.multipliedReportingOverflow(by: 1000).partialValue
        }

        let frac = ISO8601DateFormatter()
        frac.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = frac.date(from: trimmed) {
            return UInt64(max(0, date.timeIntervalSince1970 * 1000))
        }
        let plain = ISO8601DateFormatter()
        plain.formatOptions = [.withInternetDateTime]
        if let date = plain.date(from: trimmed) {
            return UInt64(max(0, date.timeIntervalSince1970 * 1000))
        }
        return nil
    }
}
