//! Live updates (docs/p6/ui.md 4.5). `GET /r/<rid>/events` is one server-sent-events
//! stream per open page: each sync round of the repo sends what it found (`push`, `op`,
//! then `sync`), and a page that fell behind gets `resync`. `assets/live.js` then refetches
//! the regions marked `data-live` for that event; `GET /r/<rid>/verify` is the
//! verification panel alone, refetched on every `sync`. Opening a stream puts its repo on
//! screen for the sync loop. With JS off neither is used.

use std::convert::Infallible;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use charter_sync::ids::parse_id;
use tokio::sync::broadcast::error::RecvError;

use super::{App, Head, Theme, blocking, frame, html, open};

pub async fn events(State(app): State<Arc<App>>, Path(rid): Path<String>) -> Response {
    let Ok(rid) = parse_id(&rid) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    app.shared.focus(rid);
    let stream = futures_util::stream::unfold(app.shared.subscribe(), move |mut rx| async move {
        loop {
            let name = match rx.recv().await {
                Ok(e) if e.rid == rid => e.kind.name(),
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => "resync",
                Err(RecvError::Closed) => return None,
            };
            let ev = Event::default().event(name).data(name);
            return Some((Ok::<_, Infallible>(ev), rx));
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[derive(Template)]
#[template(path = "verify_part.html")]
pub struct VerifyPart {
    pub r: Head,
}

pub async fn verify(State(app): State<Arc<App>>, t: Theme, Path(rid): Path<String>) -> Response {
    blocking(move || {
        let r = match open(&app, &t, &rid) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (_, head) = frame(&app, &t, &r, "", "");
        html(&VerifyPart { r: head })
    })
    .await
}
