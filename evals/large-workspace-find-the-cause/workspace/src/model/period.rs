use crate::model::date::Date;

/// An inclusive range of dates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    pub start: Date,
    pub end: Date,
}

impl Period {
    #[must_use]
    pub fn new(start: Date, end: Date) -> Option<Self> {
        (start <= end).then_some(Self { start, end })
    }

    /// The Monday-to-Sunday ISO week named like `2026-W10`.
    #[must_use]
    pub fn iso_week(text: &str) -> Option<Self> {
        let (year, week) = text.split_once("-W")?;
        let year: i32 = year.parse().ok()?;
        let week: i64 = week.parse().ok()?;
        if !(1..=weeks_in_year(year)).contains(&week) {
            return None;
        }
        let monday = week_one_monday(year).add_days((week - 1) * 7);
        Some(Self {
            start: monday,
            end: monday.add_days(6),
        })
    }

    #[must_use]
    pub fn contains(&self, date: Date) -> bool {
        self.start <= date && date <= self.end
    }

    #[must_use]
    pub fn days(&self) -> i64 {
        self.end.to_days() - self.start.to_days() + 1
    }
}

/// The Monday of the week holding 4 January, which ISO 8601 calls week one.
fn week_one_monday(year: i32) -> Date {
    let fourth = Date {
        year,
        month: 1,
        day: 4,
    };
    fourth.add_days(-fourth.weekday_index())
}

fn weeks_in_year(year: i32) -> i64 {
    let this = week_one_monday(year).to_days();
    let next = week_one_monday(year + 1).to_days();
    (next - this) / 7
}
