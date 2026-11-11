//! charter-ui: the local web UI behind `chart ui` (docs/p6/ui.md, ui-v2.md).
//!
//! Pages render from `$CHARTER_HOME/repos/<rid>/` only (git cache, `view.json`,
//! `cobs.json`) and never take the repo lock; one background loop ([`sync_loop`]) keeps
//! those files fresh from Arkiv. [`security`] guards every request.

// Handlers return the error page itself as `Err(Response)`; it is built once per request.
#![allow(clippy::result_large_err)]

mod assets;
pub mod cache;
pub mod diff;
pub mod fold;
pub mod gitread;
pub mod highlight;
pub mod markdown;
pub mod pace;
mod pages;
pub mod prefs;
pub mod security;
pub mod spend;
pub mod sync_loop;
pub mod writes;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use charter_core::Rid;
use charter_store::Arkiv;
use charter_sync::{Config, SyncError};

pub use pages::App;
pub use sync_loop::Shared;

#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error(transparent)]
    Sync(#[from] SyncError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("cache: {0}")]
    Cache(String),
}

/// The router with every page, the assets and the guard.
pub fn router(app: Arc<App>, auth: Arc<security::Auth>) -> Router {
    use axum::routing::post;
    use pages::{cobs, code, forms, history, live, program, repos, settings, wallet, write};
    Router::new()
        .route("/", get(repos::index))
        .route("/wallet", get(wallet::page))
        .route("/wallet/chip", get(wallet::chip_part))
        .route("/wallet/refresh", post(wallet::refresh))
        .route("/settings", get(settings::page).post(settings::save))
        .route("/settings/theme", post(settings::theme_toggle))
        .route("/r/{rid}", get(repos::home))
        .route("/r/{rid}/branches", get(repos::branches))
        .route("/r/{rid}/tree/{rev}", get(code::tree_root))
        .route("/r/{rid}/tree/{rev}/{*path}", get(code::tree))
        .route("/r/{rid}/blob/{rev}/{*path}", get(code::blob))
        .route("/r/{rid}/raw/{rev}/{*path}", get(code::raw))
        .route("/r/{rid}/log/{rev}", get(history::log))
        .route("/r/{rid}/commit/{sha}", get(history::commit))
        .route("/r/{rid}/events", get(live::events))
        .route("/r/{rid}/verify", get(live::verify))
        .route("/r/{rid}/write", post(write::prepare).get(write::to_repo))
        .route(
            "/r/{rid}/write/{id}",
            post(write::confirm).get(write::status),
        )
        .route("/r/{rid}/issues/new", get(forms::new_issue))
        .route("/r/{rid}/releases/new", get(forms::new_release))
        .route("/r/{rid}/issues", get(cobs::issues))
        .route("/r/{rid}/issues/{id}", get(cobs::issue))
        .route("/r/{rid}/patches", get(cobs::patches))
        .route("/r/{rid}/patches/{id}", get(cobs::patch))
        .route("/r/{rid}/releases", get(program::releases))
        .route("/r/{rid}/program", get(program::program))
        .route("/r/{rid}/rules", get(program::program))
        .route("/assets/{name}", get(assets::serve))
        .fallback(pages::not_found)
        .with_state(app)
        .layer(axum::middleware::from_fn_with_state(auth, security::guard))
        // Outermost, so denials are gzipped too; tower-http skips images and event streams,
        // so each `/events` message is flushed as it is sent.
        // No BREACH exposure: the cookie is SameSite=Strict, so no cross-site request ever
        // gets a page with the CSRF token in it.
        .layer(tower_http::compression::CompressionLayer::new())
}

/// What `chart ui` passes in.
#[derive(Debug, Clone)]
pub struct Options {
    /// `0` picks a free port.
    pub port: u16,
    /// A repo to sync first even if it is not cached yet.
    pub want: Option<Rid>,
    /// The backend's `newHeads` socket; `None` polls (the fake backend).
    pub ws: Option<charter_store::heads::WsConfig>,
}

/// Binds `127.0.0.1`, starts the sync loop, prints the session URL and serves until killed.
///
/// Writes go through `a`; the sync loop reads through `reads`, which should bail on a 429
/// rather than wait (`RpcArkiv::with_max_rate_wait`).
pub async fn serve<A: Arkiv + 'static>(
    a: Arc<A>,
    reads: Arc<A>,
    cfg: Config,
    opts: Options,
) -> Result<(), UiError> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, opts.port))).await?;
    let port = listener.local_addr()?.port();
    let auth = Arc::new(security::Auth::new(port)?);
    let shared = Arc::new(Shared::default());
    if let Some(rid) = opts.want {
        shared.want(rid);
        shared.focus(rid);
    }
    let mut app = App::new(cfg.home.clone(), shared.clone(), auth.csrf.clone());
    // Keys stay in this process; without them every write form says why it is missing.
    if let Some(keys) = charter_sync::Keys::load(&cfg.home)? {
        app = app.with_writer(writes::spawn(a.clone(), cfg.clone(), keys));
    }
    sync_loop::spawn(reads, cfg, shared, opts.ws)?;
    let app = Arc::new(app);
    println!("{}", auth.url());
    axum::serve(listener, router(app, auth)).await?;
    Ok(())
}
