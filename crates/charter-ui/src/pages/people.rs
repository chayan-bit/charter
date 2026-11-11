//! Who and when, as pages say them (docs/p6/ui-v2.md 3.1): a peer's name from its alias
//! or the local nickname (Settings), and wall-clock time from a block. Block times are
//! estimated from the view's head block and sync time at 2 s per block, so a page reads
//! no chain to show them; the block number stays in the tooltip and Technical details.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::ago;
use crate::cache::RepoView;
use crate::prefs::{Prefs, Times};

/// Arkiv's block interval.
pub const BLOCK_MS: u64 = 2_000;
const WEEK_MS: u64 = 7 * 86_400_000;
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `(year, month 1..=12, day)` (UTC) of unix seconds (Howard Hinnant's civil-from-days).
pub fn civil(secs: i64) -> (i64, i64, i64) {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `14 Jun 2026` of unix milliseconds.
pub fn date(ms: u64) -> String {
    let (y, m, d) = civil(i64::try_from(ms / 1000).unwrap_or(0));
    let month = MONTHS[usize::try_from(m - 1).unwrap_or(0)];
    format!("{d} {month} {y}")
}

/// `2026-06-14 09:41 UTC`, for tooltips.
pub fn stamp(ms: u64) -> String {
    let s = i64::try_from(ms / 1000).unwrap_or(0);
    let (y, m, d) = civil(s);
    let (h, min) = (s.rem_euclid(86_400) / 3_600, s.rem_euclid(3_600) / 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02} UTC")
}

/// `12 min ago` within a week, else the date.
pub fn humane(ms: u64) -> String {
    if crate::sync_loop::now_ms().saturating_sub(ms) < WEEK_MS {
        ago(ms)
    } else {
        date(ms)
    }
}

/// Block number to time, from one view.
#[derive(Debug, Clone, Copy, Default)]
pub struct Clock {
    pub head_block: u64,
    pub synced_ms: u64,
}

impl Clock {
    pub fn of(v: Option<&RepoView>) -> Self {
        v.map_or_else(Self::default, |v| Self {
            head_block: v.head_block,
            synced_ms: v.synced_ms,
        })
    }

    /// Estimated unix ms of `block`; `None` before the first sync.
    pub fn ms(&self, block: u64) -> Option<u64> {
        (self.head_block > 0 && self.synced_ms > 0).then(|| {
            let behind = self.head_block.saturating_sub(block);
            self.synced_ms
                .saturating_sub(behind.saturating_mul(BLOCK_MS))
        })
    }

    /// `12 min ago` or `14 Jun 2026`.
    pub fn when(&self, block: u64) -> String {
        self.ms(block)
            .map_or_else(|| "not synced yet".into(), humane)
    }

    /// Tooltip: `2026-06-14 09:41 UTC, block 1,204,331`.
    pub fn full(&self, block: u64) -> String {
        let b = super::write::group(block);
        self.ms(block).map_or_else(
            || format!("block {b}"),
            |ms| format!("{}, block {b}", stamp(ms)),
        )
    }
}

/// Peer names for one page.
#[derive(Debug, Clone, Default)]
pub struct People {
    /// Peer id to the alias its peer document carries.
    pub aliases: BTreeMap<String, String>,
    pub prefs: Arc<Prefs>,
    /// This computer's peer id, shown as "you".
    pub you: Option<String>,
}

impl People {
    pub fn new(v: Option<&RepoView>, prefs: Arc<Prefs>, you: Option<String>) -> Self {
        Self {
            aliases: v.map(|v| v.aliases.clone()).unwrap_or_default(),
            prefs,
            you,
        }
    }

    pub fn name(&self, nid: &str) -> String {
        if self.you.as_deref() == Some(nid) && !self.prefs.nicknames.contains_key(nid) {
            return "you".into();
        }
        self.prefs
            .name(nid, self.aliases.get(nid).map(String::as_str))
    }

    /// The author's own clock (`ts_ms`), when Settings asks for it.
    pub fn author_clock(&self, ts_ms: u64) -> Option<String> {
        (self.prefs.times == Times::Both && ts_ms > 0).then(|| humane(ts_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_stamps_are_utc() {
        assert_eq!(civil(951_782_400), (2000, 2, 29));
        assert_eq!(date(1_700_000_000_000), "14 Nov 2023");
        assert_eq!(stamp(1_700_000_000_000), "2023-11-14 22:13 UTC");
    }

    #[test]
    fn block_times_count_back_from_the_synced_head() {
        let c = Clock {
            head_block: 1_000,
            synced_ms: 10_000_000,
        };
        assert_eq!(c.ms(1_000), Some(10_000_000));
        assert_eq!(c.ms(990), Some(9_980_000));
        assert_eq!(c.ms(2_000), Some(10_000_000), "a newer block is now");
        assert_eq!(Clock::default().ms(5), None);
        assert_eq!(Clock::default().when(5), "not synced yet");
        assert!(c.full(990).ends_with(", block 990"));
    }
}
