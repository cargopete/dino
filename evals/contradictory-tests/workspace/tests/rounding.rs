use fixture::round_half;

#[test]
fn rounds_to_the_nearest_whole_number() {
    assert_eq!(round_half(1.2), 1);
    assert_eq!(round_half(1.7), 2);
}

#[test]
fn halves_round_to_even() {
    assert_eq!(round_half(0.5), 0);
    assert_eq!(round_half(2.5), 2);
    assert_eq!(round_half(-2.5), -2);
}
