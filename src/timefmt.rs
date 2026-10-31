//! Parsing `--since`/`--until` values and formatting times for people.

use std::time::Duration;

use anyhow::{Result, anyhow};
use jiff::Timestamp;
use jiff::tz::TimeZone;

pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

/// Accepts relative ages (`30m`, `12h`, `7d`, `2w`), Unix seconds or milliseconds,
/// RFC 3339 timestamps, and local dates or date-times (`2026-10-01`, `2026-10-01 09:30`).
pub fn parse_time(text: &str) -> Result<i64> {
    parse_time_at(text, now_ms(), &TimeZone::system())
}

fn parse_time_at(text: &str, now: i64, tz: &TimeZone) -> Result<i64> {
    let text = text.trim();
    let invalid = || {
        anyhow!(
            "invalid time '{text}' (examples: 7d, 12h, 2026-10-01, 2026-10-01 09:30, 2026-10-01T09:30:00Z)"
        )
    };
    if text.is_empty() {
        return Err(invalid());
    }
    if let Some(unit) = text.chars().last().filter(|c| c.is_ascii_alphabetic())
        && text[..text.len() - 1].bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = text[..text.len() - 1].parse::<i64>()
    {
        let ms = match unit {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            'w' => 7 * 86_400_000,
            _ => return Err(invalid()),
        };
        return n
            .checked_mul(ms)
            .and_then(|age| now.checked_sub(age))
            .ok_or_else(invalid);
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        let n: i64 = text.parse().map_err(|_| invalid())?;
        // Thirteen digits covers milliseconds since 2001; ten covers seconds.
        return Ok(if text.len() >= 12 { n } else { n * 1000 });
    }
    if let Ok(ts) = text.parse::<Timestamp>() {
        return Ok(ts.as_millisecond());
    }
    // Date-time first: jiff's date parser accepts a time component and drops it.
    let normalized = text.replacen(' ', "T", 1);
    if let Ok(dt) = normalized.parse::<jiff::civil::DateTime>() {
        return Ok(dt.to_zoned(tz.clone())?.timestamp().as_millisecond());
    }
    if let Ok(date) = text.parse::<jiff::civil::Date>() {
        return Ok(date.to_zoned(tz.clone())?.timestamp().as_millisecond());
    }
    Err(invalid())
}

/// RFC 3339 in local time with its offset, second precision, for machine-readable output:
/// the same clock as the text output, and still unambiguous.
pub fn rfc3339(ms: i64) -> String {
    rfc3339_in(ms, &TimeZone::system())
}

fn rfc3339_in(ms: i64, tz: &TimeZone) -> String {
    Timestamp::from_second(ms.div_euclid(1000))
        .map(|ts| {
            ts.to_zoned(tz.clone())
                .strftime("%Y-%m-%dT%H:%M:%S%:z")
                .to_string()
        })
        .unwrap_or_default()
}

fn local(ms: i64, fmt: &str) -> String {
    Timestamp::from_millisecond(ms)
        .map(|ts| ts.to_zoned(TimeZone::system()).strftime(fmt).to_string())
        .unwrap_or_else(|_| "-".into())
}

/// `2026-10-30 15:51`
pub fn local_minute(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    local(ms, "%Y-%m-%d %H:%M")
}

/// `2026-10-30 15:51:07`
pub fn local_second(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    local(ms, "%Y-%m-%d %H:%M:%S")
}

pub fn human_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

pub fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n}B")
    } else if n < 1024 * 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{:.1}MB", n as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relative_absolute_and_local_times() {
        let tz = TimeZone::get("Asia/Shanghai").unwrap();
        let now = 1_793_345_000_000;
        assert_eq!(parse_time_at("7d", now, &tz).unwrap(), now - 7 * 86_400_000);
        assert_eq!(parse_time_at("30m", now, &tz).unwrap(), now - 1_800_000);
        assert_eq!(
            parse_time_at("1705315800", now, &tz).unwrap(),
            1_705_315_800_000
        );
        assert_eq!(
            parse_time_at("1705315800000", now, &tz).unwrap(),
            1_705_315_800_000
        );
        assert_eq!(
            parse_time_at("2024-01-15T10:30:00Z", now, &tz).unwrap(),
            1_705_314_600_000
        );
        // Local midnight in UTC+8 is 16:00 UTC the previous day.
        assert_eq!(
            parse_time_at("2024-01-15", now, &tz).unwrap(),
            1_705_248_000_000
        );
        assert_eq!(
            parse_time_at("2024-01-15 09:30", now, &tz).unwrap(),
            1_705_282_200_000
        );
        assert!(parse_time_at("yesterday", now, &tz).is_err());
        assert!(parse_time_at("7y", now, &tz).is_err());
        assert!(parse_time_at("-5d", now, &tz).is_err());
        assert!(parse_time_at("99999999999999999d", now, &tz).is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(
            rfc3339_in(1_705_314_600_123, &TimeZone::UTC),
            "2024-01-15T10:30:00+00:00"
        );
        let shanghai = TimeZone::get("Asia/Shanghai").unwrap();
        assert_eq!(
            rfc3339_in(1_705_314_600_123, &shanghai),
            "2024-01-15T18:30:00+08:00"
        );
        assert_eq!(human_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(human_duration(Duration::from_secs(5)), "5s");
        assert_eq!(human_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(human_bytes(3181), "3.1KB");
        assert_eq!(local_minute(0), "-");
    }
}
