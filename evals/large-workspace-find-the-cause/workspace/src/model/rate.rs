/// An hourly rate in hundredths of the currency unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rate {
    cents_per_hour: u64,
}

impl Rate {
    #[must_use]
    pub fn per_hour(cents: u64) -> Self {
        Self {
            cents_per_hour: cents,
        }
    }

    #[must_use]
    pub fn cents_per_hour(self) -> u64 {
        self.cents_per_hour
    }

    /// The charge for some minutes of work, rounded half up to the cent.
    #[must_use]
    pub fn charge(self, minutes: u64) -> u64 {
        (minutes * self.cents_per_hour + 30) / 60
    }
}
