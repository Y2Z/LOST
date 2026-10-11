//! Web server: the Web API and the embedded web UI (ARCHITECTURE.md §10).
//!
//! - `GET /api/find?q=QUERY&l=[OFFSET,]COUNT` — search; the same JSON as `lost -f json`
//! - `GET /api/suggest?q=TEXT` — autocomplete suggestions, a JSON array of strings
//! - `GET /api/page?url=URL[&hl=QUERY]` — the stored text of one page (`cache:`),
//!   with QUERY's words marked; 404 if it isn't in the database
//! - `GET /api/status` — version and document count
//! - `GET /` and its assets — the web UI, compiled into the binary; with
//!   `?q=`, a page of plain HTML and CSS with the results (or a stored
//!   page), for browsers without JavaScript (html.rs)
//!
//! Queries are never logged or stored. Nothing is ever fetched from elsewhere.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

use crate::query::Limit;
use crate::{Lost, Result, html};

/// The web UI, compiled into the binary so the server is a single file.
const INDEX_HTML: &str = include_str!("../res/web/index.html");
const LOST_CSS: &str = include_str!("../res/web/lost.css");
const LOST_JS: &str = include_str!("../res/web/lost.js");
const MIKADO_JS: &str = include_str!("../res/web/mikado.light.min.js");
const FAVICON_SVG: &str = include_str!("../res/images/lost.svg");

/// Suggestions returned per request.
const SUGGESTIONS: usize = 8;

/// The page only ever talks to this server, and nothing else may frame it.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     img-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; \
     frame-ancestors 'none'";

/// Server settings.
#[derive(Debug, Clone)]
pub struct Options {
    pub address: String,
    pub port: u16,
    /// Largest COUNT `/api/find` returns at once.
    pub max_results: usize,
    /// COUNT when `l` isn't given.
    pub default_limit: usize,
    /// Show the "Cached" link after each result's address (`search.cached_links`).
    pub cached_links: bool,
}

struct AppState {
    lost: Arc<Lost>,
    options: Options,
}

/// Runs the server until Ctrl+C (or SIGTERM). Blocks.
pub fn run(lost: Arc<Lost>, options: Options) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(lost, options))
}

async fn serve(lost: Arc<Lost>, options: Options) -> Result<()> {
    let address = options.address.clone();
    let port = options.port;
    let listener = tokio::net::TcpListener::bind((address.as_str(), port))
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AddrInUse => format!(
                "port {port} on {address} is already in use \
                 (another LOST server? pick another port with -p)"
            ),
            std::io::ErrorKind::PermissionDenied => {
                format!("not allowed to listen on {address}:{port}")
            }
            _ => format!("can't listen on {address}:{port}: {e}"),
        })?;

    let state = Arc::new(AppState { lost, options });
    let app = Router::new()
        .route("/", get(index))
        .route(
            "/lost.css",
            get(|| asset("text/css; charset=utf-8", LOST_CSS)),
        )
        .route(
            "/lost.js",
            get(|| asset("text/javascript; charset=utf-8", LOST_JS)),
        )
        .route(
            "/mikado.light.min.js",
            get(|| asset("text/javascript; charset=utf-8", MIKADO_JS)),
        )
        .route("/favicon.svg", get(|| asset("image/svg+xml", FAVICON_SVG)))
        .route("/api/find", get(find))
        .route("/api/suggest", get(suggest))
        .route("/api/page", get(page))
        .route("/api/status", get(status))
        .fallback(not_found)
        .layer(axum::middleware::map_response(security_headers))
        .with_state(state);

    let shown = if address == "0.0.0.0" || address == "::" {
        format!("http://<this machine's address>:{port} (all interfaces)")
    } else if address.contains(':') {
        format!("http://[{address}]:{port}")
    } else {
        format!("http://{address}:{port}")
    };
    eprintln!("LOST is serving {shown}. Press Ctrl+C to stop.");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    eprintln!("LOST server stopped.");
    Ok(())
}

/// Ctrl+C, or SIGTERM (systemd, Docker, kill).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// The web UI. A request with a query (`/?q=…&p=N`, `/?q=cache:URL&hl=…`)
/// never comes from it (it changes the address itself), so it gets the
/// page without JavaScript: plain HTML and CSS (html.rs).
async fn index(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(query) = params.get("q").map(|q| crate::text::single_line(q)) else {
        return (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            html::with_cached_links(INDEX_HTML, state.options.cached_links),
        )
            .into_response();
    };
    let page = params
        .get("p")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, html::MAX_PAGE);
    let highlight = params.get("hl").cloned().unwrap_or_default();

    let task_state = state.clone();
    let task_query = query.clone();
    let rendered = tokio::task::spawn_blocking(move || -> (StatusCode, String) {
        let lost = &task_state.lost;
        let query = task_query;
        if query.is_empty() {
            return (StatusCode::OK, String::new());
        }
        match crate::query::cache_url(&query) {
            Some(Err(e)) => (StatusCode::BAD_REQUEST, html::message(&e)),
            Some(Ok(url)) => {
                let hl = (!highlight.is_empty()).then_some(highlight.as_str());
                match lost.page(&url, hl) {
                    Ok(Some(found)) => (StatusCode::OK, html::stored(Some(&found), &url)),
                    Ok(None) => (StatusCode::NOT_FOUND, html::stored(None, &url)),
                    Err(e) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        html::message(&e.to_string()),
                    ),
                }
            }
            None => {
                let limit = Limit::new((page - 1) * html::PAGE, html::PAGE);
                match lost.search(&query, limit) {
                    Ok(results) => (StatusCode::OK, html::results(&results, page)),
                    Err(e) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        html::message(&e.to_string()),
                    ),
                }
            }
        }
    })
    .await;
    let (status, content) = rendered.unwrap_or_else(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            html::message("The search failed."),
        )
    });
    let body = html::with_cached_links(
        &html::document(&query, &content),
        state.options.cached_links,
    );
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

async fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found\n").into_response()
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, axum::Json(json!({ "error": message.into() }))).into_response()
}

async fn find(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = params.get("q").cloned().unwrap_or_default();
    let mut limit = match params.get("l") {
        Some(l) => match l.parse::<Limit>() {
            Ok(limit) => limit,
            Err(e) => return error(StatusCode::BAD_REQUEST, e),
        },
        None => Limit::new(0, state.options.default_limit),
    };
    limit.count = limit.count.min(state.options.max_results);

    // Searching is blocking work: keep it off the async threads.
    let task_state = state.clone();
    let result = tokio::task::spawn_blocking(move || task_state.lost.search(&query, limit)).await;
    match result {
        Ok(Ok(results)) => axum::Json(results).into_response(),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "search failed"),
    }
}

async fn suggest(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let text = params.get("q").cloned().unwrap_or_default();
    let task_state = state.clone();
    let result =
        tokio::task::spawn_blocking(move || task_state.lost.suggest(&text, SUGGESTIONS)).await;
    match result {
        Ok(Ok(suggestions)) => axum::Json(suggestions).into_response(),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "suggest failed"),
    }
}

async fn page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(url) = params
        .get("url")
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
    else {
        return error(StatusCode::BAD_REQUEST, "url is missing");
    };
    let highlight = params.get("hl").cloned();
    let task_state = state.clone();
    let result =
        tokio::task::spawn_blocking(move || task_state.lost.page(&url, highlight.as_deref())).await;
    match result {
        Ok(Ok(Some(page))) => axum::Json(page).into_response(),
        Ok(Ok(None)) => error(StatusCode::NOT_FOUND, "This page isn't in the database."),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "lookup failed"),
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    axum::Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "documents": state.lost.document_count(),
    }))
    .into_response()
}
