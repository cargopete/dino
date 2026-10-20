use fixture::IntervalSet;

#[test]
fn inserts_and_finds_points() {
    let mut set = IntervalSet::new();
    set.insert(10..20);
    assert!(set.contains(10));
    assert!(set.contains(19));
    assert!(!set.contains(20));
    assert!(!set.contains(9));
    assert_eq!(set.len(), 10);
}

#[test]
fn overlapping_inserts_merge() {
    let mut set = IntervalSet::new();
    set.insert(1..5);
    set.insert(4..8);
    assert_eq!(set.iter().collect::<Vec<_>>(), vec![1..8]);
}

#[test]
fn removes_from_the_end() {
    let mut set = IntervalSet::new();
    set.insert(0..10);
    set.remove(5..10);
    assert_eq!(set.iter().collect::<Vec<_>>(), vec![0..5]);
    assert_eq!(set.len(), 5);
}
