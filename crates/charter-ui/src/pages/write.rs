//! Write pages (docs/p6/ui.md 4.4, ui-design.md 5): `POST /r/<rid>/write` prices a form
//! and shows the confirm dialog; `POST /r/<rid>/write/<id>` signs and sends it once;
//! `GET /r/<rid>/write/<id>` shows sent, then confirmed (htmx polls `?part=1`; with JS
//! off a reload does the same). Nothing is written on a GET.

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

use super::{App, Head, Layout, Theme, blocking, error_page, frame, html, open, short};
use crate::writes::{Draft, Estimate, Status, Step, Writer, form};

pub const WRITES_OFF: &str = "Writes are off: there are no keys in $CHARTER_HOME. \
    Run `chart keys init`, fund the wallet, then restart `chart ui`.";

/// `0.0042 tGLM`.
pub fn tglm(wei: U256) -> String {
    format!("{} tGLM", fmt_glm(wei))
}

#[derive(Template)]
#[template(path = "confirm.html")]
pub struct Confirm {
    pub l: Layout,
    pub r: Head,
    pub id: String,
    pub back: String,
    /// `(label, value, mono)`.
    pub rows: Vec<(&'static str, String, bool)>,
    pub cost: String,
    pub short_of: Option<String>,
    /// The RPC refused or nearly ran out of quota: Confirm is disabled until it resets.
    pub quota: Option<String>,
    /// What will be signed, one `type action` line and its JSON payload per op.
    pub ops: Vec<(String, String)>,
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
                    format!("transfer {} to the escrow", tglm(*amount)),
                    String::new(),
                ),
                (
                    format!("xyz.arkiv.escrow deposit, purpose {purpose}"),
                    String::new(),
                ),
            ],
            Step::Release(_) => vec![],
        })
        .collect()
}

/// The wallet-to-control-address transfers the send makes before the ops, or why none.
fn top_up_row(e: &Estimate) -> (&'static str, String, bool) {
    let total = e.top_ups.iter().fold(U256::ZERO, |s, a| s + a);
    let v = match e.top_ups.len() {
        0 => {
            return (
                "Wallet top-up",
                "none needed: the control address covers this write".into(),
                false,
            );
        }
        1 => String::new(),
        n => format!("{n} transfers, "),
    };
    let fee = tglm(e.top_up_fee);
    (
        "Wallet top-up",
        format!(
            "{v}{} to the control address, fee at most {fee}",
            tglm(total)
        ),
        true,
    )
}

fn confirm_page(
    app: &App,
    theme: &Theme,
    rid: &str,
    id: String,
    d: &Draft,
    e: &Estimate,
) -> Response {
    let r = match open(app, theme, rid) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let (mut l, head) = frame(app, theme, &r, "", "Confirm write");
    l.crumbs.push((String::new(), "confirm".into()));
    let need = e.cost + e.value;
    let mut rows = vec![
        ("Action", d.label.clone(), false),
        ("Repo", head.name.clone(), false),
        ("Transactions", e.txs.to_string(), false),
        top_up_row(e),
        ("Entities", e.entities.to_string(), false),
        ("Estimated cost", format!("at most {}", tglm(e.cost)), true),
    ];
    if e.value > U256::ZERO {
        rows.push(("Moves", tglm(e.value), true));
    }
    rows.push(("Balance after", tglm(e.balance.saturating_sub(need)), true));
    let short_of = (e.balance < need).then(|| {
        format!(
            "The wallet and the repo address hold {}; this write may need {}. Run `chart fund` first.",
            tglm(e.balance),
            tglm(need)
        )
    });
    html(&Confirm {
        l,
        r: head,
        id,
        back: d.back.clone(),
        rows,
        cost: tglm(need),
        short_of,
        quota: app.shared.quota_wait(),
        ops: signed_ops(d),
    })
}

fn writer(app: &App, theme: &Theme) -> Result<Arc<Writer>, Response> {
    app.writer
        .clone()
        .ok_or_else(|| error_page(app, theme, StatusCode::FORBIDDEN, WRITES_OFF))
}

/// `POST /r/<rid>/write`: the form, priced; nothing is signed.
pub async fn prepare(
    State(app): State<Arc<App>>,
    theme: Theme,
    Path(rid): Path<String>,
    Form(f): Form<form::Fields>,
) -> Response {
    let w = match writer(&app, &theme) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let Ok(id) = parse_id(&rid) else {
        return error_page(&app, &theme, StatusCode::NOT_FOUND, "No such repo.");
    };
    let d = match form::draft(&id, &f) {
        Ok(d) => d,
        Err(e) => return error_page(&app, &theme, StatusCode::BAD_REQUEST, &e),
    };
    let (draft_id, d, e) = match w.prepare(d).await {
        Ok(x) => x,
        Err(e) => {
            let msg = format!("This write cannot be priced: {e}");
            return error_page(&app, &theme, StatusCode::BAD_REQUEST, &msg);
        }
    };
    blocking(move || confirm_page(&app, &theme, &rid, draft_id, &d, &e)).await
}

/// `GET /r/<rid>/write` (a reload after a boosted form post): back to the repo, unwritten.
pub async fn to_repo(Path(rid): Path<String>) -> Response {
    let to = parse_id(&rid).map_or_else(|_| "/".to_owned(), |r| format!("/r/{}", hex32(&r)));
    (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
}

/// `POST /r/<rid>/write/<id>`: sign and send, then show its status.
pub async fn confirm(
    State(app): State<Arc<App>>,
    theme: Theme,
    Path((rid, id)): Path<(String, String)>,
) -> Response {
    let w = match writer(&app, &theme) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let Ok(r) = parse_id(&rid) else {
        return error_page(&app, &theme, StatusCode::NOT_FOUND, "No such repo.");
    };
    // A multi-tx write cut off by a 429 leaves partial state; the draft stays for later.
    if let Some(q) = app.shared.quota_wait() {
        let msg = format!("{q}; nothing was written, confirm again then.");
        return error_page(&app, &theme, StatusCode::SERVICE_UNAVAILABLE, &msg);
    }
    if !w.confirm(&r, &id) {
        let msg = "This confirmation is unknown, expired or already sent; nothing was written.";
        return error_page(&app, &theme, StatusCode::NOT_FOUND, msg);
    }
    let to = format!("/r/{}/write/{id}", hex32(&r));
    (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
}

/// The status panel and toast of a sent write (`toast.html`).
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
}

impl Toast {
    fn new(hex: String, id: String, s: Status) -> Self {
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
            hex,
            id,
            state,
            block,
            s,
        }
    }

    pub fn done(&self) -> bool {
        matches!(self.state, "confirmed" | "failed")
    }

    pub fn short(&self, s: &str) -> String {
        short(s.trim_start_matches("0x"))
    }
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
#[template(path = "toast.html")]
pub struct Part {
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
    theme: Theme,
    Path((rid, id)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let s = app.writer.as_ref().and_then(|w| w.status(&id));
    let Some(s) = s else {
        return error_page(
            &app,
            &theme,
            StatusCode::NOT_FOUND,
            "No such write in this session.",
        );
    };
    blocking(move || {
        let r = match open(&app, &theme, &rid) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let t = Toast::new(r.hex.clone(), id, s);
        if q.contains_key("part") {
            return html(&Part { t });
        }
        let (mut l, head) = frame(&app, &theme, &r, "", &t.s.label);
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
