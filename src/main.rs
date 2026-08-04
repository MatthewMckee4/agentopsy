mod trace;
mod view;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;

use crate::trace::TraceCache;

const PAGE_CACHE_TTL: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct AppState {
    cache: Arc<Mutex<AppCache>>,
    trace_root: PathBuf,
}

#[derive(Default)]
struct AppCache {
    page: Option<CachedPage>,
    traces: TraceCache,
}

struct CachedPage {
    body: Bytes,
    rendered_at: Instant,
}

impl CachedPage {
    /// Coalesces refresh bursts; the body remains resident until the next render.
    fn body_if_fresh(&self) -> Option<Bytes> {
        (self.rendered_at.elapsed() < PAGE_CACHE_TTL).then(|| self.body.clone())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let state = AppState {
        cache: Arc::new(Mutex::new(AppCache::default())),
        trace_root: PathBuf::from(home).join(".codex/sessions"),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .with_state(state)
        .layer(middleware::map_response(add_security_headers));
    let address = SocketAddr::from(([127, 0, 0, 1], 8765));
    let listener = TcpListener::bind(address).await?;
    print_url(address);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index(State(state): State<AppState>) -> Result<impl IntoResponse, (StatusCode, String)> {
    let html = tokio::task::spawn_blocking(move || {
        let mut cache = state
            .cache
            .lock()
            .map_err(|_| "trace cache lock is poisoned".to_owned())?;
        if let Some(body) = cache.page.as_ref().and_then(CachedPage::body_if_fresh) {
            return Ok(body);
        }
        let dashboard = cache.traces.load(&state.trace_root);
        let body = Bytes::from(view::render(&dashboard).map_err(|error| error.to_string())?);
        cache.page = Some(CachedPage {
            body: body.clone(),
            rendered_at: Instant::now(),
        });
        drop(cache);
        Ok::<_, String>(body)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Html(html)))
}

async fn health() -> &'static str {
    "ok\n"
}

async fn add_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[expect(clippy::print_stdout, reason = "the CLI contract prints the local URL")]
fn print_url(address: SocketAddr) {
    println!("Agentopsy: http://{address}");
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use axum::body::{Body, Bytes};
    use axum::http::{HeaderValue, Response, header};

    use super::{CachedPage, PAGE_CACHE_TTL, add_security_headers};

    #[test]
    fn reuses_only_fresh_page_bodies() {
        let mut page = CachedPage {
            body: Bytes::from_static(b"dashboard"),
            rendered_at: Instant::now(),
        };

        assert_eq!(page.body_if_fresh(), Some(page.body.clone()));
        page.rendered_at -= PAGE_CACHE_TTL;
        assert_eq!(page.body_if_fresh(), None);
    }

    #[tokio::test]
    async fn applies_security_headers_to_every_response() {
        let response = add_security_headers(Response::new(Body::empty())).await;
        let headers = response.headers();

        for (name, value) in [
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ] {
            assert_eq!(headers.get(name), Some(&HeaderValue::from_static(value)));
        }
    }
}
