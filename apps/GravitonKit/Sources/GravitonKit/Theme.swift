import SwiftUI

/// Design tokens from the Graviton mockups (dark, minimal, one accent).
public enum G {
    // Surfaces, back to front.
    public static let background = Color(hex: 0x0B0C0E)
    public static let sidebar = Color(hex: 0x0F1013)
    public static let surface = Color(hex: 0x111215)
    public static let card = Color(hex: 0x131418)
    public static let raised = Color(hex: 0x16171B)
    public static let control = Color(hex: 0x1A1C21)
    public static let console = Color(hex: 0x08090B)
    public static let border = Color.white.opacity(0.06)
    public static let borderStrong = Color.white.opacity(0.09)

    // Text.
    public static let text = Color(hex: 0xECEDEF)
    public static let textSecondary = Color(hex: 0xB4B9C2)
    public static let muted = Color(hex: 0x9BA1AB)
    public static let faint = Color(hex: 0x808794)

    // Accent + status. Status colours only ever mean status.
    public static let accent = Color(hex: 0x7C8CFF)
    public static let onAccent = Color(hex: 0x0B0C0E)
    public static let running = Color(hex: 0x3DD68C)
    public static let warning = Color(hex: 0xF5B547)
    public static let danger = Color(hex: 0xFF8A8A)
    public static let stopped = Color(hex: 0x5A606B)
    /// Chart series, checked with the dataviz palette validator on the card
    /// surface (dark band, colour-blind ΔE 26.6): one-series charts and
    /// received traffic in blue, sent traffic in amber.
    public static let chartPrimary = Color(hex: 0x6E7EF2)
    public static let chartSecondary = Color(hex: 0xB98142)

    public static func mono(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight, design: .monospaced)
    }

    public static func color(for level: LogLevel) -> Color {
        switch level {
        case .debug: faint
        case .info: accent
        case .warn: warning
        case .error: danger
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
