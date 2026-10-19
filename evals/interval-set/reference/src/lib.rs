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
        if range.start >= range.end {
            return;
        }
        let (mut start, mut end) = (range.start, range.end);
        if let Some((&s, &e)) = self.ranges.range(..start).next_back() {
            if e >= start {
                start = s;
                end = end.max(e);
            }
        }
        let absorbed: Vec<u32> = self.ranges.range(start..=end).map(|(&s, _)| s).collect();
        for s in absorbed {
            if let Some(e) = self.ranges.remove(&s) {
                end = end.max(e);
            }
        }
        self.ranges.insert(start, end);
    }

    /// Removes every point in `range` from the set.
    pub fn remove(&mut self, range: Range<u32>) {
        if range.start >= range.end {
            return;
        }
        if let Some((&s, &e)) = self.ranges.range(..range.start).next_back() {
            if e > range.start {
                self.ranges.insert(s, range.start);
                if e > range.end {
                    self.ranges.insert(range.end, e);
                }
            }
        }
        let inside: Vec<(u32, u32)> = self
            .ranges
            .range(range.start..range.end)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in inside {
            self.ranges.remove(&s);
            if e > range.end {
                self.ranges.insert(range.end, e);
            }
        }
    }

    /// Whether `point` is in the set.
    #[must_use]
    pub fn contains(&self, point: u32) -> bool {
        self.ranges
            .range(..=point)
            .next_back()
            .is_some_and(|(_, &end)| point < end)
    }

    /// The canonical ranges, in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = Range<u32>> + '_ {
        self.ranges.iter().map(|(&start, &end)| start..end)
    }

    /// The number of points in the set.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.ranges.iter().map(|(&s, &e)| u64::from(e - s)).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}
