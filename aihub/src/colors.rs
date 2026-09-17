//! Quota color thresholds and style mappings.
//!
//! Constraint from brief:
//! "Quota colors. Pick three thresholds and keep them in one place in the code.
//! A single lane at 0% doesn't paint the whole slot red."

use ratatui::style::Color;

/// Warning threshold: percentage used above this gets Yellow.
pub const QUOTA_WARN_PCT: f64 = 70.0;
/// Critical threshold: percentage used above this gets Red.
pub const QUOTA_CRIT_PCT: f64 = 90.0;

/// Returns the color corresponding to how much quota is used.
///
/// - < 70%: Green (plentiful)
/// - >= 70% and < 90%: Yellow (warning / low)
/// - >= 90%: Red (critical / depleted)
pub fn quota_color(used_pct: f64) -> Color {
    if used_pct >= QUOTA_CRIT_PCT {
        Color::Red
    } else if used_pct >= QUOTA_WARN_PCT {
        Color::Yellow
    } else {
        Color::Green
    }
}
