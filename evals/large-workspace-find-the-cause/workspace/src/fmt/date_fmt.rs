use crate::model::date::{Date, Weekday};
use crate::model::period::Period;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[must_use]
pub fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
        Weekday::Sunday => "Sun",
    }
}

/// `Mon 02 Mar`
#[must_use]
pub fn short(date: Date) -> String {
    format!(
        "{} {:02} {}",
        weekday_name(date.weekday()),
        date.day,
        MONTHS[(date.month - 1) as usize]
    )
}

/// `2026-03-02 to 2026-03-08`, or the one date when the period is a day.
#[must_use]
pub fn range(period: Period) -> String {
    if period.start == period.end {
        period.start.to_string()
    } else {
        format!("{} to {}", period.start, period.end)
    }
}
