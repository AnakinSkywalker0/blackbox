//! Parses the `WHEN` argument of `bb why`: "now", "15:40", "15:40:30", "10m ago".

use chrono::{DateTime, Duration, Local, NaiveTime, TimeZone};

/// Parses a duration like `30s`, `90s`, `10m`, `2h` into seconds.
pub fn parse_duration(s: &str) -> Result<i64, String> {
    let s = s.trim().to_lowercase();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("'{s}' needs a unit (s, m or h), e.g. 30s or 10m"))?;
    let (num, unit) = s.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| format!("'{s}' is not a valid duration, try 30s, 10m or 2h"))?;
    let mult = match unit.trim() {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        _ => return Err(format!("unknown unit in '{s}', use s, m or h")),
    };
    Ok(n * mult)
}

/// Resolves `input` to a unix timestamp, relative to `now`.
pub fn parse_when(input: &str, now: DateTime<Local>) -> Result<i64, String> {
    let s = input.trim().to_lowercase();
    if s.is_empty() || s == "now" {
        return Ok(now.timestamp());
    }
    if let Some(rest) = s.strip_suffix("ago") {
        let secs = parse_duration(rest)?;
        return Ok((now - Duration::seconds(secs)).timestamp());
    }
    let time = NaiveTime::parse_from_str(&s, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(&s, "%H:%M"))
        .map_err(|_| {
            format!("couldn't understand '{input}'. Try 15:40, 15:40:30, \"10m ago\" or now")
        })?;
    let mut date = now.date_naive();
    let mut candidate = Local.from_local_datetime(&date.and_time(time)).earliest();
    // A time that hasn't happened yet today means yesterday.
    if candidate.map_or(true, |c| c > now) {
        date = date.pred_opt().ok_or("date out of range")?;
        candidate = Local.from_local_datetime(&date.and_time(time)).earliest();
    }
    candidate
        .map(|c| c.timestamp())
        .ok_or_else(|| format!("'{input}' doesn't exist on that day (daylight saving gap)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 3, 10, 16, 0, 0).unwrap()
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("4m").unwrap(), 240);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("xm").is_err());
    }

    #[test]
    fn now_and_relative() {
        let n = now();
        assert_eq!(parse_when("now", n).unwrap(), n.timestamp());
        assert_eq!(parse_when("10m ago", n).unwrap(), n.timestamp() - 600);
        assert_eq!(parse_when("90s ago", n).unwrap(), n.timestamp() - 90);
    }

    #[test]
    fn clock_times() {
        let n = now();
        let t = parse_when("15:40", n).unwrap();
        assert_eq!(t, n.timestamp() - 20 * 60);
        let t = parse_when("15:40:30", n).unwrap();
        assert_eq!(t, n.timestamp() - 19 * 60 - 30);
    }

    #[test]
    fn future_time_means_yesterday() {
        let n = now();
        let t = parse_when("17:00", n).unwrap();
        assert_eq!(t, n.timestamp() + 3600 - 86400);
    }

    #[test]
    fn garbage_rejected() {
        assert!(parse_when("banana", now()).is_err());
        assert!(parse_when("25:99", now()).is_err());
    }
}
