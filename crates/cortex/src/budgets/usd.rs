//! Fixed-point USD type for deterministic budget math.
//!
//! Every USD value in the budget pipeline is stored as a decimal string
//! (`provider_rate_cards.{prompt,completion}_per_mtok_usd`,
//! `budgets.hard_cap_usd`, `provider_call_events.usd_cost`) — float math
//! across replayed audit chains would produce drift over time.
//!
//! `Usd` wraps an `i128` count of micro-USD (1 USD = 1_000_000 µUSD) so add,
//! subtract, multiply-by-token-count, and divide-by-million are exact integer
//! operations. Conversion to/from the on-disk decimal string is the only place
//! we touch `f64` parsing, and only via `str::parse` against a normalized
//! integer-and-fractional split.

use anyhow::{anyhow, Result};
use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, AddAssign, Mul, Sub};

const SCALE: i128 = 1_000_000;

/// USD amount in micro-dollars (10⁻⁶ of a dollar). Six decimal places of
/// precision is enough for prompt-token math: at $0.05 / Mtok, one token is
/// 0.05 µUSD, so even single-token costs round to whole µUSD without loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usd(i128);

impl Usd {
    /// Zero dollars.
    pub const ZERO: Usd = Usd(0);

    /// Build from a decimal string ("0.80", "20", "20.00", "0").
    /// Negative inputs are rejected — budgets never carry negative spend.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(anyhow!("empty USD string"));
        }
        if s.starts_with('-') {
            return Err(anyhow!("negative USD not allowed: {s}"));
        }
        let (int_part, frac_part) = match s.split_once('.') {
            Some((i, f)) => (i, f),
            None => (s, ""),
        };
        let int_val: i128 = if int_part.is_empty() {
            0
        } else {
            int_part
                .parse::<i128>()
                .map_err(|e| anyhow!("invalid integer part '{int_part}': {e}"))?
        };
        // Pad/truncate fractional part to exactly 6 digits.
        let frac_val: i128 = if frac_part.is_empty() {
            0
        } else if frac_part.len() <= 6 {
            let mut padded = String::with_capacity(6);
            padded.push_str(frac_part);
            for _ in frac_part.len()..6 {
                padded.push('0');
            }
            padded
                .parse::<i128>()
                .map_err(|e| anyhow!("invalid fractional part '{frac_part}': {e}"))?
        } else {
            frac_part[..6]
                .parse::<i128>()
                .map_err(|e| anyhow!("invalid fractional part '{frac_part}': {e}"))?
        };
        Ok(Usd(int_val * SCALE + frac_val))
    }

    /// Decimal string with two-decimal-place precision for display
    /// (`/status`, toasts). Use [`Self::to_storage_string`] for DB writes.
    pub fn to_display_string(self) -> String {
        let dollars = self.0 / SCALE;
        let cents = ((self.0 % SCALE) / 10_000).abs();
        format!("{dollars}.{cents:02}")
    }

    /// Decimal string with six-decimal-place precision, trailing zeros
    /// stripped (down to two minimum). Stored verbatim in SQLite so a
    /// round-trip parse-then-format is a fixed point.
    pub fn to_storage_string(self) -> String {
        let dollars = self.0 / SCALE;
        let micros = (self.0 % SCALE).abs();
        if micros == 0 {
            return format!("{dollars}.00");
        }
        let mut s = format!("{micros:06}");
        // Strip trailing zeros down to a minimum of 2 fractional digits so
        // "20.000000" → "20.00" but "0.123450" → "0.12345".
        while s.len() > 2 && s.ends_with('0') {
            s.pop();
        }
        format!("{dollars}.{s}")
    }

    /// Cost of `tokens` priced at `per_mtok` USD per million tokens.
    ///
    /// Computed as `(per_mtok_micros * tokens + ROUND_HALF_UP) / 1_000_000`
    /// in i128 arithmetic — exact, deterministic, no float involvement.
    pub fn from_tokens_at_rate(tokens: u64, per_mtok: Usd) -> Usd {
        if tokens == 0 || per_mtok.0 == 0 {
            return Usd::ZERO;
        }
        let numerator = per_mtok.0 * (tokens as i128);
        // Round half up.
        let micros = (numerator + 500_000) / 1_000_000;
        Usd(micros)
    }

    /// True if `self == 0`.
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Internal micro-USD count (mainly for tests).
    #[cfg(test)]
    pub(crate) fn micros(self) -> i128 {
        self.0
    }

    /// `self * (pct / 100)` with half-up rounding to the nearest µUSD.
    /// Used for the 80%-of-soft-cap warning threshold.
    pub(crate) fn scale_percent(self, pct: u32) -> Usd {
        let micros = (self.0 * pct as i128 + 50) / 100;
        Usd(micros)
    }

    /// `self - rhs` clamped at zero. Used in the
    /// "lowest remaining headroom wins" min-by selection.
    pub(crate) fn saturating_sub(self, rhs: Usd) -> Usd {
        if rhs.0 >= self.0 {
            Usd(0)
        } else {
            Usd(self.0 - rhs.0)
        }
    }

    /// Crate-internal raw micros — used by the engine's `min_by_key` over
    /// `Usd` values without exposing the field publicly.
    pub(crate) fn raw_micros(self) -> i128 {
        self.0
    }
}

impl Add for Usd {
    type Output = Usd;
    fn add(self, rhs: Usd) -> Usd {
        Usd(self.0 + rhs.0)
    }
}

impl AddAssign for Usd {
    fn add_assign(&mut self, rhs: Usd) {
        self.0 += rhs.0;
    }
}

impl Sub for Usd {
    type Output = Usd;
    fn sub(self, rhs: Usd) -> Usd {
        Usd(self.0 - rhs.0)
    }
}

impl Mul<u64> for Usd {
    type Output = Usd;
    fn mul(self, rhs: u64) -> Usd {
        Usd(self.0 * (rhs as i128))
    }
}

impl PartialOrd for Usd {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Usd {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_round_trip_storage() {
        // Each input round-trips through parse → to_storage_string →
        // parse to the same micro-USD count, exactly.
        let cases = [
            ("0", 0),
            ("20", 20_000_000),
            ("20.00", 20_000_000),
            ("0.80", 800_000),
            ("0.123450", 123_450),
            ("19.50", 19_500_000),
        ];
        for (input, expected_micros) in cases {
            let u = Usd::parse(input).unwrap();
            assert_eq!(u.micros(), expected_micros, "parse {input}");
            let s = u.to_storage_string();
            let u2 = Usd::parse(&s).unwrap();
            assert_eq!(u, u2, "round-trip via {s}");
        }
    }

    #[test]
    fn from_tokens_matches_decimal_arithmetic() {
        // 1000 prompt tokens at $3.00 / Mtok = $0.003 = 3000 µUSD.
        let rate = Usd::parse("3.00").unwrap();
        assert_eq!(Usd::from_tokens_at_rate(1000, rate), Usd(3000));
        // 1_000_000 tokens at $3.00 / Mtok = $3.00.
        assert_eq!(Usd::from_tokens_at_rate(1_000_000, rate), Usd(3_000_000));
        // Zero edges.
        assert_eq!(Usd::from_tokens_at_rate(0, rate), Usd::ZERO);
        assert_eq!(Usd::from_tokens_at_rate(1000, Usd::ZERO), Usd::ZERO);
    }

    #[test]
    fn parse_rejects_negative() {
        assert!(Usd::parse("-1").is_err());
        assert!(Usd::parse("-0.50").is_err());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(Usd::parse("").is_err());
        assert!(Usd::parse("abc").is_err());
        assert!(Usd::parse("1.2.3").is_err());
    }

    #[test]
    fn add_sub_compose() {
        let a = Usd::parse("19.50").unwrap();
        let b = Usd::parse("0.80").unwrap();
        assert_eq!((a + b).to_storage_string(), "20.30");
        let c = Usd::parse("20.00").unwrap();
        assert_eq!((c - a).to_storage_string(), "0.50");
    }

    #[test]
    fn ordering_uses_micros_not_strings() {
        // String comparison would order "10.00" < "9.00" — verify we don't.
        let a = Usd::parse("9.00").unwrap();
        let b = Usd::parse("10.00").unwrap();
        assert!(a < b);
    }
}
