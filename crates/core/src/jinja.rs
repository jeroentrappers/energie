//! Numeric helpers that reproduce the Jinja/Python semantics the live templates rely on.
//!
//! The HA templates round with Jinja's `round` filter, which is Python's `round()` (ties to
//! even) for the default method and `math.floor` for `'floor'`. `|int` truncates toward zero.
//! Getting these exactly right matters: a 9.5 A target that rounds to 10 instead of 9 is a
//! different command on the wire.

/// Jinja `x | round(n)`: Python `round(x, n)`, which rounds the *exact* binary value half to
/// even. Rust's precision formatting does the same correctly rounded conversion, so it is used
/// here rather than `(x * 10^n).round() / 10^n`, whose multiplication adds its own rounding
/// error (wrong on ~1.5 % of 6-decimal inputs at 5 digits).
pub fn round(x: f64, n: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    if n < 0 {
        let m = 10f64.powi(-n);
        return (x / m).round_ties_even() * m;
    }
    let p = n as usize;
    format!("{x:.p$}").parse().expect("a formatted float parses")
}

/// Jinja `x | round(n, 'floor')`: HA's `forgiving_round` computes
/// `math.floor(x * 10**n) / 10**n`, with no guard against representation error, so
/// `2.3 | round(2, 'floor')` is 2.29 (2.3 × 100 = 229.99999999999997) and so is this.
pub fn round_floor(x: f64, n: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let m = 10f64.powi(n);
    (x * m).floor() / m
}

/// Jinja `x | int`: truncate toward zero.
pub fn int(x: f64) -> i64 {
    x.trunc() as i64
}

/// `[a, b] | max` with Python's NaN-free semantics for our inputs.
pub fn max2(a: f64, b: f64) -> f64 {
    if a >= b { a } else { b }
}

/// `[a, b] | min`.
pub fn min2(a: f64, b: f64) -> f64 {
    if a <= b { a } else { b }
}

/// `[lo, [hi, x] | min] | max`.
pub fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    max2(lo, min2(hi, x))
}

/// How HA prints a Python `float` in a template (`{{ x }}`) or a numeric state string: Python's
/// `repr`, i.e. the shortest round-trip digits, always with a decimal point (`20.0`), and
/// scientific notation below 1e-4 or from 1e16 on (`1e-05`, `1.5e+16`).
pub fn py_float(x: f64) -> String {
    if !x.is_finite() {
        return if x.is_nan() {
            "nan".into()
        } else if x > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let a = x.abs();
    if a != 0.0 && !(1e-4..1e16).contains(&a) {
        // Rust `{:e}` prints `1.5e-5` / `1e16`; Python wants a sign and two exponent digits.
        let s = format!("{x:e}");
        let (mant, exp) = s.split_once('e').expect("`{:e}` always has an exponent");
        let exp: i32 = exp.parse().expect("integer exponent");
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mant}e{sign}{:02}", exp.abs());
    }
    let s = format!("{x}");
    if s.contains('.') { s } else { format!("{s}.0") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_is_ties_to_even_like_python() {
        assert_eq!(round(0.5, 0), 0.0);
        assert_eq!(round(1.5, 0), 2.0);
        assert_eq!(round(2.5, 0), 2.0);
        assert_eq!(round(-2.5, 0), -2.0);
        assert_eq!(round(14.85, 1), 14.8); // 14.85 is 14.8499.. in binary, like Python
        assert_eq!(round(1.23456, 3), 1.235);
        assert!(round(f64::NAN, 1).is_nan());
        assert_eq!(round(1250.0, -2), 1200.0);
        assert_eq!(round(1350.0, -2), 1400.0);
    }

    /// Python `round()` works on the exact binary value: 2.675 is 2.67499999… so it gives 2.67,
    /// and 0.285 is 0.28499999… → 0.28; 1.000015 → 1.00002 (1.0000150000000000589…, above the half). These
    /// are CPython's results; a multiply-then-round implementation gets some wrong.
    #[test]
    fn round_uses_the_exact_binary_value_like_cpython() {
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(round(0.285, 2), 0.28);
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(0.375, 2), 0.38);
        assert_eq!(round(1.000015, 5), 1.00002);
        assert_eq!(round(-0.5, 0), -0.0);
    }

    /// HA `forgiving_round(x, n, 'floor')` is `math.floor(x * 10**n) / 10**n`; it keeps the
    /// representation error of the product (CPython: `math.floor(2.3 * 100) / 100 == 2.29`).
    /// The modulator's `avail` hits it: (2300 / 230 + 920 / 230) × 1.4 = 19.599999999999998,
    /// which HA floors to 19.5, not 19.6.
    #[test]
    fn round_floor_floors_the_product_like_ha() {
        assert_eq!(round_floor(16.99, 1), 16.9);
        assert_eq!(round_floor(1.7, 1), 1.7);
        assert_eq!(round_floor(2.3, 2), 2.29);
        assert_eq!(round_floor((2300.0 / 230.0 + 920.0 / 230.0) * 1.4, 1), 19.5);
        assert_eq!(round_floor(-0.05, 1), -0.1);
        assert_eq!(round_floor(5.999, 0), 5.0);
        assert!(round_floor(f64::INFINITY, 1).is_infinite());
    }

    #[test]
    fn int_truncates_toward_zero() {
        assert_eq!(int(9.9), 9);
        assert_eq!(int(-9.9), -9);
    }

    #[test]
    fn min_max_clamp() {
        assert_eq!(max2(1.0, 2.0), 2.0);
        assert_eq!(max2(3.0, 2.0), 3.0);
        assert_eq!(min2(1.0, 2.0), 1.0);
        assert_eq!(min2(3.0, 2.0), 2.0);
        assert_eq!(clamp(7.0, -5.0, 5.0), 5.0);
        assert_eq!(clamp(-7.0, -5.0, 5.0), -5.0);
        assert_eq!(clamp(1.0, -5.0, 5.0), 1.0);
    }

    /// `{{ x }}` of a float prints Python's `repr`: the digits HA stores, `.0` on whole
    /// numbers, scientific notation outside [1e-4, 1e16). Reference values from CPython.
    #[test]
    fn py_float_prints_like_python_repr() {
        assert_eq!(py_float(3142.0), "3142.0");
        assert_eq!(py_float(20.0), "20.0");
        assert_eq!(py_float(3.1), "3.1");
        assert_eq!(py_float(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(py_float(-0.0), "-0.0");
        assert_eq!(py_float(0.0), "0.0");
        assert_eq!(py_float(123456789012345.6), "123456789012345.6");
        assert_eq!(py_float(1e-5), "1e-05");
        assert_eq!(py_float(1.5e-5), "1.5e-05");
        assert_eq!(py_float(1e16), "1e+16");
        assert_eq!(py_float(f64::NAN), "nan");
        assert_eq!(py_float(f64::INFINITY), "inf");
        assert_eq!(py_float(f64::NEG_INFINITY), "-inf");
    }
}
