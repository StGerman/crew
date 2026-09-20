//! Retry timing. Pure functions, so the awkward cases are cheap to test exhaustively.

/// First failure waits this long; each subsequent failure doubles it.
pub const BACKOFF_BASE_MS: u64 = 10_000;

/// Ceiling on the doubling. `2^16 * 10s` already exceeds any sane cap, so this only ever
/// prevents the shift itself from going out of range.
pub const EXP_CAP: u32 = 16;

/// Exponential backoff with a bounded exponent.
///
/// The spec writes `min(10000 * 2^(attempt-1), cap)`, which caps the *product* but lets the
/// exponent grow without limit. `attempt` climbs monotonically on the failure path, reaching 64
/// after roughly five hours at a 5-minute cap — and `10000 * 2^63` overflows a 64-bit integer.
/// In a release build that wraps silently, turning the longest backoff into the shortest exactly
/// when the system is already in trouble. Capping the exponent is the fix; the saturating
/// arithmetic is belt and braces.
pub fn backoff_ms(attempt: u32, cap_ms: u64) -> u64 {
    let shift = attempt.saturating_sub(1).min(EXP_CAP);
    BACKOFF_BASE_MS.saturating_mul(1u64 << shift).min(cap_ms)
}

/// Delay before re-dispatching a run that asked to continue.
///
/// The spec uses a flat 1 second forever, which is how an issue nothing can advance turns into
/// an unbounded respawn loop — and, at ten concurrent issues, roughly 600 tracker requests per
/// minute, enough to rate-limit itself off most providers. Here the delay escalates while
/// nothing observable changes, and resets the moment it does.
pub fn continuation_delay_ms(consecutive_no_progress: u32, poll_interval_ms: u64) -> u64 {
    match consecutive_no_progress {
        0 => 5_000,
        1 => 30_000,
        _ => poll_interval_ms.max(30_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 300_000;

    #[test]
    fn backoff_doubles_from_the_base_until_it_reaches_the_cap() {
        assert_eq!(backoff_ms(1, CAP), 10_000);
        assert_eq!(backoff_ms(2, CAP), 20_000);
        assert_eq!(backoff_ms(3, CAP), 40_000);
        assert_eq!(backoff_ms(4, CAP), 80_000);
        assert_eq!(backoff_ms(5, CAP), 160_000);
        assert_eq!(backoff_ms(6, CAP), CAP, "clamped from 320s");
    }

    #[test]
    fn backoff_never_overflows_or_collapses_at_any_attempt_count() {
        // The spec's formula wraps somewhere past attempt 63. Walk the whole u32 range at the
        // boundaries plus a dense sweep, and assert the delay is always the cap and never zero.
        for attempt in (0..=64).chain([100, 1_000, 65_535, u32::MAX - 1, u32::MAX]) {
            let d = backoff_ms(attempt, CAP);
            assert!(d <= CAP, "attempt {attempt} exceeded the cap: {d}");
            if attempt >= 6 {
                assert_eq!(d, CAP, "attempt {attempt} collapsed to {d}");
            }
        }
    }

    #[test]
    fn attempt_zero_is_treated_as_the_first_attempt() {
        assert_eq!(backoff_ms(0, CAP), BACKOFF_BASE_MS);
    }

    #[test]
    fn a_tiny_cap_wins_over_the_base_delay() {
        assert_eq!(backoff_ms(1, 500), 500);
        assert_eq!(backoff_ms(50, 500), 500);
    }

    #[test]
    fn continuation_backs_off_while_nothing_changes() {
        let poll = 30_000;
        assert_eq!(continuation_delay_ms(0, poll), 5_000);
        assert_eq!(continuation_delay_ms(1, poll), 30_000);
        assert_eq!(continuation_delay_ms(2, poll), 30_000);
        assert_eq!(continuation_delay_ms(99, poll), 30_000);
    }

    #[test]
    fn continuation_never_polls_faster_than_the_tick_itself() {
        // A long poll interval must not be undercut by the escalation ladder.
        assert_eq!(continuation_delay_ms(5, 120_000), 120_000);
    }
}
