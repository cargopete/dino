//! Lengths of time held in seconds, as the old timer stored them.

use crate::util::clock::split_hms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Seconds(pub u64);

impl Seconds {
    #[must_use]
    pub fn from_minutes(minutes: u64) -> Self {
        Self(minutes * 60)
    }

    #[must_use]
    pub fn whole_minutes(self) -> u64 {
        self.0 / 60
    }

    /// `01:30:00`
    #[must_use]
    pub fn stopwatch(self) -> String {
        let (hours, minutes, seconds) = split_hms(self.0);
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

/// `1h 30m` for a number of seconds. Leftover seconds are dropped.
#[must_use]
pub fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds / 60 % 60;
    match (hours, minutes) {
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h {minutes}m"),
    }
}
