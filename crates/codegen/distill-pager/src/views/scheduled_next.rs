//! Next-fire suffix for scheduled `/loop` rows (tasks pane and dock Watchers).
//!
//! Wall-clock `next_fire_at` and monotonic elapsed since create stay separate
//! clocks: a pinned `now` in tests must not move the interval fallback.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::app::agent::ScheduledTaskInfo;
use crate::util::{format_duration, parse_schedule_interval_secs};

/// ` (next in 2m5s)`, ` (due now)`, or empty when no fire time can be derived.
pub(crate) fn next_suffix(info: &ScheduledTaskInfo, now: DateTime<Utc>) -> String {
    suffix(
        &info.human_schedule,
        info.created_at.elapsed(),
        info.next_fire_at.as_deref(),
        now,
    )
}

fn suffix(
    human_schedule: &str,
    elapsed: Duration,
    next_fire_at: Option<&str>,
    now: DateTime<Utc>,
) -> String {
    let remaining = if let Some(stamp) = next_fire_at
        && let Ok(dt) = DateTime::parse_from_rfc3339(stamp)
    {
        Some((dt.with_timezone(&Utc) - now).to_std().unwrap_or_default())
    } else {
        parse_schedule_interval_secs(human_schedule).map(|secs| {
            Duration::from_secs(secs)
                .checked_sub(elapsed)
                .unwrap_or(Duration::ZERO)
        })
    };
    match remaining {
        None => String::new(),
        Some(d) if d.is_zero() => " (due now)".to_owned(),
        Some(d) => format!(" (next in {})", format_duration(d)),
    }
}

#[cfg(test)]
#[path = "scheduled_next_tests.rs"]
mod tests;
