//! Durations in blocks (`90d`, `12h`, `300`) and amounts in tGLM (18 decimals).

use crate::repo::BLOCKS_PER_DAY;
use charter_store::U256;

const DECIMALS: usize = 18;

/// `<n>d` days, `<n>h` hours, `<n>m` minutes or `<n>` blocks (2 s blocks).
pub fn blocks(s: &str) -> Result<u64, String> {
    let (num, mul) = match s.as_bytes().last() {
        Some(b'd') => (&s[..s.len() - 1], BLOCKS_PER_DAY),
        Some(b'h') => (&s[..s.len() - 1], BLOCKS_PER_DAY / 24),
        Some(b'm') => (&s[..s.len() - 1], 30),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mul))
        .ok_or_else(|| format!("not a duration: {s:?} (use 90d, 12h, 30m or a block count)"))
}

/// Decimal tGLM to wei.
pub fn glm(s: &str) -> Result<U256, String> {
    let bad = || format!("not an amount: {s:?}");
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    if int.len() + frac.len() == 0 || !digits(int) || !digits(frac) || frac.len() > DECIMALS {
        return Err(bad());
    }
    let padded = format!(
        "{}{frac:0<DECIMALS$}",
        if int.is_empty() { "0" } else { int }
    );
    U256::from_str_radix(&padded, 10).map_err(|_| bad())
}

/// Wei as decimal tGLM, trailing zeros trimmed.
pub fn fmt_glm(wei: U256) -> String {
    let s = format!("{wei:0>width$}", width = DECIMALS + 1);
    let (int, frac) = s.split_at(s.len() - DECIMALS);
    let frac = frac.trim_end_matches('0');
    match frac.is_empty() {
        true => int.to_owned(),
        false => format!("{int}.{frac}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_to_blocks() {
        assert_eq!(blocks("90d"), Ok(90 * 43_200));
        assert_eq!(blocks("12h"), Ok(21_600));
        assert_eq!(blocks("30m"), Ok(900));
        assert_eq!(blocks("300"), Ok(300));
        for bad in ["", "d", "1.5d", "-1", "99999999999999999999d"] {
            assert!(blocks(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn amounts_round_trip() {
        assert_eq!(glm("0.02"), Ok(U256::from(20_000_000_000_000_000u64)));
        assert_eq!(glm("1"), Ok(U256::from(10u64.pow(18))));
        assert_eq!(glm(".5"), Ok(U256::from(5 * 10u64.pow(17))));
        for bad in ["", ".", "1.2.3", "x", "1e3", "0.0000000000000000001"] {
            assert!(glm(bad).is_err(), "{bad}");
        }
        assert_eq!(fmt_glm(U256::from(20_000_000_000_000_000u64)), "0.02");
        assert_eq!(fmt_glm(U256::from(10u64.pow(18) * 3)), "3");
        assert_eq!(fmt_glm(U256::ZERO), "0");
    }
}
