//! Write pages (docs/p6/ui.md 4.4, ui-v2.md 3.4): `POST /r/<rid>/write` prices a form
//! and shows the cost dialog; `POST /r/<rid>/write/<id>` signs and sends it once;
//! `GET /r/<rid>/write/<id>` shows the three steps (htmx polls `?part=1` until the block;
//! the op event of the live stream then marks it visible; with JS off a reload does the
//! same). Nothing is written on a GET. The only write without the dialog is one the user
//! opted into in Settings ([`crate::prefs::skips`]), and it still ends on the status page.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use charter_store::U256;
use charter_sync::ids::{hex32, parse_id};
use charter_sync::units::fmt_glm;

use super::money::{Amount, amount};
use super::{App, Head, Layout, Pref, blocking, error_page, frame, html, open, short};
use crate::writes::{Draft, Estimate, Status, Step, Writer, form};
use crate::{cache, prefs};

pub const WRITES_OFF: &str = "Writes are off: there are no keys in $CHARTER_HOME. \
    Run `chart keys init`, fund the wallet, then restart `chart ui`.";

#[derive(Template)]
#[template(path = "confirm.html")]
pub struct Confirm {
    pub l: Layout,
    pub r: Head,
    pub id: String,
    pub back: String,
    /// `Post this comment?`
    pub ask: String,
    /// The primary button: `Comment for up to 0.0022 tGLM`.
    pub button: String,
    /// What is published, in words (a title, a body, an approval of a commit).
    pub publish: String,
    /// Most you can pay, rounded up.
    pub most: Amount,
    pub wallet_now: Amount,
    pub wallet_after: Amount,
    /// `about 6 s`.
    pub takes: String,
    /// `about 6 months`.
    pub kept: String,
    /// Breakdown lines: `(label, text, or an amount for the column)`; the last is the total.
    pub receipt: Vec<(&'static str, String, Option<Amount>)>,
    /// The balance cannot cover it: Send is disabled.
    pub short_of: Option<String>,
    /// The RPC refused or nearly ran out of quota: Send is disabled until it resets.
    pub quota: Option<String>,
    /// The paying wallet, to copy when it needs funds.
    pub address: String,
    /// What will be signed, one `type action` line and its JSON payload per op.
    pub ops: Vec<(String, String)>,
}

/// `(question, button verb)` of a draft label.
fn ask(label: &str) -> (String, &'static str) {
    let (q, verb) = match label {
        "Open issue" => ("Open this issue?", "Open issue"),
        "Comment" => ("Post this comment?", "Comment"),
        "Approve patch" => ("Approve this patch?", "Approve"),
        "Review patch" => ("Post this review?", "Review"),
        "Mark patch merged" => ("Mark this patch merged?", "Mark merged"),
        "Claim bounty" => ("Claim this bounty?", "Open patch"),
        "Fund bounty" => ("Fund this bounty?", "Fund"),
        form::REMOVE_REVOKED => ("Remove the revoked devices from the group?", "Remove"),
        l if l.starts_with("Publish release") => return (format!("{l}?"), "Publish"),
        l => return (format!("{l}?"), "Send"),
    };
    (q.to_owned(), verb)
}

/// What the draft publishes, in words, from its first op.
fn publishes(d: &Draft) -> String {
    let field =
        |p: &serde_json::Value, k: &str| p.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    for s in &d.steps {
        let Step::Op {
            action, payload, ..
        } = s
        else {
            continue;
        };
        if let Some(t) = field(payload, "title") {
            return t;
        }
        if let Some(t) = field(payload, "tag") {
            return format!("Release {t}");
        }
        match (*action, field(payload, "head")) {
            ("approve", Some(h)) => return format!("Approval of commit {}", short(&h)),
            ("merged", _) => return "A note that this patch is merged".into(),
            ("pledge", _) => continue,
            _ => {}
        }
        if let Some(b) = field(payload, "body").filter(|b| !b.is_empty()) {
            return b;
        }
    }
    if removes(d) {
        return "A new group key that every revoked device is left out of".into();
    }
    match d.steps.iter().find_map(|s| match s {
        Step::Deposit { amount, .. } => Some(*amount),
        _ => None,
    }) {
        Some(a) => format!(
            "A pledge of {} to this issue's bounty, paid into the repo's escrow",
            amount(a, false)
        ),
        None => d.label.clone(),
    }
}

fn removes(d: &Draft) -> bool {
    d.steps.iter().any(|s| matches!(s, Step::RemoveRevoked))
}

/// How long the write stays on Arkiv: ops are renewed, a group commit is applied once.
fn kept_for(d: &Draft, blocks: u64) -> String {
    match removes(d) {
        true => format!(
            "{} on Arkiv; members apply it on their next sync",
            kept(blocks)
        ),
        false => format!("{}, then it needs a renewal", kept(blocks)),
    }
}

/// `about 6 months` of a lifetime in blocks.
fn kept(blocks: u64) -> String {
    let days = blocks / charter_sync::repo::BLOCKS_PER_DAY;
    match days {
        0 => "less than a day".into(),
        1..60 => format!("about {days} days"),
        _ => format!("about {} months", (days + 15) / 30),
    }
}

fn signed_ops(d: &Draft) -> Vec<(String, String)> {
    d.steps
        .iter()
        .flat_map(|s| match s {
            Step::Op {
                type_name,
                action,
                payload,
                ..
            } => vec![(
                format!("{type_name} {action}"),
                serde_json::to_string_pretty(payload).unwrap_or_default(),
            )],
            Step::Deposit { amount, purpose } => vec![
                (
                    format!("transfer {} tGLM to the escrow", fmt_glm(*amount)),
                    String::new(),
                ),
                (
                    format!("xyz.arkiv.escrow deposit, purpose {purpose}"),
                    String::new(),
                ),
            ],
            Step::Release(_) => vec![],
            Step::RemoveRevoked => vec![(
                "MLS commit removing every revoked device's leaf (a new epoch key)".into(),
                String::new(),
            )],
        })
        .collect()
}

/// The wallet-to-control-address transfers the send makes before the ops, or why none.
fn top_up(e: &Estimate) -> String {
    let total = e.top_ups.iter().fold(U256::ZERO, |s, a| s + a);
    let n = match e.top_ups.len() {
        0 => return "none needed".into(),
        1 => String::new(),
        n => format!("{n} transfers, "),
    };
    format!(
        "{n}{} to the repo's paying address, fee at most {}",
        amount(total, true),
        amount(e.top_up_fee, true)
    )
}

/// The dialog of a priced draft.
fn dialog(app: &App, l: Layout, head: Head, id: String, d: &Draft, e: &Estimate) -> Confirm {
    let need = e.cost + e.value;
    let most = amount(need, true);
    let (q, verb) = ask(&d.label);
    let mut receipt = vec![
        ("Transactions", e.txs.to_string(), None),
        ("Records", e.entities.to_string(), None),
        ("Wallet top-up", top_up(e), None),
        (
            "Network fees, at most",
            String::new(),
            Some(amount(e.cost, true)),
        ),
    ];
    if e.value > U256::ZERO {
        receipt.push((
            "Paid into escrow",
            String::new(),
            Some(amount(e.value, true)),
        ));
    }
    receipt.push(("Total, at most", String::new(), Some(amount(need, true))));
    let short_of = (e.balance < need).then(|| {
        format!(
            "Your wallet holds {} and this may cost up to {most}. Nothing was spent. \
             Add funds with `chart fund`, then send again.",
            amount(e.balance, false)
        )
    });
    Confirm {
        l,
        id,
        back: d.back.clone(),
        button: format!("{verb} for up to {most}"),
        ask: q,
        publish: publishes(d),
        // The wallet alone, as the header chip shows it; the repo's paying address pays the rest.
        wallet_now: amount(e.wallet, false),
        wallet_after: amount(e.wallet.saturating_sub(e.from_wallet), false),
        takes: format!("about {} s", 2 * e.txs + 2),
        kept: kept_for(d, e.lifetime),
        most,
        receipt,
        short_of,
        quota: app.shared.quota_wait(),
        address: app
            .writer
            .as_ref()
            .map(|w| w.address.to_string())
            .unwrap_or_default(),
        ops: signed_ops(d),
        r: head,
    }
}

fn confirm_page(
    app: &App,
    pref: &Pref,
    rid: &str,
    id: String,
    d: &Draft,
    e: &Estimate,
) -> Response {
    let r = match open(app, pref, rid) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let (mut l, head) = frame(app, pref, &r, "", "Confirm write");
    l.crumbs.push((String::new(), "confirm".into()));
    html(&dialog(app, l, head, id, d, e))
}

fn writer(app: &App, pref: &Pref) -> Result<Arc<Writer>, Response> {
    app.writer
        .clone()
        .ok_or_else(|| error_page(app, pref, StatusCode::FORBIDDEN, WRITES_OFF))
}

/// `POST /r/<rid>/write`: the form, priced; nothing is signed.
pub async fn prepare(
    State(app): State<Arc<App>>,
    pref: Pref,
    Path(rid): Path<String>,
    Form(f): Form<form::Fields>,
) -> Response {
    let w = match writer(&app, &pref) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let Ok(id) = parse_id(&rid) else {
        return error_page(&app, &pref, StatusCode::NOT_FOUND, "No such repo.");
    };
    let d = match form::draft(&id, &f) {
        Ok(d) => d,
        Err(e) => return error_page(&app, &pref, StatusCode::BAD_REQUEST, &e),
    };
    let (draft_id, d, e) = match w.prepare(d).await {
        Ok(x) => x,
        Err(e) => {
            let msg = format!("This write cannot be priced: {e}");
            return error_page(&app, &pref, StatusCode::BAD_REQUEST, &msg);
        }
    };
    if prefs::skips(&pref.0, &e, app.shared.quota_wait().is_some())
        && w.confirm(&id, &draft_id, true)
    {
        return to_status(&id, &draft_id);
    }
    blocking(move || confirm_page(&app, &pref, &rid, draft_id, &d, &e)).await
}

fn to_status(rid: &charter_core::Rid, id: &str) -> Response {
    let to = format!("/r/{}/write/{id}", hex32(rid));
    (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
}

/// `GET /r/<rid>/write` (a reload after a boosted form post): back to the repo, unwritten.
pub async fn to_repo(Path(rid): Path<String>) -> Response {
    let to = parse_id(&rid).map_or_else(|_| "/".to_owned(), |r| format!("/r/{}", hex32(&r)));
    (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
}

/// `POST /r/<rid>/write/<id>`: sign and send, then show its status.
pub async fn confirm(
    State(app): State<Arc<App>>,
    pref: Pref,
    Path((rid, id)): Path<(String, String)>,
) -> Response {
    let w = match writer(&app, &pref) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let Ok(r) = parse_id(&rid) else {
        return error_page(&app, &pref, StatusCode::NOT_FOUND, "No such repo.");
    };
    // A multi-tx write cut off by a 429 leaves partial state; the draft stays for later.
    if let Some(q) = app.shared.quota_wait() {
        let msg = format!("{q}; nothing was written, confirm again then.");
        return error_page(&app, &pref, StatusCode::SERVICE_UNAVAILABLE, &msg);
    }
    if !w.confirm(&r, &id, false) {
        let msg = "This confirmation is unknown, expired or already sent; nothing was written.";
        return error_page(&app, &pref, StatusCode::NOT_FOUND, msg);
    }
    to_status(&r, &id)
}

/// The progress steps and toast of a sent write (`toast.html`).
#[derive(Debug, Clone)]
pub struct Toast {
    pub hex: String,
    pub id: String,
    /// `sending`, `sent`, `confirmed` or `failed`.
    pub state: &'static str,
    pub s: Status,
    pub txs: Vec<String>,
    pub ids: Vec<String>,
    pub block: String,
    /// Every op it wrote is in the local cache (`cobs.json`), so pages show it.
    pub visible: bool,
    /// Fees paid, rounded up; the estimate until every receipt is in.
    pub cost: Amount,
    /// Seconds from confirm to the block.
    pub took: u64,
    /// The small-write limit it was sent under, when the dialog was skipped.
    pub limit: Option<String>,
    /// Rendered as the polled fragment (which also refreshes the header's wallet chip).
    pub part: bool,
}

impl Toast {
    fn new(hex: String, id: String, s: Status, visible: bool, limit: Option<String>) -> Self {
        let state = match (&s.error, s.block, s.txs.is_empty()) {
            (Some(_), _, _) => "failed",
            (None, Some(_), _) => "confirmed",
            (None, None, true) => "sending",
            (None, None, false) => "sent",
        };
        let block = s.block.map(group).unwrap_or_default();
        Self {
            txs: s.txs.iter().map(|h| format!("{h}")).collect(),
            ids: s.ids.iter().map(hex32).collect(),
            cost: amount(s.spent().0, true),
            took: s.done_ms.saturating_sub(s.started_ms).div_ceil(1000),
            limit: limit.filter(|_| s.skipped),
            part: false,
            hex,
            id,
            state,
            block,
            visible,
            s,
        }
    }

    /// Polling stops at the block; the live stream's op event shows "visible".
    pub fn done(&self) -> bool {
        matches!(self.state, "confirmed" | "failed")
    }

    /// It moved funds besides fees (an escrow deposit).
    pub fn moved(&self) -> bool {
        self.s.est.value > U256::ZERO
    }
}

/// Every op id of `s` is in the repo's `cobs.json`.
fn visible(dir: &std::path::Path, s: &Status) -> bool {
    if s.label == form::REMOVE_REVOKED {
        // No op to find: a round that ran after the commit landed counts no revoked leaf.
        let v = cache::read::<cache::RepoView>(&dir.join(cache::VIEW))
            .ok()
            .flatten();
        return s.done_ms > 0
            && v.is_some_and(|v| v.revoked_leaves == 0 && v.synced_ms > s.done_ms);
    }
    if s.ids.is_empty() {
        return false;
    }
    let ops = cache::read::<cache::Cobs>(&dir.join(cache::COBS))
        .ok()
        .flatten();
    let have: std::collections::BTreeSet<&str> = ops
        .as_ref()
        .map(|c| c.ops.iter().map(|o| o.id.as_str()).collect())
        .unwrap_or_default();
    s.ids.iter().all(|i| have.contains(hex32(i).as_str()))
}

/// `1284771` as `1,284,771`.
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[derive(Template)]
#[template(path = "toast_part.html")]
pub struct Part {
    pub l: Layout,
    pub t: Toast,
}

#[derive(Template)]
#[template(path = "status.html")]
pub struct StatusPage {
    pub l: Layout,
    pub r: Head,
    pub t: Toast,
}

/// `GET /r/<rid>/write/<id>[?part=1]`.
pub async fn status(
    State(app): State<Arc<App>>,
    pref: Pref,
    Path((rid, id)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let s = app.writer.as_ref().and_then(|w| w.status(&id));
    let Some(s) = s else {
        return error_page(
            &app,
            &pref,
            StatusCode::NOT_FOUND,
            "No such write in this session.",
        );
    };
    blocking(move || {
        let r = match open(&app, &pref, &rid) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let shown = visible(&r.dir, &s);
        let mut t = Toast::new(r.hex.clone(), id, s, shown, pref.0.skip_under.clone());
        if q.contains_key("part") {
            t.part = true;
            let l = Layout::new(&app, &pref, "");
            return html(&Part { l, t });
        }
        let (mut l, mut head) = frame(&app, &pref, &r, "", &t.s.label);
        // The removal this page follows is what the repo's notice asks for.
        if t.s.label == form::REMOVE_REVOKED && t.s.error.is_none() {
            head.revoked_leaves = 0;
        }
        l.crumbs.push((String::new(), "write".into()));
        html(&StatusPage { l, r: head, t })
    })
    .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn blocks_are_grouped_by_thousands() {
        assert_eq!(super::group(1_284_771), "1,284,771");
        assert_eq!(super::group(999), "999");
        assert_eq!(super::group(1_000), "1,000");
    }
}
