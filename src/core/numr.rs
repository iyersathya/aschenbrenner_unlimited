//! Numeric helper: stable decimal rounding via formatting (round-half-to-even on
//! the exact binary value). Shared so ported math reads the same as the sibling
//! sleeves (qqq-trader / options-trader-rust).
pub fn pyround(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", ndigits, x).parse().unwrap_or(x)
}
