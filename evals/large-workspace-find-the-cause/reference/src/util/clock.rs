//! Splitting lengths of time into clock fields.

const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_HOUR: u64 = 60;
const SECONDS_PER_HOUR: u64 = SECONDS_PER_MINUTE * MINUTES_PER_HOUR;

/// Splits a number of seconds into hours, minutes and seconds.
#[must_use]
pub fn split_hms(seconds: u64) -> (u64, u64, u64) {
    (
        seconds / SECONDS_PER_HOUR,
        seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE,
        seconds % SECONDS_PER_MINUTE,
    )
}

/// Splits a number of minutes into whole hours and the minutes left over.
#[must_use]
pub fn split_hm(minutes: u64) -> (u64, u64) {
    (minutes / MINUTES_PER_HOUR, minutes % MINUTES_PER_HOUR)
}

/// `09:05` for a number of minutes after midnight. Times past the end of
/// the day wrap, so a session running to 01:30 the next morning reads as
/// such.
#[must_use]
pub fn time_of_day(minutes: u32) -> String {
    let minutes = minutes % (24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}
