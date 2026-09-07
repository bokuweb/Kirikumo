//! Ages, written the way `kubectl` writes them.
//!
//! Every table has an AGE column and every Kubernetes user reads `3d`,
//! `2d5h`, `47m` without thinking. That format is `kubectl`'s
//! `duration.HumanDuration`, and it is not the obvious one: the precision
//! *drops* as the age grows, and where it drops is chosen so the column stays
//! narrow. Reimplementing it exactly is worth more than inventing something
//! nicer, because the value of the column is that it is already familiar.

use chrono::{DateTime, Utc};

/// How long ago, in `kubectl`'s notation.
///
/// An absent timestamp is `<unknown>`, which is what `kubectl` prints for an
/// object whose `creationTimestamp` the apiserver did not send.
pub fn age(created: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match created {
        Some(created) => human_duration((now - created).num_seconds()),
        None => "<unknown>".to_string(),
    }
}

/// `kubectl`'s duration format, from a number of seconds.
///
/// The thresholds are its own, comment for comment:
/// under two minutes, seconds; under ten minutes, minutes and seconds; under
/// three hours, minutes; under eight hours, hours and minutes; under two
/// days, hours; under eight days, days and hours; under two years, days;
/// then years and days.
pub fn human_duration(seconds: i64) -> String {
    if seconds < -1 {
        return "<invalid>".to_string();
    }
    if seconds < 0 {
        return "0s".to_string();
    }
    if seconds < 60 * 2 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 10 {
        let remainder = seconds % 60;
        return match remainder {
            0 => format!("{minutes}m"),
            _ => format!("{minutes}m{remainder}s"),
        };
    }
    if minutes < 60 * 3 {
        return format!("{minutes}m");
    }
    let hours = seconds / 3600;
    if hours < 8 {
        let remainder = minutes % 60;
        return match remainder {
            0 => format!("{hours}h"),
            _ => format!("{hours}h{remainder}m"),
        };
    }
    if hours < 48 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if hours < 24 * 8 {
        let remainder = hours % 24;
        return match remainder {
            0 => format!("{days}d"),
            _ => format!("{days}d{remainder}h"),
        };
    }
    if hours < 24 * 365 * 2 {
        return format!("{days}d");
    }
    let years = days / 365;
    if hours < 24 * 365 * 8 {
        return format!("{years}y{}d", days % 365);
    }
    format!("{years}y")
}

/// A byte count, as Kubernetes writes one: binary units, no decimal point
/// under ten.
///
/// `metrics.k8s.io` answers in bytes and a node has gigabytes of them; the
/// number is only useful rounded.
pub fn bytes(count: u64) -> String {
    const UNITS: &[&str] = &["B", "Ki", "Mi", "Gi", "Ti", "Pi"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{count}B");
    }
    match value < 10.0 {
        true => format!("{value:.1}{}", UNITS[unit]),
        false => format!("{}{}", value.round() as u64, UNITS[unit]),
    }
}

/// A CPU figure in milli-cores, as `kubectl top` writes one.
pub fn cpu(milli: u64) -> String {
    format!("{milli}m")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn seconds_up_to_two_minutes() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(119), "119s");
    }

    #[test]
    fn minutes_and_seconds_up_to_ten_minutes() {
        assert_eq!(human_duration(120), "2m");
        assert_eq!(human_duration(150), "2m30s");
        assert_eq!(human_duration(599), "9m59s");
        assert_eq!(human_duration(600), "10m");
    }

    #[test]
    fn plain_minutes_up_to_three_hours() {
        assert_eq!(human_duration(60 * 47), "47m");
        assert_eq!(human_duration(60 * 179), "179m");
    }

    #[test]
    fn hours_and_minutes_up_to_eight_hours_then_plain_hours() {
        assert_eq!(human_duration(3600 * 3), "3h");
        assert_eq!(human_duration(3600 * 3 + 60 * 20), "3h20m");
        assert_eq!(human_duration(3600 * 10), "10h");
        assert_eq!(human_duration(3600 * 47), "47h");
    }

    #[test]
    fn days_and_hours_up_to_eight_days_then_plain_days() {
        assert_eq!(human_duration(3600 * 48), "2d");
        assert_eq!(human_duration(3600 * 53), "2d5h");
        assert_eq!(human_duration(3600 * 24 * 40), "40d");
    }

    #[test]
    fn years_when_something_has_been_up_that_long() {
        assert_eq!(human_duration(3600 * 24 * 800), "2y70d");
        assert_eq!(human_duration(3600 * 24 * 365 * 9), "9y");
    }

    #[test]
    fn a_clock_that_is_behind_reads_as_now_rather_than_as_a_negative_age() {
        assert_eq!(human_duration(-1), "0s");
        assert_eq!(human_duration(-500), "<invalid>");
    }

    #[test]
    fn an_object_with_no_creation_time_says_so() {
        let now = Utc::now();
        assert_eq!(age(None, now), "<unknown>");
        assert_eq!(age(Some(now - Duration::seconds(90)), now), "90s");
    }

    #[test]
    fn bytes_are_binary_and_rounded() {
        assert_eq!(bytes(512), "512B");
        assert_eq!(bytes(1024), "1.0Ki");
        assert_eq!(bytes(268_435_456), "256Mi");
        assert_eq!(bytes(6_012_338_176), "5.6Gi");
    }

    #[test]
    fn cpu_is_in_millicores_because_that_is_what_a_request_is_written_in() {
        assert_eq!(cpu(143), "143m");
        assert_eq!(cpu(0), "0m");
    }
}
