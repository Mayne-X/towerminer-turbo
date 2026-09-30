//! Display helpers.

pub fn hashrate(hps: f64) -> String {
    if hps >= 1e6 {
        format!("{:.2} MH/s", hps / 1e6)
    } else if hps >= 1e4 {
        format!("{:.1} kH/s", hps / 1e3)
    } else if hps >= 1e3 {
        format!("{:.2} kH/s", hps / 1e3)
    } else {
        format!("{:.0} H/s", hps)
    }
}

pub fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC from a Unix timestamp.
pub fn utc(ts: u64) -> String {
    let (y, mo, d) = civil_from_days((ts / 86_400) as i64);
    let s = ts % 86_400;
    format!("{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Long block hashes: first 12 and last 8 hex digits.
pub fn short_hash(h: &str) -> String {
    if h.chars().count() <= 24 {
        h.to_string()
    } else {
        let head: String = h.chars().take(12).collect();
        let tail: String = h.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{head}...{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(utc(1_790_000_000), "2026-09-21 14:13:20");
        assert_eq!(utc(4_107_542_399), "2100-02-28 23:59:59");
    }

    #[test]
    fn rates_and_durations() {
        assert_eq!(hashrate(0.0), "0 H/s");
        assert_eq!(hashrate(999.4), "999 H/s");
        assert_eq!(hashrate(1234.0), "1.23 kH/s");
        assert_eq!(hashrate(16_892.4), "16.9 kH/s");
        assert_eq!(hashrate(2_500_000.0), "2.50 MH/s");
        assert_eq!(duration(59), "00:00:59");
        assert_eq!(duration(3_725), "01:02:05");
        assert_eq!(duration(90_061), "1d 01:01:01");
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(&"0123456789".repeat(7)), "012345678901...23456789");
    }
}
