//! Pure-Rust timestamp formatting and Docker/RFC 3339/duration parsing.

/// Format Unix milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn format_rfc3339_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let millis = ms % 1000;
    let days = (total_secs / 86_400) as i64;
    let rem = total_secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Parse a Go/Docker compound positive duration (`ns`, `us`/`µs`, `ms`, `s`, `m`, `h`, `d`) into milliseconds.
pub fn parse_duration_ms(input: &str) -> Option<u64> {
    let mut s = input.trim();
    if s.is_empty() || s.starts_with('-') || s.starts_with('+') {
        return None;
    }

    let mut total_ns: u128 = 0;
    let mut parsed_any = false;

    while !s.is_empty() {
        let mut int_part: u128 = 0;
        let mut has_int = false;
        while let Some(&b) = s.as_bytes().first() {
            if b.is_ascii_digit() {
                has_int = true;
                int_part = int_part.checked_mul(10)?.checked_add(u128::from(b - b'0'))?;
                s = &s[1..];
            } else {
                break;
            }
        }

        let mut frac_num: u128 = 0;
        let mut frac_den: u128 = 1;
        let mut has_frac = false;
        if let Some(after_dot) = s.strip_prefix('.') {
            s = after_dot;
            while let Some(&b) = s.as_bytes().first() {
                if b.is_ascii_digit() {
                    has_frac = true;
                    if frac_den <= 1_000_000_000_000 {
                        frac_num = frac_num * 10 + u128::from(b - b'0');
                        frac_den *= 10;
                    }
                    s = &s[1..];
                } else {
                    break;
                }
            }
        }

        if !has_int && !has_frac {
            return None;
        }

        let unit_end = s
            .char_indices()
            .find(|&(_, c)| c.is_ascii_digit() || c == '.')
            .map_or(s.len(), |(idx, _)| idx);
        if unit_end == 0 {
            return None;
        }
        let unit = &s[..unit_end];
        s = &s[unit_end..];

        let unit_ns: u128 = match unit {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3_600 * 1_000_000_000,
            "d" => 86_400 * 1_000_000_000,
            _ => return None,
        };

        let part_ns = int_part
            .checked_mul(unit_ns)?
            .checked_add(frac_num.checked_mul(unit_ns)? / frac_den)?;
        total_ns = total_ns.checked_add(part_ns)?;
        parsed_any = true;
    }

    parsed_any.then(|| u64::try_from(total_ns / 1_000_000).ok()).flatten()
}

/// Parse a Docker CLI / REST timestamp (Unix seconds, Go duration relative to `now_ms`, or RFC 3339 / ISO 8601).
pub fn parse_timestamp_ms(input: &str, now_ms: u64) -> Result<u64, String> {
    parse_timestamp_ms_with_offset(input, now_ms, 0)
}

/// Like [`parse_timestamp_ms`], using `default_offset_secs` when a date/datetime omits a timezone suffix.
pub fn parse_timestamp_ms_with_offset(input: &str, now_ms: u64, default_offset_secs: i64) -> Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("timestamp cannot be empty".into());
    }

    if let Some(ms) = parse_unix_seconds_ms(trimmed) {
        return Ok(ms);
    }

    if let Some(dur_ms) = parse_duration_ms(trimmed) {
        return Ok(now_ms.saturating_sub(dur_ms));
    }

    parse_iso_datetime_ms(trimmed, default_offset_secs)
        .ok_or_else(|| format!("invalid timestamp or duration `{trimmed}` (expected RFC 3339, Unix seconds, or e.g. 10m / 1h30m)"))
}

fn parse_unix_seconds_ms(s: &str) -> Option<u64> {
    let (secs_str, frac_str) = match s.split_once('.') {
        Some((a, b)) => {
            if a.is_empty() || b.is_empty() || !a.bytes().all(|c| c.is_ascii_digit()) || !b.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            (a, Some(b))
        }
        None => {
            if !s.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            (s, None)
        }
    };
    let secs: u64 = secs_str.parse().ok()?;
    let frac_ms = frac_str.map_or(0, parse_frac_ms);
    secs.checked_mul(1000)?.checked_add(frac_ms)
}

fn parse_frac_ms(frac: &str) -> u64 {
    let mut digits = [0u64; 3];
    for (i, b) in frac.bytes().take(3).enumerate() {
        digits[i] = u64::from(b - b'0');
    }
    digits[0] * 100 + digits[1] * 10 + digits[2]
}

fn parse_iso_datetime_ms(s: &str, default_offset_secs: i64) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    if year < 1970 || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }

    let (hour, min, sec, frac_ms, offset_secs) = if b.len() == 10 {
        (0u64, 0u64, 0u64, 0u64, default_offset_secs)
    } else {
        let sep = b[10];
        if sep != b'T' && sep != b't' && sep != b' ' {
            return None;
        }
        let rest = s.get(11..)?;
        if rest.len() < 8 || rest.as_bytes()[2] != b':' || rest.as_bytes()[5] != b':' {
            return None;
        }
        let hour: u64 = rest.get(0..2)?.parse().ok()?;
        let min: u64 = rest.get(3..5)?.parse().ok()?;
        let sec: u64 = rest.get(6..8)?.parse().ok()?;
        if hour > 23 || min > 59 || sec > 59 {
            return None;
        }
        let mut tail = rest.get(8..)?;
        let mut frac_ms = 0u64;
        if let Some(after_dot) = tail.strip_prefix('.') {
            let digits_len = after_dot.bytes().take_while(|c| c.is_ascii_digit()).count();
            if digits_len == 0 {
                return None;
            }
            frac_ms = parse_frac_ms(&after_dot[..digits_len]);
            tail = &after_dot[digits_len..];
        }
        let offset_secs = match tail {
            "" => default_offset_secs,
            "Z" | "z" => 0,
            _ => parse_tz_offset_secs(tail)?,
        };
        (hour, min, sec, frac_ms, offset_secs)
    };

    let days = days_from_civil(year, month, day);
    let local_secs = days
        .checked_mul(86_400)?
        .checked_add((hour * 3600 + min * 60 + sec) as i64)?;
    let utc_secs = local_secs.checked_sub(offset_secs)?;
    if utc_secs < 0 {
        return None;
    }
    u64::try_from(utc_secs).ok()?.checked_mul(1000)?.checked_add(frac_ms)
}

fn parse_tz_offset_secs(tz: &str) -> Option<i64> {
    let (sign, rest) = match tz.as_bytes().first()? {
        b'+' => (1i64, &tz[1..]),
        b'-' => (-1i64, &tz[1..]),
        _ => return None,
    };
    let (h, m): (i64, i64) = if rest.len() == 5 && rest.as_bytes()[2] == b':' {
        (rest.get(0..2)?.parse().ok()?, rest.get(3..5)?.parse().ok()?)
    } else if rest.len() == 4 && rest.bytes().all(|c| c.is_ascii_digit()) {
        (rest.get(0..2)?.parse().ok()?, rest.get(2..4)?.parse().ok()?)
    } else {
        return None;
    };
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

/// Days since 1970-01-01 using Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rfc3339_ms() {
        assert_eq!(format_rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339_ms(1_794_304_448_042), "2026-11-10T09:54:08.042Z");
    }

    #[test]
    fn parses_compound_durations() {
        assert_eq!(parse_duration_ms("500ms"), Some(500));
        assert_eq!(parse_duration_ms("1.5s"), Some(1500));
        assert_eq!(parse_duration_ms("1h30m45s"), Some(5_445_000));
        assert_eq!(parse_duration_ms("2d1h"), Some((2 * 86_400 + 3600) * 1000));
        assert_eq!(parse_duration_ms("2500us"), Some(2));
        assert_eq!(parse_duration_ms("1500000ns"), Some(1));
        assert_eq!(parse_duration_ms(""), None);
        assert_eq!(parse_duration_ms("10"), None);
        assert_eq!(parse_duration_ms("-5s"), None);
        assert_eq!(parse_duration_ms("10foo"), None);
    }

    #[test]
    fn parses_timestamps_and_relative_windows() {
        let now = 1_794_304_448_500;
        assert_eq!(parse_timestamp_ms("1794304448", now).unwrap(), 1_794_304_448_000);
        assert_eq!(parse_timestamp_ms("1794304448.05", now).unwrap(), 1_794_304_448_050);
        assert_eq!(parse_timestamp_ms("10m", now).unwrap(), now - 600_000);
        assert_eq!(parse_timestamp_ms("2026-11-10T09:54:08Z", now).unwrap(), 1_794_304_448_000);
        assert_eq!(parse_timestamp_ms("2026-11-10T09:54:08.123456Z", now).unwrap(), 1_794_304_448_123);
        assert_eq!(parse_timestamp_ms("2026-11-10T20:54:08+11:00", now).unwrap(), 1_794_304_448_000);
        assert_eq!(parse_timestamp_ms("2026-11-10 04:54:08-05:00", now).unwrap(), 1_794_304_448_000);
        // Naive date uses default_offset_secs (+11:00 = 39_600s -> 11h earlier in UTC).
        assert_eq!(
            parse_timestamp_ms_with_offset("2026-11-10", now, 11 * 3600).unwrap(),
            parse_timestamp_ms("2026-11-10T00:00:00+11:00", now).unwrap()
        );
    }

    #[test]
    fn rejects_invalid_timestamps() {
        let now = 1_794_304_448_500;
        assert!(parse_timestamp_ms("", now).is_err());
        assert!(parse_timestamp_ms("2025-02-29T00:00:00Z", now).is_err());
        assert!(parse_timestamp_ms("2024-02-29T00:00:00Z", now).is_ok());
        assert!(parse_timestamp_ms("2026-13-01T00:00:00Z", now).is_err());
        assert!(parse_timestamp_ms("2026-11-10T25:00:00Z", now).is_err());
        assert!(parse_timestamp_ms("not-a-time", now).is_err());
    }
}
