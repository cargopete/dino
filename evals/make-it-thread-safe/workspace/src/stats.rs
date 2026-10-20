#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub submitted: u64,
    pub completed: u64,
    pub cancelled: u64,
    /// Callback invocations, summed over every subscriber.
    pub delivered: u64,
}
