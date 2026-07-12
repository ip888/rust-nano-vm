//! Small RFC 3339 / civil-date helpers shared by the audit log appender
//! and the `/v1/health` handler.
//!
//! Hand-rolled to avoid pulling `chrono` in for what amounts to ~15
//! lines of date math. UTC only — every consumer here only ever wants
//! UTC, so a single hard-wired offset keeps the API minimal.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time as an RFC 3339 / ISO 8601 string with
/// millisecond precision and a literal `Z` (UTC) suffix:
/// `YYYY-MM-DDTHH:MM:SS.mmmZ`. 24 characters, always.
pub(crate) fn rfc3339_now() -> String {
    rfc3339_offset(0)
}

/// Wall-clock time offset by `offset_secs` seconds, in the same RFC
/// 3339 shape as [`rfc3339_now`]. Negative offsets are supported;
/// underflow past the epoch clamps to `1970-01-01T00:00:00.000Z`.
/// Used for computing token expiry timestamps that can be lexicographically
/// compared with other RFC 3339 UTC strings.
pub(crate) fn rfc3339_offset(offset_secs: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // i64 + u64 → do arithmetic in i64 with saturating, then clamp
    // to 0. Underflow past epoch → the exact `1970-01-01T00:00:00.000Z`
    // string documented above. We deliberately drop the current
    // `subsec_millis` in the clamp branch — leaving them in would emit
    // `1970-01-01T00:00:00.<nonzero>Z`, which contradicts the doc and
    // makes the value depend on wall-clock at the moment of clamp.
    let signed_secs = (now.as_secs() as i64).saturating_add(offset_secs);
    let (secs, millis) = if signed_secs < 0 {
        (0u64, 0u32)
    } else {
        (signed_secs as u64, now.subsec_millis())
    };
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let s_of_day = (secs % 86_400) as u32;
    let h = s_of_day / 3600;
    let m = (s_of_day % 3600) / 60;
    let s = s_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

/// Convert a count of days since 1970-01-01 (UTC) into a `(year, month,
/// day)` civil date. Howard Hinnant's algorithm (public domain), trimmed
/// for the non-negative input we actually use. Handles leap years
/// exactly.
pub(crate) fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_shape() {
        let s = rfc3339_now();
        assert_eq!(s.len(), 24, "got {s}");
        assert!(s.ends_with('Z'));
        assert_eq!(s.chars().nth(4).unwrap(), '-');
        assert_eq!(s.chars().nth(7).unwrap(), '-');
        assert_eq!(s.chars().nth(10).unwrap(), 'T');
        assert_eq!(s.chars().nth(13).unwrap(), ':');
        assert_eq!(s.chars().nth(16).unwrap(), ':');
        assert_eq!(s.chars().nth(19).unwrap(), '.');
    }

    #[test]
    fn civil_anchors() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10957), (2000, 1, 1));
        assert_eq!(civil_from_days(18321), (2020, 2, 29));
        assert_eq!(civil_from_days(18322), (2020, 3, 1));
        assert_eq!(civil_from_days(18687), (2021, 3, 1));
    }
}
