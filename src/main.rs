mod trace;
mod view;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use tokio::net::TcpListener;

use crate::trace::TraceCache;

#[derive(Clone)]
struct AppState {
    cache: Arc<Mutex<TraceCache>>,
    trace_root: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let state = AppState {
        cache: Arc::new(Mutex::new(TraceCache::default())),
        trace_root: PathBuf::from(home).join(".codex/sessions"),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .with_state(state);
    let address = SocketAddr::from(([127, 0, 0, 1], 8765));
    let listener = TcpListener::bind(address).await?;
    print_url(address);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index(State(state): State<AppState>) -> Result<impl IntoResponse, (StatusCode, String)> {
    let dashboard = tokio::task::spawn_blocking(move || {
        let mut cache = state
            .cache
            .lock()
            .map_err(|_| "trace cache lock is poisoned".to_owned())?;
        Ok::<_, String>(cache.load(&state.trace_root))
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    let html = view::render(&dashboard).map_err(internal_error)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Html(html)))
}

async fn health() -> &'static str {
    "ok\n"
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[expect(clippy::print_stdout, reason = "the CLI contract prints the local URL")]
fn print_url(address: SocketAddr) {
    println!("Agentopsy: http://{address}");
}
