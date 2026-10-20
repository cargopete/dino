/// The 0.x duration format. It only ever showed the minutes field, which
/// the old report printed beside a separate hours column.
#[must_use]
pub fn format_duration(minutes: u32) -> String {
    format!("{}m", minutes % 60)
}

/// The 0.x hours column.
#[must_use]
pub fn format_hours(minutes: u32) -> String {
    format!("{}h", minutes / 60)
}

/// The 0.x money format: whole units only, no symbol.
#[must_use]
pub fn format_amount(cents: u64) -> String {
    (cents / 100).to_string()
}
