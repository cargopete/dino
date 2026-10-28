//! A 95% Wilson score interval for a pass rate.
//!
//! Repeated trials are a count. The interval is what makes the count readable:
//! one success in one trial is not the same claim as seven in ten.

/// Normal quantile at 0.975, so the interval is two-sided 95%.
const Z_95: f64 = 1.959_963_984_540_054;

/// Lower and upper bounds in `0.0..=1.0`. No trials is the whole range: nothing
/// was measured.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn wilson(successes: u64, total: u64) -> (f64, f64) {
    if total == 0 {
        return (0.0, 1.0);
    }
    let n = total as f64;
    let proportion = successes as f64 / n;
    let z2 = Z_95 * Z_95;
    let denominator = 1.0 + z2 / n;
    let centre = (proportion + z2 / (2.0 * n)) / denominator;
    let spread = proportion * (1.0 - proportion) / n + z2 / (4.0 * n * n);
    let margin = Z_95 * spread.max(0.0).sqrt() / denominator;
    // The closed form sits on 0 or 1 when every trial agrees. Float error
    // leaves a value just inside the range, which would print as 100.0% only
    // by luck of rounding.
    (snap(centre - margin), snap(centre + margin))
}

fn snap(bound: f64) -> f64 {
    let bound = bound.clamp(0.0, 1.0);
    if bound < 1e-9 {
        0.0
    } else if 1.0 - bound < 1e-9 {
        1.0
    } else {
        bound
    }
}

/// The interval as it is printed beside a rate.
#[must_use]
pub fn wilson_interval(successes: u64, total: u64) -> String {
    let (low, high) = wilson(successes, total);
    format!("(95% Wilson {:.1}% to {:.1}%)", low * 100.0, high * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_trials_is_the_whole_range() {
        let (low, high) = wilson(0, 0);
        assert!(low < 1e-12, "{low}");
        assert!((high - 1.0).abs() < 1e-12, "{high}");
    }

    #[test]
    fn five_of_ten_is_the_usual_interval() {
        let (low, high) = wilson(5, 10);
        assert!((low - 0.236_593).abs() < 1e-5, "{low}");
        assert!((high - 0.763_407).abs() < 1e-5, "{high}");
        assert_eq!(wilson_interval(5, 10), "(95% Wilson 23.7% to 76.3%)");
    }

    #[test]
    fn none_and_all_sit_on_the_boundary() {
        let (low, high) = wilson(0, 10);
        assert!(low < 1e-12, "{low}");
        assert!((high - 0.277_533).abs() < 1e-5, "{high}");
        assert_eq!(wilson_interval(0, 10), "(95% Wilson 0.0% to 27.8%)");

        let (low, high) = wilson(10, 10);
        assert!((low - 0.722_467).abs() < 1e-5, "{low}");
        assert!((high - 1.0).abs() < 1e-12, "{high}");
        assert_eq!(wilson_interval(10, 10), "(95% Wilson 72.2% to 100.0%)");
    }

    #[test]
    fn one_trial_is_a_wide_claim() {
        assert_eq!(wilson_interval(1, 1), "(95% Wilson 20.7% to 100.0%)");
        assert_eq!(wilson_interval(0, 1), "(95% Wilson 0.0% to 79.3%)");
    }
}
