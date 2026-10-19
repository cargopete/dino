use crate::util::clock::split_hm;

/// `45m`, `2h` or `1h 30m`.
#[must_use]
pub fn compact(minutes: u64) -> String {
    match split_hm(minutes) {
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h {minutes}m"),
    }
}

/// `1:30`, as on a stopwatch that counts hours.
#[must_use]
pub fn clock(minutes: u64) -> String {
    let (hours, minutes) = split_hm(minutes);
    format!("{hours}:{minutes:02}")
}

/// Hours to two decimal places, rounded half up: `1.50h`.
#[must_use]
pub fn decimal(minutes: u64) -> String {
    let hundredths = (minutes * 100 + 30) / 60;
    format!("{}.{:02}h", hundredths / 100, hundredths % 100)
}
