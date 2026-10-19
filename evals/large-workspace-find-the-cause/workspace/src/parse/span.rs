use crate::parse::clock_time;

const MINUTES_PER_DAY: u32 = 24 * 60;

/// Reads `09:00-10:30` or `09:00+1h30m` into a start and a length in
/// minutes. A range whose end is before its start runs past midnight.
pub fn parse(text: &str) -> Result<(u32, u32), String> {
    if let Some((start, end)) = text.split_once('-') {
        let start = clock_time::parse(start)?;
        let end = clock_time::parse(end)?;
        let minutes = if end >= start {
            end - start
        } else {
            end + MINUTES_PER_DAY - start
        };
        return Ok((start, minutes));
    }
    if let Some((start, length)) = text.split_once('+') {
        return Ok((clock_time::parse(start)?, parse_length(length)?));
    }
    Err(format!("expected a span like 09:00-10:30 or 09:00+45m, found `{text}`"))
}

/// Reads a length such as `45m`, `2h` or `1h30m`.
pub fn parse_length(text: &str) -> Result<u32, String> {
    let mut total = 0u32;
    let mut digits = String::new();
    let mut seen_unit = false;
    for character in text.chars() {
        match character {
            '0'..='9' => digits.push(character),
            'h' | 'm' => {
                let amount: u32 = digits
                    .parse()
                    .map_err(|_| format!("missing number before `{character}` in `{text}`"))?;
                total += if character == 'h' { amount * 60 } else { amount };
                digits.clear();
                seen_unit = true;
            }
            other => return Err(format!("unexpected `{other}` in length `{text}`")),
        }
    }
    if !digits.is_empty() || !seen_unit {
        return Err(format!("length `{text}` needs a unit, h or m"));
    }
    Ok(total)
}
