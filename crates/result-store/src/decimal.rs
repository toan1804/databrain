//! Exact decimals (transported as text) for sort, filters and summaries.
//!
//! `NUMBER(38)` ids past 2^53 are not f64 values: `9007199254740993` and
//! `9007199254740992` are one f64. Values are parsed into digits and a
//! decimal exponent and compared through an order-preserving byte key, so
//! any precision and scale compares exactly (`1.50` equals `1.5`).

use std::cmp::Ordering;

use databrain_connector_core::arrow::array::{Array, BinaryArray};
use databrain_connector_core::arrow::error::ArrowError;

use crate::display;

/// A parsed number. Finite values are `0.<digits> × 10^exp`, digits without
/// leading or trailing zeros; zero has no digits (and no sign).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Num {
    NegInf,
    Finite {
        neg: bool,
        digits: Vec<u8>,
        exp: i64,
    },
    PosInf,
    /// Postgres `numeric` NaN; sorts above everything, as in Postgres.
    NaN,
}

/// Exponents beyond this are clamped (no database has them).
const MAX_EXP: i64 = 1 << 40;

impl Num {
    /// Parse decimal text: `-12.50`, `+1e3`, `.5`, `NaN`, `Infinity`.
    pub(crate) fn parse(s: &str) -> Option<Num> {
        let s = s.trim();
        let (neg, body) = match s.as_bytes().first()? {
            b'-' => (true, &s[1..]),
            b'+' => (false, &s[1..]),
            _ => (false, s),
        };
        if body.eq_ignore_ascii_case("nan") {
            return (!s.starts_with(['-', '+'])).then_some(Num::NaN);
        }
        if body.eq_ignore_ascii_case("inf") || body.eq_ignore_ascii_case("infinity") {
            return Some(if neg { Num::NegInf } else { Num::PosInf });
        }
        let (mantissa, e) = match body.find(['e', 'E']) {
            Some(i) => (&body[..i], Some(&body[i + 1..])),
            None => (body, None),
        };
        let (int, frac) = match mantissa.split_once('.') {
            Some((i, f)) => (i, f),
            None => (mantissa, ""),
        };
        if int.is_empty() && frac.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut exp = match e {
            Some(e) => parse_exp(e)?,
            None => 0,
        };
        let mut digits: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
        exp += int.len() as i64;
        let lead = digits.iter().take_while(|&&d| d == b'0').count();
        digits.drain(..lead);
        exp -= lead as i64;
        while digits.last() == Some(&b'0') {
            digits.pop();
        }
        if digits.is_empty() {
            return Some(Num::Finite { neg: false, digits, exp: 0 });
        }
        Some(Num::Finite { neg, digits, exp: exp.clamp(-MAX_EXP, MAX_EXP) })
    }

    pub(crate) fn is_finite(&self) -> bool {
        matches!(self, Num::Finite { .. })
    }

    /// Bytes whose lexicographic order is the numeric order.
    pub(crate) fn key(&self) -> Vec<u8> {
        match self {
            Num::NegInf => vec![0],
            Num::Finite { digits, .. } if digits.is_empty() => vec![2],
            Num::Finite { neg: false, digits, exp } => {
                let mut k = Vec::with_capacity(9 + digits.len());
                k.push(3);
                k.extend_from_slice(&((*exp as u64) ^ (1 << 63)).to_be_bytes());
                k.extend_from_slice(digits);
                k
            }
            Num::Finite { neg: true, digits, exp } => {
                // Inverted: a larger magnitude sorts first; the terminator
                // puts -0.12 after -0.123.
                let mut k = Vec::with_capacity(10 + digits.len());
                k.push(1);
                k.extend(((*exp as u64) ^ (1 << 63)).to_be_bytes().map(|b| !b));
                k.extend(digits.iter().map(|b| !b));
                k.push(0xFF);
                k
            }
            Num::PosInf => vec![4],
            Num::NaN => vec![5],
        }
    }

    /// Nearest f64 (one rounding).
    pub(crate) fn to_f64(&self) -> f64 {
        match self {
            Num::NegInf => f64::NEG_INFINITY,
            Num::PosInf => f64::INFINITY,
            Num::NaN => f64::NAN,
            Num::Finite { neg, digits, exp } if !digits.is_empty() => {
                let s = format!("{}0.{}e{exp}", if *neg { "-" } else { "" }, std::str::from_utf8(digits).unwrap_or("0"));
                s.parse().unwrap_or(f64::NAN)
            }
            Num::Finite { .. } => 0.0,
        }
    }
}

fn parse_exp(e: &str) -> Option<i64> {
    let (neg, d) = match e.as_bytes().first()? {
        b'-' => (true, &e[1..]),
        b'+' => (false, &e[1..]),
        _ => (false, e),
    };
    if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v = d.bytes().fold(0i64, |acc, b| acc.saturating_mul(10).saturating_add((b - b'0') as i64)).min(MAX_EXP);
    Some(if neg { -v } else { v })
}

/// Sort/compare keys of a decimal-text column; text that is no number is null.
pub(crate) fn keys(array: &dyn Array) -> Result<BinaryArray, ArrowError> {
    let text = display::to_display(array)?;
    Ok((0..text.len()).map(|i| if text.is_null(i) { None } else { Num::parse(text.value(i)).map(|n| n.key()) }).collect())
}

/// Sum of exact values: an `i128` mantissa at the widest scale seen, with an
/// f64 sum for when that overflows (more than 38 significant digits).
#[derive(Debug, Default)]
pub(crate) struct ExactSum {
    mant: i128,
    scale: u32,
    overflow: bool,
    approx: f64,
}

impl ExactSum {
    pub(crate) fn add(&mut self, n: &Num) {
        self.approx += n.to_f64();
        if !self.overflow && self.try_add(n).is_none() {
            self.overflow = true;
        }
    }

    fn try_add(&mut self, n: &Num) -> Option<()> {
        let Num::Finite { neg, digits, exp } = n else { return None };
        let mut m: i128 = 0;
        for d in digits {
            m = m.checked_mul(10)?.checked_add((d - b'0') as i128)?;
        }
        if *neg {
            m = -m;
        }
        // value = m × 10^(exp - len)
        let shift = exp.checked_sub(digits.len() as i64)?;
        let scale = if shift >= 0 {
            m = m.checked_mul(10i128.checked_pow(u32::try_from(shift).ok()?)?)?;
            0
        } else {
            u32::try_from(-shift).ok()?
        };
        if scale > self.scale {
            self.mant = self.mant.checked_mul(10i128.checked_pow(scale - self.scale)?)?;
            self.scale = scale;
        } else {
            m = m.checked_mul(10i128.checked_pow(self.scale - scale)?)?;
        }
        self.mant = self.mant.checked_add(m)?;
        Some(())
    }

    pub(crate) fn to_f64(&self) -> f64 {
        if self.overflow { self.approx } else { format!("{}e-{}", self.mant, self.scale).parse().unwrap_or(self.approx) }
    }
}

/// Smallest and largest exact value.
#[derive(Debug, Default)]
pub(crate) struct ExactRange {
    pub(crate) min: Option<(Vec<u8>, Num)>,
    pub(crate) max: Option<(Vec<u8>, Num)>,
}

impl ExactRange {
    pub(crate) fn add(&mut self, n: &Num) {
        let k = n.key();
        if self.min.as_ref().is_none_or(|(m, _)| k.cmp(m) == Ordering::Less) {
            self.min = Some((k.clone(), n.clone()));
        }
        if self.max.as_ref().is_none_or(|(m, _)| k.cmp(m) == Ordering::Greater) {
            self.max = Some((k, n.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(a: &str, b: &str) -> Ordering {
        Num::parse(a).unwrap().key().cmp(&Num::parse(b).unwrap().key())
    }

    #[test]
    fn parses_and_orders() {
        let ordered = [
            "-Infinity",
            "-1e20",
            "-9007199254740993",
            "-9007199254740992",
            "-10",
            "-0.123",
            "-0.12",
            "-0.0001",
            "0",
            "0.0001",
            "0.12",
            "0.123",
            "1.5",
            "1.500001",
            "9.99",
            "10",
            "9007199254740992",
            "9007199254740993",
            "99999999999999999999999999999999999999",
            "1e40",
            "Infinity",
            "NaN",
        ];
        for w in ordered.windows(2) {
            assert_eq!(cmp(w[0], w[1]), Ordering::Less, "{} < {}", w[0], w[1]);
        }
        for (a, b) in
            [("1.50", "1.5"), ("-0.000", "0"), ("+0", "-0"), ("001.10", "1.1"), (".5", "0.50"), ("1E2", "100"), ("12.5e-1", "1.25"), ("inf", "+Infinity")]
        {
            assert_eq!(cmp(a, b), Ordering::Equal, "{a} = {b}");
        }
        for bad in ["", "-", ".", "1.2.3", "1e", "abc", "1,000", "e5", "-NaN", "0x10"] {
            assert_eq!(Num::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn exact_sum() {
        let sum = |vals: &[&str]| {
            let mut s = ExactSum::default();
            vals.iter().for_each(|v| s.add(&Num::parse(v).unwrap()));
            s
        };
        assert_eq!(sum(&["9007199254740992", "1", "1"]).to_f64(), 9007199254740994.0);
        assert_eq!(sum(&["0.1", "0.2"]).to_f64(), 0.3);
        assert_eq!(sum(&["1e3", "-0.001"]).to_f64(), 999.999);
        let s = sum(&["170141183460469231731687303715884105727", "1"]);
        assert!(s.overflow && s.to_f64() > 1.7e38, "falls back to f64 past i128");
    }
}
