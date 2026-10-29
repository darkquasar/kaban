//! Integer arithmetic for reconciliation. Floating inputs cross the seam once.
use serde::{Deserialize, Serialize};

/// A confidence in units of 1/10000. Use `new` or `from_f64` at input seams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Fixed(pub u32);

/// An invalid confidence or signed weight at the input seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("fixed_point_out_of_range")]
pub struct FixedError;

impl Fixed {
    /// Zero confidence.
    pub const ZERO: Self = Self(0);
    /// Unit confidence.
    pub const ONE: Self = Self(10_000);

    /// Validate integer confidence without clamping.
    /// # Errors
    /// Returns `FixedError` above 10000.
    pub const fn new(value: u32) -> Result<Self, FixedError> {
        if value <= 10_000 {
            Ok(Self(value))
        } else {
            Err(FixedError)
        }
    }

    /// Round half to even once at the floating input seam.
    /// # Errors
    /// Rejects non-finite values and values outside [0, 1].
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn from_f64(value: f64) -> Result<Self, FixedError> {
        if !(0.0..=1.0).contains(&value) {
            return Err(FixedError);
        }
        Self::new((value * 10_000.0).round_ties_even() as u32)
    }
}
/// The four-decimal rendering of EN-F12a (`6556` is `0.6556`, `10000` is `1.0000`): the one
/// text form of a weight in `explain` and `doctor`; the integer stays the compared value.
impl std::fmt::Display for Fixed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:04}", self.0 / 10_000, self.0 % 10_000)
    }
}
impl TryFrom<u32> for Fixed {
    type Error = FixedError;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<Fixed> for u32 {
    fn from(value: Fixed) -> Self {
        value.0
    }
}

/// Signed log2 odds in units of 1/10000. Accumulate in `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LogWeight(pub i32);

impl LogWeight {
    /// Convert a finite signed weight, rounding half to even once.
    /// # Errors
    /// Rejects weights whose rounded scaled value does not fit `i32`.
    #[allow(clippy::cast_possible_truncation)]
    pub fn from_f64(value: f64) -> Result<Self, FixedError> {
        let scaled = (value * 10_000.0).round_ties_even();
        if !scaled.is_finite() || scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
            return Err(FixedError);
        }
        Ok(Self(scaled as i32))
    }

    /// Deterministic integer log2 using Q62 repeated squaring and 48 fractional bits.
    /// Zero endpoints represent infinite evidence as `i32::MIN`/`MAX`; equal inputs
    /// (including two zero endpoints) contribute zero. Priors for finite ER weights
    /// should be strictly between zero and one.
    pub fn log2_ratio(num: Fixed, den: Fixed) -> Self {
        if num == den {
            return Self(0);
        }
        if num.0 == 0 {
            return Self(i32::MIN);
        }
        if den.0 == 0 {
            return Self(i32::MAX);
        }
        let (n, d, sign) = if num > den {
            (num.0, den.0, 1_i64)
        } else {
            (den.0, num.0, -1_i64)
        };
        let mut x = (u128::from(n) << 62) / u128::from(d);
        let mut whole = 0_u64;
        while x >= (2_u128 << 62) {
            x >>= 1;
            whole += 1;
        }
        let mut fraction = 0_u64;
        for _ in 0..48 {
            x = (x * x) >> 62;
            fraction <<= 1;
            if x >= (2_u128 << 62) {
                x >>= 1;
                fraction |= 1;
            }
        }
        let scaled = i128::from((whole << 48) | fraction) * 10_000;
        let rounded = round_ratio(scaled, 1_i128 << 48);
        // The ratio of two nonzero u32 inputs has |log2| < 32.
        Self(i32::try_from(rounded * i128::from(sign)).unwrap_or(0))
    }

    /// The ER seam `clamp(W / match_threshold)`, after signed `i64` accumulation.
    /// # Errors
    /// A non-positive match threshold is invalid.
    pub fn raw_score(sum: i64, threshold: Self) -> Result<Fixed, FixedError> {
        if threshold.0 <= 0 {
            return Err(FixedError);
        }
        let score = (i128::from(sum) * 10_000 / i128::from(threshold.0)).clamp(0, 10_000);
        Ok(Fixed(u32::try_from(score).unwrap_or(0)))
    }
}

fn round_ratio(num: i128, den: i128) -> i128 {
    let q = num / den;
    let remainder = num % den;
    q + i128::from(remainder * 2 > den || (remainder * 2 == den && q % 2 != 0))
}
