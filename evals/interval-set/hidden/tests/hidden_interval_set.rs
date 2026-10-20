use std::ops::Range;

use fixture::IntervalSet;

fn ranges(set: &IntervalSet) -> Vec<Range<u32>> {
    set.iter().collect()
}

fn set_of(items: &[Range<u32>]) -> IntervalSet {
    let mut set = IntervalSet::new();
    for r in items {
        set.insert(r.clone());
    }
    set
}

#[test]
fn new_set_is_empty() {
    let set = IntervalSet::new();
    assert!(set.is_empty());
    assert_eq!(set.len(), 0);
    assert!(!set.contains(0));
    assert!(!set.contains(u32::MAX));
    assert!(ranges(&set).is_empty());
}

#[test]
fn adjacent_ranges_merge() {
    assert_eq!(ranges(&set_of(&[1..3, 3..5])), vec![1..5]);
    assert_eq!(ranges(&set_of(&[3..5, 1..3])), vec![1..5]);
    assert_eq!(ranges(&set_of(&[1..3, 4..5])), vec![1..3, 4..5]);
}

#[test]
fn insert_bridges_several_ranges() {
    let mut set = set_of(&[0..2, 4..6, 8..10, 12..14, 20..22]);
    set.insert(1..13);
    assert_eq!(ranges(&set), vec![0..14, 20..22]);
    assert_eq!(set.len(), 16);
    set.insert(14..20);
    assert_eq!(ranges(&set), vec![0..22]);
}

#[test]
fn insert_inside_an_existing_range_changes_nothing() {
    let mut set = set_of(&[0..100]);
    set.insert(10..20);
    assert_eq!(ranges(&set), vec![0..100]);
}

#[test]
fn insert_that_covers_everything_absorbs_it() {
    let mut set = set_of(&[5..6, 10..12, 30..31]);
    set.insert(0..50);
    assert_eq!(ranges(&set), vec![0..50]);
}

#[test]
fn remove_splits_a_range_in_two() {
    let mut set = set_of(&[0..10]);
    set.remove(3..5);
    assert_eq!(ranges(&set), vec![0..3, 5..10]);
    assert_eq!(set.len(), 8);
    assert!(set.contains(2));
    assert!(!set.contains(3));
    assert!(!set.contains(4));
    assert!(set.contains(5));
}

#[test]
fn remove_across_several_ranges_trims_both_ends() {
    let mut set = set_of(&[0..4, 6..8, 10..14]);
    set.remove(2..12);
    assert_eq!(ranges(&set), vec![0..2, 12..14]);
}

#[test]
fn remove_of_absent_points_changes_nothing() {
    let mut set = set_of(&[0..4, 10..14]);
    set.remove(4..10);
    assert_eq!(ranges(&set), vec![0..4, 10..14]);
    set.remove(20..30);
    assert_eq!(ranges(&set), vec![0..4, 10..14]);
}

#[test]
fn remove_everything_leaves_an_empty_set() {
    let mut set = set_of(&[0..4, 10..14]);
    set.remove(0..14);
    assert!(set.is_empty());
    assert_eq!(set.len(), 0);
}

#[test]
#[allow(clippy::reversed_empty_ranges)]
fn empty_ranges_are_no_ops() {
    let mut set = set_of(&[5..10]);
    set.insert(20..20);
    set.insert(30..25);
    set.remove(7..7);
    set.remove(9..6);
    set.insert(10..10);
    assert_eq!(ranges(&set), vec![5..10]);
    let mut empty = IntervalSet::new();
    empty.insert(3..3);
    assert!(empty.is_empty());
}

#[test]
fn the_whole_range_up_to_max() {
    let mut set = IntervalSet::new();
    set.insert(0..u32::MAX);
    assert_eq!(set.len(), u64::from(u32::MAX));
    assert!(set.contains(0));
    assert!(set.contains(u32::MAX - 1));
    assert!(!set.contains(u32::MAX));
    set.remove(7..8);
    assert_eq!(ranges(&set), vec![0..7, 8..u32::MAX]);
    assert_eq!(set.len(), u64::from(u32::MAX) - 1);
}

#[test]
fn ranges_touching_max() {
    let mut set = IntervalSet::new();
    set.insert(u32::MAX - 1..u32::MAX);
    assert!(set.contains(u32::MAX - 1));
    assert!(!set.contains(u32::MAX));
    assert!(!set.contains(u32::MAX - 2));
    set.insert(u32::MAX - 3..u32::MAX - 1);
    assert_eq!(ranges(&set), vec![u32::MAX - 3..u32::MAX]);
    set.insert(u32::MAX - 10..u32::MAX);
    assert_eq!(ranges(&set), vec![u32::MAX - 10..u32::MAX]);
    set.remove(u32::MAX - 1..u32::MAX);
    assert_eq!(ranges(&set), vec![u32::MAX - 10..u32::MAX - 1]);
    set.remove(0..u32::MAX);
    assert!(set.is_empty());
}

#[test]
fn ranges_touching_zero() {
    let mut set = set_of(&[0..1, 1..2]);
    assert_eq!(ranges(&set), vec![0..2]);
    assert!(set.contains(0));
    set.remove(0..1);
    assert_eq!(ranges(&set), vec![1..2]);
    assert!(!set.contains(0));
}

#[test]
fn many_disjoint_ranges_are_queried_quickly() {
    let mut set = IntervalSet::new();
    let n: u32 = 100_000;
    for i in 0..n {
        set.insert(i * 4..i * 4 + 2);
    }
    assert_eq!(set.len(), u64::from(n) * 2);
    let mut hits = 0u32;
    for p in 0..n * 4 {
        if set.contains(p) {
            hits += 1;
        }
    }
    assert_eq!(hits, n * 2);
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

const UNIVERSE: u32 = 64;

// The oracle stores one bool per point of a small universe placed at `base`.
fn canonical(points: &[bool], base: u32) -> Vec<Range<u32>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < points.len() {
        if points[i] {
            let start = i;
            while i < points.len() && points[i] {
                i += 1;
            }
            out.push(base + start as u32..base + i as u32);
        } else {
            i += 1;
        }
    }
    out
}

fn random_against_oracle(seed: u64, base: u32) {
    let mut rng = Lcg(seed);
    let mut set = IntervalSet::new();
    let mut points = vec![false; UNIVERSE as usize];
    for step in 0..4000 {
        let a = rng.below(UNIVERSE + 1);
        let b = rng.below(UNIVERSE + 1);
        let (lo, hi) = if rng.below(8) == 0 { (a.max(b), a.min(b)) } else { (a.min(b), a.max(b)) };
        let insert = rng.below(5) < 3;
        let range = base + lo..base + hi;
        if insert {
            set.insert(range.clone());
        } else {
            set.remove(range.clone());
        }
        for p in lo..hi {
            points[p as usize] = insert;
        }
        let expected = canonical(&points, base);
        assert_eq!(
            ranges(&set),
            expected,
            "seed {seed} base {base} step {step}: after {} {range:?}",
            if insert { "insert" } else { "remove" }
        );
        let count = points.iter().filter(|&&p| p).count() as u64;
        assert_eq!(set.len(), count, "seed {seed} base {base} step {step}: len");
        assert_eq!(set.is_empty(), count == 0);
        for p in 0..UNIVERSE {
            assert_eq!(
                set.contains(base + p),
                points[p as usize],
                "seed {seed} base {base} step {step}: contains({})",
                base + p
            );
        }
        if base > 0 {
            assert!(!set.contains(base - 1));
        }
    }
}

#[test]
fn random_operations_match_a_bitset_near_zero() {
    for seed in 1..=8 {
        random_against_oracle(seed, 0);
    }
}

#[test]
fn random_operations_match_a_bitset_near_max() {
    for seed in 11..=18 {
        random_against_oracle(seed, u32::MAX - UNIVERSE);
    }
}

#[test]
fn random_operations_match_a_bitset_mid_range() {
    for seed in 21..=24 {
        random_against_oracle(seed, 1_000_000);
    }
}
