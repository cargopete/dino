use crate::util::num::group_thousands;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    Eur,
    Gbp,
    Usd,
    Chf,
}

impl Currency {
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code.to_ascii_uppercase().as_str() {
            "EUR" => Some(Self::Eur),
            "GBP" => Some(Self::Gbp),
            "USD" => Some(Self::Usd),
            "CHF" => Some(Self::Chf),
            _ => None,
        }
    }

    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Eur => "EUR",
            Self::Gbp => "GBP",
            Self::Usd => "USD",
            Self::Chf => "CHF",
        }
    }

    /// The prefix written before an amount. Francs have no symbol in common
    /// use, so they take the code and a space.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Eur => "€",
            Self::Gbp => "£",
            Self::Usd => "$",
            Self::Chf => "CHF ",
        }
    }
}

/// `€1,234.50`
#[must_use]
pub fn format_money(cents: u64, currency: Currency) -> String {
    format!("{}{}", currency.prefix(), plain_amount(cents, true))
}

/// `1234.50`, or `1,234.50` when grouped.
#[must_use]
pub fn plain_amount(cents: u64, grouped: bool) -> String {
    let units = cents / 100;
    let units = if grouped {
        group_thousands(units)
    } else {
        units.to_string()
    };
    format!("{units}.{:02}", cents % 100)
}
