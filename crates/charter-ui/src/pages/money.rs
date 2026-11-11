//! The one way pages show an amount of tGLM: `0.0044 tGLM` from 0.0001 tGLM up (at most four
//! decimals), `5 gwei` or `5 wei` below it, always with the exact tGLM value for a tooltip.

use std::fmt;

use charter_store::U256;
use charter_sync::units::fmt_glm;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amount {
    /// `0.0044`, `12.5`, `5`.
    pub num: String,
    /// `tGLM`, `gwei` or `wei`; empty for a value that is not an amount, shown as it is.
    pub unit: &'static str,
    /// `0.004399871 tGLM`.
    pub exact: String,
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.unit {
            "" => f.write_str(&self.num),
            u => write!(f, "{} {u}", self.num),
        }
    }
}

const GLM: u64 = 1_000_000_000_000_000_000;
const GWEI: u64 = 1_000_000_000;
/// 0.0001 tGLM.
const SMALL: u64 = 100_000_000_000_000;

impl Amount {
    /// The number padded for a column: four decimals for tGLM and gwei, none for wei.
    pub fn fixed(&self) -> String {
        match self.unit {
            "tGLM" | "gwei" => {
                let (int, frac) = self.num.split_once('.').unwrap_or((&self.num, ""));
                format!("{int}.{frac:0<4}")
            }
            _ => self.num.clone(),
        }
    }
}

/// `wei` in `scale`-wei units, at most four decimals with trailing zeros dropped, rounded up
/// (a most-you-pay or a cost) or down (a balance).
fn decimals(wei: U256, scale: U256, up: bool) -> String {
    let step = scale / U256::from(10_000_u64);
    let q = match up {
        true => wei.div_ceil(step),
        false => wei / step,
    };
    let ten_k = U256::from(10_000_u64);
    let frac = format!("{:04}", (q % ten_k).to::<u64>());
    match frac.trim_end_matches('0') {
        "" => (q / ten_k).to_string(),
        f => format!("{}.{f}", q / ten_k),
    }
}

/// `wei` as pages show it.
pub fn amount(wei: U256, up: bool) -> Amount {
    let (num, unit) = match wei {
        w if w.is_zero() => ("0".to_owned(), "tGLM"),
        w if w < U256::from(GWEI) => (w.to_string(), "wei"),
        w if w < U256::from(SMALL) => (decimals(w, U256::from(GWEI), up), "gwei"),
        w => (decimals(w, U256::from(GLM), up), "tGLM"),
    };
    Amount {
        num,
        unit,
        exact: format!("{} tGLM", fmt_glm(wei)),
    }
}

/// A decimal wei amount from an op or program state (rounded down: it is held, not paid);
/// anything else as it is.
pub fn glm(wei: &str) -> Amount {
    let raw = || Amount {
        num: wei.to_owned(),
        unit: "",
        exact: wei.to_owned(),
    };
    U256::from_str_radix(wei.trim_matches('"'), 10).map_or_else(|_| raw(), |w| amount(w, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(wei: u128, up: bool) -> String {
        amount(U256::from(wei), up).to_string()
    }

    #[test]
    fn tiny_amounts_show_wei_or_gwei_and_the_rest_four_decimals() {
        assert_eq!(show(0, true), "0 tGLM");
        assert_eq!(show(5, true), "5 wei");
        assert_eq!(show(999_999_999, false), "999999999 wei");
        assert_eq!(show(1_000_000_000, true), "1 gwei");
        assert_eq!(show(1_234_567_891, true), "1.2346 gwei");
        assert_eq!(show(1_234_567_891, false), "1.2345 gwei");
        assert_eq!(show(99_999_999_999_999, false), "99999.9999 gwei");
        assert_eq!(show(100_000_000_000_000, true), "0.0001 tGLM");
        assert_eq!(show(4_399_871_000_000_000, true), "0.0044 tGLM");
        assert_eq!(show(4_399_871_000_000_000, false), "0.0043 tGLM");
        assert_eq!(show(5_000_000_000_000_000_000, false), "5 tGLM");
        assert_eq!(show(12_500_000_000_000_000_000, true), "12.5 tGLM");
    }

    #[test]
    fn the_exact_value_rides_along_and_non_numbers_pass_through() {
        assert_eq!(
            amount(U256::from(5), true).exact,
            "0.000000000000000005 tGLM"
        );
        assert_eq!(glm("\"5\"").to_string(), "5 wei");
        assert_eq!(glm("fixed").to_string(), "fixed");
    }

    #[test]
    fn columns_pad_tglm_and_gwei_to_four_decimals() {
        let col = |wei: u128| amount(U256::from(wei), true).fixed();
        assert_eq!(col(3_000_000_000_000_000), "0.0030");
        assert_eq!(col(13_900_000_000_000_000), "0.0139");
        assert_eq!(col(5_000_000_000_000_000_000), "5.0000");
        assert_eq!(col(0), "0.0000");
        assert_eq!(col(1_500_000_000), "1.5000");
        assert_eq!(col(5), "5");
    }
}
