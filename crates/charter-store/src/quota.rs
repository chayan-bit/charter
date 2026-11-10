//! The RPC's own account of its rate quota, read from each response's headers:
//! `RateLimit` (`limit=10000, remaining=0, reset=2035`, or the structured form
//! `"name";r=0;t=2035`), `RateLimit-Policy`, `Arkiv-Quota-Used-Percent` and a 429's
//! `Retry-After`. `arkiv_query` costs 10 units of a per-IP budget (docs/p0/arkiv-limits.md),
//! so long-running readers (`chart ui`) pace themselves by it.

use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;

/// How long an exhausted quota with no reset time is assumed to stay exhausted.
const UNKNOWN_RESET: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq)]
pub struct Quota {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    /// Seconds until the window resets, as sent.
    pub reset: Option<u64>,
    pub policy: Option<String>,
    /// `Arkiv-Quota-Used-Percent`.
    pub used_percent: Option<f64>,
    /// The response was a 429.
    pub refused: bool,
    /// A 429's `Retry-After`, in seconds.
    pub retry_after: Option<u64>,
    /// When the response arrived.
    pub at: Instant,
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)?.to_str().ok().map(str::trim)
}

/// `limit`, `remaining` and `reset` from either `RateLimit` form.
fn rate_limit(v: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let (mut l, mut r, mut t) = (None, None, None);
    for part in v.split([',', ';']) {
        let Some((k, n)) = part.trim().split_once('=') else {
            continue;
        };
        let n = n.trim().trim_matches('"').parse().ok();
        match k.trim() {
            "limit" | "l" => l = n,
            "remaining" | "r" => r = n,
            "reset" | "t" => t = n,
            _ => {}
        }
    }
    (l, r, t)
}

impl Quota {
    /// The quota a response reports; `None` when it says nothing about one.
    pub fn from_response(status: u16, h: &HeaderMap, at: Instant) -> Option<Self> {
        let (limit, remaining, reset) =
            header(h, "ratelimit").map_or((None, None, None), rate_limit);
        let used_percent = header(h, "arkiv-quota-used-percent").and_then(|v| v.parse().ok());
        let retry_after = header(h, "retry-after").and_then(|v| v.parse().ok());
        let policy = header(h, "ratelimit-policy").map(str::to_owned);
        let q = Self {
            limit,
            remaining,
            reset,
            policy,
            used_percent,
            refused: status == 429,
            retry_after: retry_after.filter(|_| status == 429),
            at,
        };
        let said = q.limit.is_some() || q.remaining.is_some() || q.used_percent.is_some();
        (said || status == 429).then_some(q)
    }

    /// Percent of the window used: the node's own figure, else from limit and remaining;
    /// a 429 is 100.
    pub fn used(&self) -> Option<f64> {
        let derived = match (self.limit, self.remaining) {
            (Some(l), Some(r)) if l > 0 => Some(100.0 * l.saturating_sub(r) as f64 / l as f64),
            _ => None,
        };
        let refused = self.refused.then_some(100.0);
        [self.used_percent, derived, refused]
            .into_iter()
            .flatten()
            .reduce(f64::max)
    }

    /// When this report stops being news: `Retry-After`, else the window reset, else a
    /// few minutes.
    pub fn until(&self) -> Instant {
        let s = self.retry_after.or(self.reset);
        self.at + s.map_or(UNKNOWN_RESET, Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(kv: &[(&'static str, &str)]) -> HeaderMap {
        kv.iter()
            .map(|(k, v)| (k.parse().expect("name"), v.parse().expect("value")))
            .collect()
    }

    #[test]
    fn an_exhausted_budget_and_its_retry_after_are_read() {
        let at = Instant::now();
        let h = headers(&[
            ("RateLimit", "limit=10000, remaining=0, reset=2035"),
            ("Retry-After", "2035"),
            ("RateLimit-Policy", "10000;w=3600"),
        ]);
        let q = Quota::from_response(429, &h, at).expect("quota");
        assert_eq!(
            (q.limit, q.remaining, q.reset, q.retry_after),
            (Some(10_000), Some(0), Some(2035), Some(2035))
        );
        assert_eq!(q.policy.as_deref(), Some("10000;w=3600"));
        assert_eq!(q.used(), Some(100.0));
        assert_eq!(q.until(), at + Duration::from_secs(2035));
    }

    #[test]
    fn the_structured_form_and_the_used_percent_header_are_read() {
        let at = Instant::now();
        let h = headers(&[
            ("RateLimit", "\"anon\";r=2500;t=30"),
            ("RateLimit-Policy", "\"anon\";q=10000;w=600"),
        ]);
        let q = Quota::from_response(200, &h, at).expect("quota");
        assert_eq!(
            (q.remaining, q.reset, q.used()),
            (Some(2500), Some(30), None)
        );
        let h = headers(&[
            ("Arkiv-Quota-Used-Percent", "42.5"),
            ("RateLimit", "limit=100, remaining=90"),
        ]);
        let q = Quota::from_response(200, &h, at).expect("quota");
        assert_eq!(
            q.used(),
            Some(42.5),
            "the node's own figure wins when higher"
        );
        assert_eq!(q.retry_after, None, "Retry-After counts only on a 429");
        assert!(Quota::from_response(200, &HeaderMap::new(), at).is_none());
        let bare = Quota::from_response(429, &HeaderMap::new(), at).expect("a bare 429");
        assert_eq!(bare.used(), Some(100.0));
        assert_eq!(bare.until(), at + UNKNOWN_RESET);
    }
}
