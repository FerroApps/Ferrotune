//! When automatic checks run: 10-20 s after start, then every 6 h ± 30 min,
//! also while hidden in the tray. Never in mock/screenshot/debug builds
//! unless `FERRO_UPDATE_FORCE=1`.

use std::time::Duration;

pub const FIRST_MIN: Duration = Duration::from_secs(10);
pub const FIRST_MAX: Duration = Duration::from_secs(20);
pub const INTERVAL: Duration = Duration::from_secs(6 * 3600);
pub const JITTER: Duration = Duration::from_secs(30 * 60);

/// `unit` in `[0, 1)` (random) → delay before the first check.
pub fn first_delay(unit: f64) -> Duration {
    lerp(FIRST_MIN, FIRST_MAX, unit)
}

/// `unit` in `[0, 1)` → delay until the next check.
pub fn next_delay(unit: f64) -> Duration {
    lerp(INTERVAL - JITTER, INTERVAL + JITTER, unit)
}

fn lerp(lo: Duration, hi: Duration, unit: f64) -> Duration {
    let u = if unit.is_finite() {
        unit.clamp(0.0, 1.0)
    } else {
        0.0
    };
    lo + (hi - lo).mul_f64(u)
}

/// Whether timed checks may run in this process at all.
pub fn auto_allowed(mock: bool, screenshot: bool, debug_build: bool, force: bool) -> bool {
    force || !(mock || screenshot || debug_build)
}

/// `FERRO_UPDATE_FORCE=1`.
pub fn force_from_env() -> bool {
    std::env::var("FERRO_UPDATE_FORCE").is_ok_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_in_bounds() {
        for i in 0..=100 {
            let u = f64::from(i) / 100.0;
            let f = first_delay(u);
            assert!(f >= FIRST_MIN && f <= FIRST_MAX, "{f:?}");
            let n = next_delay(u);
            assert!(n >= INTERVAL - JITTER && n <= INTERVAL + JITTER, "{n:?}");
        }
        assert_eq!(first_delay(f64::NAN), FIRST_MIN);
        assert_eq!(first_delay(-3.0), FIRST_MIN);
        assert_eq!(first_delay(7.0), FIRST_MAX);
        let r: f64 = rand::random();
        assert!(first_delay(r) >= FIRST_MIN);
    }

    #[test]
    fn no_timed_checks_in_mock_screenshot_or_debug() {
        assert!(auto_allowed(false, false, false, false));
        assert!(!auto_allowed(true, false, false, false));
        assert!(!auto_allowed(false, true, false, false));
        assert!(!auto_allowed(false, false, true, false));
        assert!(auto_allowed(true, true, true, true), "FERRO_UPDATE_FORCE=1");
    }
}
