/// Rounds to the nearest multiple of `step`, halves going up. A step of zero
/// or one leaves the value as it is.
#[must_use]
pub fn round_to_nearest(value: u64, step: u64) -> u64 {
    if step <= 1 {
        return value;
    }
    (value + step / 2) / step * step
}

/// Rounds up to the next multiple of `step`.
#[must_use]
pub fn round_up(value: u64, step: u64) -> u64 {
    if step <= 1 {
        return value;
    }
    value.div_ceil(step) * step
}
