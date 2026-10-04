use benchmark_clamp::clamp;

#[test]
fn clamps_values_and_preserves_values_inside_the_interval() {
    for (value, lower, upper, expected) in [
        (-5, 0, 10, 0),
        (5, 0, 10, 5),
        (15, 0, 10, 10),
        (0, 0, 10, 0),
        (10, 0, 10, 10),
        (-7, -10, -3, -7),
        (i64::MIN, i64::MIN, i64::MAX, i64::MIN),
        (i64::MAX, i64::MIN, i64::MAX, i64::MAX),
        (9, 4, 4, 4),
    ] {
        assert_eq!(clamp(value, lower, upper), expected);
    }
}

#[test]
#[should_panic]
fn rejects_reversed_bounds() {
    clamp(5, 10, 0);
}
