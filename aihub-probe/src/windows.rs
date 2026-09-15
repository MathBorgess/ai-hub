use aihub_core::{QuotaStatus, QuotaWindow};

/// Pure helper: computes seconds remaining until window reset.
///
/// If `duration_s` is provided alongside `start_epoch_s`, this computes the remaining
/// seconds from now until `start_epoch_s + duration_s`.
/// Returns `None` if the window has already expired or overflowed.
pub fn compute_window_resets(start_epoch_s: u64, duration_s: u64) -> Option<u64> {
    let target = start_epoch_s.checked_add(duration_s)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if target > now {
        Some(target - now)
    } else {
        None
    }
}

/// Pure helper: picks the most restrictive / tightest window (highest used_pct).
///
/// Ported from handoff.mjs `pickTightest(windows)`.
/// In handoff.mjs, tightest was lowest remaining_pct (`b.remaining_pct < a.remaining_pct`).
/// In aihub-core, used_pct = 100 - remaining_pct, so tightest is highest used_pct.
pub fn pick_tightest_window(windows: &[QuotaWindow]) -> Option<&QuotaWindow> {
    windows.iter().max_by(|a, b| {
        a.used_pct
            .partial_cmp(&b.used_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// Pure helper: maps a percentage used to a QuotaStatus bucket given a low threshold.
///
/// In handoff.mjs:
/// - if remaining is null/undefined -> "unknown"
/// - if remaining <= 0 -> "empty" (i.e. used_pct >= 100.0)
/// - if remaining < LOW_PCT -> "low" (i.e. used_pct > 100.0 - low_threshold_pct)
/// - else "ok"
///
/// `used_pct` is in range 0.0..=100.0.
/// `low_threshold_pct` is the low remaining threshold (e.g. 20.0 means remaining < 20% is Low).
pub fn bucket_for_usage(used_pct: f64, low_threshold_pct: f64) -> QuotaStatus {
    if !used_pct.is_finite() {
        return QuotaStatus::Unknown;
    }
    if used_pct >= 100.0 {
        QuotaStatus::Empty
    } else if (100.0 - used_pct) < low_threshold_pct {
        QuotaStatus::Low
    } else {
        QuotaStatus::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aihub_core::WindowKind;

    #[test]
    fn test_pick_tightest_window() {
        let w1 = QuotaWindow::new(WindowKind::FiveHour, 20.0, Some(3600), Some(18000));
        let w2 = QuotaWindow::new(WindowKind::SevenDay, 85.0, Some(86400), Some(604800));
        let w3 = QuotaWindow::new(WindowKind::FiveHour, 50.0, Some(7200), Some(18000));

        let windows = vec![w1, w2, w3];
        let tightest = pick_tightest_window(&windows).expect("tightest window");
        assert_eq!(tightest.kind, WindowKind::SevenDay);
        assert_eq!(tightest.used_pct, 85.0);
    }

    #[test]
    fn test_bucket_for_usage() {
        // low_threshold_pct = 20.0 (remaining < 20% means used_pct > 80.0)
        assert_eq!(bucket_for_usage(100.0, 20.0), QuotaStatus::Empty);
        assert_eq!(bucket_for_usage(105.0, 20.0), QuotaStatus::Empty);
        assert_eq!(bucket_for_usage(90.0, 20.0), QuotaStatus::Low);
        assert_eq!(bucket_for_usage(80.01, 20.0), QuotaStatus::Low);
        assert_eq!(bucket_for_usage(80.0, 20.0), QuotaStatus::Ok);
        assert_eq!(bucket_for_usage(10.0, 20.0), QuotaStatus::Ok);
        assert_eq!(bucket_for_usage(0.0, 20.0), QuotaStatus::Ok);
        assert_eq!(bucket_for_usage(f64::NAN, 20.0), QuotaStatus::Unknown);
    }

    #[test]
    fn test_compute_window_resets() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // 100 seconds in the future
        let res = compute_window_resets(now - 100, 200);
        assert!(res.is_some());
        let secs = res.unwrap();
        assert!((98..=100).contains(&secs));

        // Expired
        let expired = compute_window_resets(now - 300, 200);
        assert_eq!(expired, None);
    }
}
