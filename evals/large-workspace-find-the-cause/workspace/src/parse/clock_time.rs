/// Parses `HH:MM` on a 24-hour clock into minutes after midnight. `24:00`
/// is allowed as the end of a day.
pub fn parse(text: &str) -> Result<u32, String> {
    let (hours, minutes) = text
        .split_once(':')
        .ok_or_else(|| format!("expected a time like 09:30, found `{text}`"))?;
    if hours.is_empty() || hours.len() > 2 || minutes.len() != 2 {
        return Err(format!("expected a time like 09:30, found `{text}`"));
    }
    let hours: u32 = hours
        .parse()
        .map_err(|_| format!("bad hour in `{text}`"))?;
    let minutes: u32 = minutes
        .parse()
        .map_err(|_| format!("bad minute in `{text}`"))?;
    if minutes >= 60 || hours > 24 || (hours == 24 && minutes > 0) {
        return Err(format!("`{text}` is not a time of day"));
    }
    Ok(hours * 60 + minutes)
}
