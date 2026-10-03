//! UTC timestamp run identifiers (`20260902T231500Z`), no external date crate.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix time in seconds.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
}

/// `YYYYMMDDThhmmssZ` for the current UTC time.
pub fn now() -> String {
    from_unix(now_unix())
}

/// Parse a run id back to unix seconds, for sorting/GC. Returns `None` if it is
/// not one of our ids.
pub fn to_unix(id: &str) -> Option<u64> {
    let b = id.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| id.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(4..6)?, n(6..8)?);
    let (h, mi, s) = (n(9..11)?, n(11..13)?, n(13..15)?);
    let days = days_from_civil(y, mo, d);
    (days * 86400 + h * 3600 + mi * 60 + s).try_into().ok()
}

fn from_unix(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}{mo:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil (days since 1970-01-01).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        // 2026-09-02T23:15:00Z
        let secs = 1_788_390_900;
        let id = from_unix(secs);
        assert_eq!(id, "20260902T231500Z");
        assert_eq!(to_unix(&id), Some(secs));
    }

    #[test]
    fn rejects_foreign_ids() {
        assert_eq!(to_unix("not-a-runid"), None);
        assert_eq!(to_unix("snapper-42"), None);
    }
}
