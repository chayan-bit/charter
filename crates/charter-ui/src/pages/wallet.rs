//! `/wallet` (docs/p6/ui-v2.md 4.4): balance, address and the spend ledger. The balance
//! is the one the writer last read (at start, on pricing, after a write, on Refresh), so
//! the page itself never reads the chain.

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use charter_store::U256;
use charter_sync::units::glm;

use super::money::{Amount, amount};
use super::people::{date, humane, stamp};
use super::write::WRITES_OFF;
use super::{App, Layout, Pref, WalletChip, blocking, html, short};
use crate::spend::{self, Spend};
use crate::writes::Writer;

/// Below this the chip and the page warn (tGLM): a handful of writes at testnet fees.
pub const LOW: &str = "0.01";

fn low(wei: U256) -> bool {
    glm(LOW).is_ok_and(|l| wei < l)
}

pub fn chip(w: &Writer) -> WalletChip {
    match w.balance() {
        Some((wei, _)) => WalletChip {
            amount: Some(amount(wei, false)),
            low: low(wei),
        },
        None => WalletChip {
            amount: None,
            low: false,
        },
    }
}

/// One ledger row as the page shows it.
pub struct Row {
    pub when: String,
    pub stamp: String,
    pub action: String,
    pub repo: String,
    pub hex: String,
    pub cost: Amount,
    pub value: Option<Amount>,
    pub skipped: bool,
    pub estimated: bool,
}

fn row(app: &App, s: &Spend) -> Row {
    let wei = |v: &str| U256::from_str_radix(v, 10).unwrap_or_default();
    let view: Option<crate::cache::RepoView> = crate::cache::read(
        &app.home
            .join("repos")
            .join(&s.repo)
            .join(crate::cache::VIEW),
    )
    .ok()
    .flatten();
    Row {
        when: humane(s.ts_ms),
        stamp: stamp(s.ts_ms),
        action: s.action.clone(),
        repo: view.map_or_else(|| short(&s.repo), |v| v.name),
        hex: s.repo.clone(),
        cost: amount(wei(&s.wei), true),
        value: Some(wei(&s.value))
            .filter(|v| *v > U256::ZERO)
            .map(|v| amount(v, false)),
        skipped: s.skipped,
        estimated: s.estimated,
    }
}

#[derive(Template)]
#[template(path = "wallet.html")]
pub struct WalletPage {
    pub l: Layout,
    /// `None` without keys.
    pub address: Option<String>,
    pub balance: Option<Amount>,
    pub read: String,
    pub low: bool,
    pub rows: Vec<Row>,
    pub total: Amount,
    pub error: Option<String>,
    pub off: &'static str,
    pub since: String,
}

impl WalletPage {
    pub fn low_mark(&self) -> &'static str {
        LOW
    }
}

pub async fn page(State(app): State<Arc<App>>, pref: Pref) -> Response {
    app.shared.touch();
    blocking(move || {
        let mut l = Layout::new(&app, &pref, "Wallet");
        l.nav = "wallet";
        let w = app.writer.as_ref();
        let bal = w.and_then(|w| w.balance());
        let (entries, error) = match spend::read(&app.home) {
            Ok(e) => (e, None),
            Err(e) => (vec![], Some(e)),
        };
        let total = entries
            .iter()
            .filter_map(|s| U256::from_str_radix(&s.wei, 10).ok())
            .fold(U256::ZERO, |a, b| a + b);
        html(&WalletPage {
            address: w.map(|w| w.address.to_string()),
            balance: bal.map(|(wei, _)| amount(wei, false)),
            read: bal.map_or_else(String::new, |(_, t)| humane(t)),
            low: bal.is_some_and(|(wei, _)| low(wei)),
            since: entries.last().map_or_else(String::new, |s| date(s.ts_ms)),
            rows: entries.iter().map(|s| row(&app, s)).collect(),
            total: amount(total, true),
            error,
            off: WRITES_OFF,
            l,
        })
    })
    .await
}

/// `GET /wallet/chip`: the header chip alone, for live refresh after a write.
#[derive(Template)]
#[template(path = "wallet_chip.html")]
pub struct Chip {
    pub l: Layout,
}

pub async fn chip_part(State(app): State<Arc<App>>, pref: Pref) -> Response {
    html(&Chip {
        l: Layout::new(&app, &pref, ""),
    })
}

/// `POST /wallet/refresh`: one balance read in the background, then back to the page.
pub async fn refresh(State(app): State<Arc<App>>) -> Response {
    let Some(w) = app.writer.as_ref() else {
        return (StatusCode::FORBIDDEN, WRITES_OFF).into_response();
    };
    w.refresh().await;
    (StatusCode::SEE_OTHER, [(header::LOCATION, "/wallet")]).into_response()
}
