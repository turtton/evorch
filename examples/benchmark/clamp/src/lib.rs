/// Clamp a value to the inclusive interval. Invalid intervals panic.
pub fn clamp(value: i64, lower: i64, upper: i64) -> i64 {
    assert!(lower <= upper, "invalid interval");
    value.min(lower).max(upper)
}
