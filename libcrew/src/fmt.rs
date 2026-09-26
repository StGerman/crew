//! Number and duration formatting shared by every surface that shows a snapshot, so a
//! duration means the same thing in the dashboard, `crewctl status` and anywhere else.

pub fn fmt_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

pub fn fmt_ms(ms: u64) -> String {
    let s = ms / 1_000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
    }
}

/// Wall-clock milliseconds as `YYYY-MM-DD HH:MM:SSZ`.
///
/// UTC, and said so, because the daemon may not be on this machine and a local rendering
/// would silently disagree with the timestamps in its own logs. Computed rather than
/// pulled from a formatting crate: `time` is already a dependency but only for parsing, and
/// one civil-date conversion is cheaper than the feature flag.
pub fn timestamp(ms: i64) -> String {
    let secs = ms.div_euclid(1_000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z", tod / 3_600, (tod % 3_600) / 60, tod % 60)
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a calendar date, exact for
/// every value this will ever see and with no table to get wrong.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
