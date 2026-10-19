use crate::fmt::units;

/// How a report writes a length of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationStyle {
    /// `1h 30m`
    Compact,
    /// `1:30`
    Clock,
    /// `1.50h`
    Decimal,
}

impl DurationStyle {
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "compact" => Some(Self::Compact),
            "clock" => Some(Self::Clock),
            "decimal" => Some(Self::Decimal),
            _ => None,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Clock => "clock",
            Self::Decimal => "decimal",
        }
    }

    #[must_use]
    pub fn render(self, minutes: u64) -> String {
        match self {
            Self::Compact => units::compact(minutes),
            Self::Clock => units::clock(minutes),
            Self::Decimal => units::decimal(minutes),
        }
    }
}

/// Spelled out for prose, as in `1 hour 30 minutes` or `45 minutes`.
#[must_use]
pub fn format_duration_long(minutes: u64) -> String {
    let hours = minutes / 60;
    let rest = minutes % 60;
    let hours_part = match hours {
        0 => None,
        1 => Some("1 hour".to_owned()),
        n => Some(format!("{n} hours")),
    };
    let minutes_part = match rest {
        0 if hours > 0 => None,
        1 => Some("1 minute".to_owned()),
        n => Some(format!("{n} minutes")),
    };
    match (hours_part, minutes_part) {
        (Some(hours), Some(minutes)) => format!("{hours} {minutes}"),
        (Some(hours), None) => hours,
        (None, Some(minutes)) => minutes,
        (None, None) => "0 minutes".to_owned(),
    }
}
