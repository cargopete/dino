//! What a run cost, once somebody names the rate.
//!
//! The catalogue's list prices are for sessions; an eval is priced only at the
//! rate its invocation names. A hosted model is list price per million tokens,
//! a rented GPU is dollars an hour, and a machine on a desk is watts. The figure is reported beside the result. It never decides pass or fail.

use dino_protocol::TokenUsage;
use serde::Deserialize;
use serde::Serialize;

/// How a run is priced. One of these, never a blend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunRate {
    /// USD per million tokens. Cached tokens are inside `input_tokens` and are
    /// priced as input: a cache discount is a different rate, and this one
    /// does not know it. Reasoning tokens are inside `output_tokens`.
    List {
        input_per_million: f64,
        output_per_million: f64,
    },
    /// The machine's draw. Wall time becomes estimated watt-hours.
    Watts(f64),
    /// GPU hire, USD per hour of wall time.
    DollarsPerHour(f64),
}

/// Which rate produced [`RunCost::amount`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    List,
    Hire,
    Watts,
}

/// A run's cost in one unit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RunCost {
    pub basis: CostBasis,
    pub amount: f64,
}

impl RunCost {
    /// The phrase printed beside a case and under the corpus.
    #[must_use]
    pub fn phrase(&self) -> String {
        match self.basis {
            CostBasis::List => format!("{} list", format_usd(self.amount)),
            CostBasis::Hire => format!("{} hire", format_usd(self.amount)),
            CostBasis::Watts => format!("{} Wh estimated", format_wh(self.amount)),
        }
    }
}

/// Reads the four optional flags into one rate.
///
/// # Errors
///
/// Returns a sentence when the flags contradict each other or a number is not
/// a usable rate.
pub fn parse_rate(
    input_per_million: Option<f64>,
    output_per_million: Option<f64>,
    watts: Option<f64>,
    dollars_per_hour: Option<f64>,
) -> Result<Option<RunRate>, &'static str> {
    let list = match (input_per_million, output_per_million) {
        (Some(input), Some(output)) => Some((input, output)),
        (None, None) => None,
        _ => return Err("token prices are set together"),
    };
    let chosen = [list.is_some(), watts.is_some(), dollars_per_hour.is_some()]
        .into_iter()
        .filter(|set| *set)
        .count();
    if chosen > 1 {
        return Err("price a run by list tokens, by watts, or by the hour");
    }
    if let Some((input, output)) = list {
        if !usable_money(input) || !usable_money(output) {
            return Err("a token price must be a non-negative number");
        }
        return Ok(Some(RunRate::List {
            input_per_million: input,
            output_per_million: output,
        }));
    }
    if let Some(watts) = watts {
        if !watts.is_finite() || watts <= 0.0 {
            return Err("watts must be a number greater than zero");
        }
        return Ok(Some(RunRate::Watts(watts)));
    }
    if let Some(per_hour) = dollars_per_hour {
        if !usable_money(per_hour) {
            return Err("dollars per hour must be a non-negative number");
        }
        return Ok(Some(RunRate::DollarsPerHour(per_hour)));
    }
    Ok(None)
}

fn usable_money(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

/// Prices one run. `duration_ms` is wall time.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn cost_of(tokens: &TokenUsage, duration_ms: u128, rate: RunRate) -> RunCost {
    let hours = duration_ms as f64 / 3_600_000.0;
    match rate {
        RunRate::List {
            input_per_million,
            output_per_million,
        } => RunCost {
            basis: CostBasis::List,
            amount: tokens.input_tokens as f64 * input_per_million / 1_000_000.0
                + tokens.output_tokens as f64 * output_per_million / 1_000_000.0,
        },
        RunRate::Watts(watts) => RunCost {
            basis: CostBasis::Watts,
            amount: watts * hours,
        },
        RunRate::DollarsPerHour(per_hour) => RunCost {
            basis: CostBasis::Hire,
            amount: per_hour * hours,
        },
    }
}

/// The sum, when every cost was priced the same way.
#[must_use]
pub fn total_cost<'a>(costs: impl IntoIterator<Item = &'a RunCost>) -> Option<RunCost> {
    let mut costs = costs.into_iter();
    let first = *costs.next()?;
    let mut amount = first.amount;
    for cost in costs {
        if cost.basis != first.basis {
            return None;
        }
        amount += cost.amount;
    }
    Some(RunCost {
        basis: first.basis,
        amount,
    })
}

fn format_usd(amount: f64) -> String {
    if amount >= 1.0 {
        format!("${amount:.2}")
    } else {
        format!("${amount:.4}")
    }
}

fn format_wh(amount: f64) -> String {
    if amount >= 10.0 {
        format!("{amount:.1}")
    } else {
        format!("{amount:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: output,
            reasoning_tokens: 0,
        }
    }

    #[test]
    fn list_price_is_input_plus_output_and_cache_is_not_added_again() {
        let rate = RunRate::List {
            input_per_million: 3.0,
            output_per_million: 15.0,
        };
        let mut usage = tokens(1_000_000, 1_000_000);
        usage.cached_input_tokens = 800_000;
        usage.reasoning_tokens = 400_000;
        let cost = cost_of(&usage, 0, rate);
        assert!((cost.amount - 18.0).abs() < 1e-9, "{}", cost.amount);
        assert_eq!(cost.phrase(), "$18.00 list");
    }

    #[test]
    fn a_cheap_case_keeps_its_fractions_of_a_cent() {
        let cost = cost_of(
            &tokens(1_000, 0),
            0,
            RunRate::List {
                input_per_million: 3.0,
                output_per_million: 15.0,
            },
        );
        assert_eq!(cost.phrase(), "$0.0030 list");
    }

    #[test]
    fn watts_follow_the_wall_clock() {
        let cost = cost_of(&tokens(0, 0), 3_600_000, RunRate::Watts(400.0));
        assert!((cost.amount - 400.0).abs() < 1e-9, "{}", cost.amount);
        assert_eq!(cost.phrase(), "400.0 Wh estimated");
    }

    #[test]
    fn hire_is_the_hourly_rate_times_the_wall_clock() {
        let cost = cost_of(&tokens(0, 0), 1_800_000, RunRate::DollarsPerHour(1.81));
        assert!((cost.amount - 0.905).abs() < 1e-9, "{}", cost.amount);
        assert_eq!(cost.phrase(), "$0.9050 hire");
    }

    #[test]
    fn the_flags_are_one_rate_or_none() {
        assert!(parse_rate(None, None, None, None).unwrap().is_none());
        assert!(parse_rate(Some(1.0), None, None, None).is_err());
        assert!(parse_rate(Some(1.0), Some(2.0), Some(400.0), None).is_err());
        assert!(parse_rate(Some(-1.0), Some(2.0), None, None).is_err());
        assert!(parse_rate(None, None, Some(0.0), None).is_err());
        assert!(parse_rate(None, None, Some(f64::INFINITY), None).is_err());
        assert!(parse_rate(None, None, None, Some(-0.5)).is_err());
        assert!(matches!(
            parse_rate(None, None, Some(250.0), None).unwrap(),
            Some(RunRate::Watts(watts)) if (watts - 250.0).abs() < 1e-9
        ));
    }

    #[test]
    fn a_total_needs_one_basis() {
        let list = cost_of(
            &tokens(1_000_000, 0),
            0,
            RunRate::List {
                input_per_million: 2.0,
                output_per_million: 0.0,
            },
        );
        let again = cost_of(
            &tokens(500_000, 0),
            0,
            RunRate::List {
                input_per_million: 2.0,
                output_per_million: 0.0,
            },
        );
        let hire = cost_of(
            &TokenUsage::default(),
            3_600_000,
            RunRate::DollarsPerHour(1.0),
        );
        let summed = total_cost([&list, &again]).unwrap();
        assert!((summed.amount - 3.0).abs() < 1e-9, "{}", summed.amount);
        assert!(total_cost([&list, &hire]).is_none());
        assert!(total_cost(std::iter::empty()).is_none());
    }
}
