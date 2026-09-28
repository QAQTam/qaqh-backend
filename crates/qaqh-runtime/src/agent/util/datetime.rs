//! Date/time helpers (split from the former util monolith; behavior preserved,
//! including the historical UTC+8 bias — to be revisited with tests).

/// Convert epoch seconds to human-readable UTC date.
pub(crate) fn epoch_to_date(epoch_secs: u64) -> String {
    use qaqh_types::platform::civil_from_days;
    let total_days = (epoch_secs / 86400) as i64;
    let (y, m, d) = civil_from_days(total_days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Return today's date as "YYYY-MM-DD" (UTC+8).
pub(crate) fn chrono_local_date() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let (y, m, d) = qaqh_types::platform::civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Return current time as "UTC+8 YYYY-MM-DD HH:MM".
pub(crate) fn chrono_local_datetime() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let (y, m, d) = qaqh_types::platform::civil_from_days(days as i64);
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    format!("UTC+8 {y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}")
}
