use crate::error::Error;

pub fn boolean(key: &str, value: &str) -> Result<bool, Error> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(Error::Value {
            key: key.to_owned(),
            message: format!("expected a boolean, found `{other}`"),
        }),
    }
}

pub fn whole(key: &str, value: &str) -> Result<u64, Error> {
    value.parse().map_err(|_| Error::Value {
        key: key.to_owned(),
        message: format!("expected a whole number, found `{value}`"),
    })
}

/// A decimal amount such as `95` or `95.5` or `95.50`, in hundredths.
pub fn hundredths(key: &str, value: &str) -> Result<u64, Error> {
    let invalid = || Error::Value {
        key: key.to_owned(),
        message: format!("expected an amount like 80.00, found `{value}`"),
    };
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (value, ""),
    };
    if whole.is_empty() || fraction.len() > 2 {
        return Err(invalid());
    }
    let whole: u64 = whole.parse().map_err(|_| invalid())?;
    let fraction: u64 = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().map_err(|_| invalid())? * 10,
        _ => fraction.parse().map_err(|_| invalid())?,
    };
    Ok(whole * 100 + fraction)
}

pub fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}
