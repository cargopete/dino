//! A set of `u32` points stored as half-open ranges.

use std::collections::BTreeMap;
use std::ops::Range;

/// A set of `u32` points, stored as sorted, disjoint, non-adjacent half-open
/// ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntervalSet {
    ranges: BTreeMap<u32, u32>,
}

impl IntervalSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds every point in `range` to the set.
    pub fn insert(&mut self, range: Range<u32>) {
        todo!("insert {range:?}")
    }

    /// Removes every point in `range` from the set.
    pub fn remove(&mut self, range: Range<u32>) {
        todo!("remove {range:?}")
    }

    /// Whether `point` is in the set.
    #[must_use]
    pub fn contains(&self, point: u32) -> bool {
        todo!("contains {point}")
    }

    /// The canonical ranges, in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = Range<u32>> + '_ {
        self.ranges.iter().map(|(&start, &end)| start..end)
    }

    /// The number of points in the set.
    #[must_use]
    pub fn len(&self) -> u64 {
        todo!()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}
