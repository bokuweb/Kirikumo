//! Kubernetes quantities.
//!
//! `128Mi`, `1.5`, `100m`, `142893652n`, `1e3` are all the same field type,
//! and every number worth showing — a node's capacity, a container's request,
//! what `metrics.k8s.io` reports — arrives in it. There are two suffix
//! families, binary (`Ki`, `Mi`, `Gi`, …) and decimal (`n`, `u`, `m`, ``,
//! `k`, `M`, `G`, …), and the decimal ones go *below* one, which is what
//! makes CPU work: `100m` is a tenth of a core and `142893652n` is a seventh
//! of one.
//!
//! Parsing returns `f64` in base units — cores for CPU, bytes for memory —
//! because a node's memory in bytes overflows nothing but a `u32` and a
//! millicore count is not exact in any integer.

/// Parse a quantity into base units.
///
/// `None` when the text is not a quantity; callers draw an empty cell rather
/// than a zero, because a missing limit and a limit of zero are different
/// things.
pub fn parse(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // Exponent form: `1e3`, `1.5e-3`. The `e` is not a suffix, so it has to
    // be caught before the suffix table, which would otherwise read the `3`
    // as part of a number it has already stopped reading.
    if let Some((mantissa, exponent)) = split_exponent(text) {
        let mantissa: f64 = mantissa.parse().ok()?;
        let exponent: i32 = exponent.parse().ok()?;
        return Some(mantissa * 10f64.powi(exponent));
    }
    let digits_end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(digits_end);
    let number: f64 = number.parse().ok()?;
    Some(number * multiplier(suffix)?)
}

/// Split `1.5e-3` into its mantissa and exponent, if it is in that form.
fn split_exponent(text: &str) -> Option<(&str, &str)> {
    let index = text.find(['e', 'E'])?;
    let (mantissa, rest) = text.split_at(index);
    let exponent = &rest[1..];
    // `1Ei` is an exabyte, not one times ten to the `i`.
    (!exponent.is_empty()
        && exponent
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == '+'))
    .then_some((mantissa, exponent))
}

/// What a suffix multiplies by.
fn multiplier(suffix: &str) -> Option<f64> {
    Some(match suffix {
        "" => 1.0,
        // Decimal, below one. `m` is milli, and there is no `M` for milli:
        // capital `M` is mega, which is why this table is case-sensitive.
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        // Decimal, above one.
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        // Binary.
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        _ => return None,
    })
}

/// Parse a CPU quantity into milli-cores, the unit requests are quoted in.
pub fn cpu_milli(text: &str) -> Option<u64> {
    parse(text).map(|cores| (cores * 1000.0).round().max(0.0) as u64)
}

/// Parse a memory quantity into bytes.
pub fn bytes(text: &str) -> Option<u64> {
    parse(text).map(|bytes| bytes.round().max(0.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_number_is_its_own_value() {
        assert_eq!(parse("1"), Some(1.0));
        assert_eq!(parse("1.5"), Some(1.5));
        assert_eq!(parse(" 2 "), Some(2.0));
    }

    #[test]
    fn milli_is_lowercase_and_mega_is_not() {
        assert_eq!(cpu_milli("100m"), Some(100));
        assert_eq!(cpu_milli("1"), Some(1000));
        assert_eq!(cpu_milli("1.5"), Some(1500));
        assert_eq!(bytes("1M"), Some(1_000_000));
    }

    #[test]
    fn metrics_report_cpu_in_nanocores() {
        // What `metrics.k8s.io` actually sends for a busy container.
        assert_eq!(cpu_milli("142893652n"), Some(143));
        assert_eq!(cpu_milli("500u"), Some(1));
    }

    #[test]
    fn binary_suffixes_are_powers_of_two() {
        assert_eq!(bytes("1Ki"), Some(1024));
        assert_eq!(bytes("128Mi"), Some(134_217_728));
        assert_eq!(bytes("1Gi"), Some(1_073_741_824));
    }

    #[test]
    fn exponent_form_is_a_quantity_too() {
        assert_eq!(parse("1e3"), Some(1000.0));
        assert_eq!(parse("1.5e-3"), Some(0.0015));
    }

    #[test]
    fn an_exabyte_is_not_an_exponent() {
        assert_eq!(bytes("1Ei"), Some(1024u64.pow(6)));
        assert_eq!(parse("1E"), Some(1e18));
    }

    #[test]
    fn something_that_is_not_a_quantity_is_none_and_not_zero() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("lots"), None);
        assert_eq!(parse("10Zi"), None);
        assert_eq!(cpu_milli("unbounded"), None);
    }
}
