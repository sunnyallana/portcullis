//! HTTP transport.
//!
//! The same engine and the same MCP dispatch as the stdio server, with one
//! difference that matters: identity is resolved per request, so one process
//! can serve many callers with different scopes. That is what `--role` on the
//! stdio server could not do.
//!
//! Endpoints:
//!
//! | Path | Auth | Purpose |
//! |---|---|---|
//! | `POST /mcp` | yes | MCP, JSON-RPC over HTTP |
//! | `GET /api/approvals` | yes | Parked calls |
//! | `POST /api/approvals/{id}/approve` | yes | Release one |
//! | `POST /api/approvals/{id}/deny` | yes | Refuse one |
//! | `GET /healthz` | no | Process is up |
//! | `GET /readyz` | no | Backend answers |
//! | `GET /metrics` | no | Prometheus counters |
//! | `GET /` | no (page only) | Approvals console |
//!
//! TLS is not terminated here. Run it behind a reverse proxy, or on a loopback
//! interface with the proxy on the same host.

pub mod auth;
pub mod metrics;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value as Json2, json};
use sluice_core::{Caller, Error};
use sluice_engine::Engine;
use tokio::net::TcpListener;

use crate::auth::{AuthError, Authenticator, Principal};
use crate::metrics::Metrics;

/// The console, served at `/`.
const CONSOLE: &str = include_str!("console.html");

/// Server settings.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Address to bind.
    pub listen: SocketAddr,
    /// Serve the approvals console at `/`.
    pub console: bool,
    /// Largest accepted request body.
    pub max_body_bytes: usize,
    /// Deadline for a whole request.
    pub request_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: ([127, 0, 0, 1], 8080).into(),
            console: true,
            max_body_bytes: 256 * 1024,
            request_timeout: Duration::from_secs(60),
        }
    }
}

/// Shared handler state.
#[derive(Clone)]
struct AppState {
    engine: Arc<Engine>,
    auth: Arc<dyn Authenticator>,
    metrics: Arc<Metrics>,
    console: bool,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("auth", &self.auth.describe())
            .field("console", &self.console)
            .finish_non_exhaustive()
    }
}

/// Build the router. Exposed so tests can drive it without binding a port.
pub fn router(
    engine: Arc<Engine>,
    authenticator: Arc<dyn Authenticator>,
    config: &HttpConfig,
) -> Router {
    let state = AppState {
        engine,
        auth: authenticator,
        metrics: Arc::new(Metrics::new()),
        console: config.console,
    };

    Router::new()
        .route("/mcp", post(mcp).get(mcp_get_not_supported))
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/{id}/approve", post(approve))
        .route("/api/approvals/{id}/deny", post(deny))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_endpoint))
        .route("/", get(console))
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        .with_state(state)
}

/// Bind and serve until `shutdown` resolves.
pub async fn serve(
    engine: Arc<Engine>,
    authenticator: Arc<dyn Authenticator>,
    config: &HttpConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Error> {
    if authenticator.is_open() {
        tracing::warn!(
            "authentication is disabled; every caller is the same identity. Do not run this outside development."
        );
    }
    let app = router(engine, authenticator, config);
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|e| Error::Config(format!("cannot bind {}: {e}", config.listen)))?;
    let bound = listener
        .local_addr()
        .map_err(|e| Error::Config(format!("cannot read the bound address: {e}")))?;
    tracing::info!(address = %bound, "serving MCP over HTTP");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| Error::Config(format!("the HTTP server stopped: {e}")))
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Turn a principal into a caller this deployment knows.
///
/// Attributes from the token override the role's configured defaults. That is
/// deliberate and is the point of per-request identity: the role says what a
/// support agent may do, the token says which region this one covers. Only
/// claims the operator mapped can appear here.
fn resolve_caller(engine: &Engine, principal: &Principal) -> Result<Caller, AuthError> {
    for role in &principal.roles {
        if let Ok(mut caller) = engine.caller(role, &principal.id) {
            for (key, value) in &principal.attributes {
                caller.attributes.insert(key.clone(), value.clone());
            }
            return Ok(caller);
        }
    }
    Err(AuthError::NoRole(format!(
        "this deployment has no role matching {:?}",
        principal.roles
    )))
}

/// The error is boxed because a rejection is the rare path and an axum
/// `Response` is large enough that returning it by value widens every call.
async fn identify(state: &AppState, headers: &HeaderMap) -> Result<Caller, Box<Response>> {
    let principal = match state.auth.authenticate(headers).await {
        Ok(p) => p,
        Err(e) => {
            state.metrics.auth_failure(e.code());
            return Err(Box::new(auth_response(&e)));
        }
    };
    resolve_caller(&state.engine, &principal).map_err(|e| {
        state.metrics.auth_failure(e.code());
        Box::new(auth_response(&e))
    })
}

fn auth_response(e: &AuthError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::UNAUTHORIZED);
    let mut response = (
        status,
        Json(json!({ "error": e.code(), "message": e.to_string() })),
    )
        .into_response();
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
    }
    response
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

async fn mcp(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    let started = Instant::now();

    // Clients send this after initialising. An unknown revision is worth
    // saying so about rather than guessing.
    if let Some(version) = headers
        .get("mcp-protocol-version")
        .and_then(|v| v.to_str().ok())
    {
        if !sluice_mcp::SUPPORTED_PROTOCOLS.contains(&version) {
            state.metrics.request("/mcp", 400);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "unsupported_protocol_version",
                    "message": format!("this server speaks {:?}", sluice_mcp::SUPPORTED_PROTOCOLS),
                })),
            )
                .into_response();
        }
    }

    let caller = match identify(&state, &headers).await {
        Ok(c) => c,
        Err(response) => {
            state.metrics.request("/mcp", response.status().as_u16());
            return *response;
        }
    };

    let trimmed = body.trim_start_matches('\u{feff}');
    if trimmed.trim_start().starts_with('[') {
        state.metrics.request("/mcp", 400);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "batch_not_supported",
                "message": "send one JSON-RPC request per POST; batching was removed from MCP",
            })),
        )
            .into_response();
    }

    let request: sluice_mcp::Request = match serde_json::from_str(trimmed) {
        Ok(r) => r,
        Err(e) => {
            state.metrics.request("/mcp", 400);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": Json2::Null,
                    "error": { "code": -32700, "message": format!("could not parse the request: {e}") },
                })),
            )
                .into_response();
        }
    };

    let action = (request.method == "tools/call").then(|| {
        request
            .params
            .get("name")
            .and_then(Json2::as_str)
            .unwrap_or("?")
            .to_owned()
    });

    let response = sluice_mcp::handle(&state.engine, &caller, request).await;

    if let Some(action) = action {
        let outcome = response
            .as_ref()
            .and_then(|r| r.result.as_ref())
            .and_then(|r| r.get("isError"))
            .and_then(Json2::as_bool)
            .map_or("ok", |is_error| if is_error { "error" } else { "ok" });
        state.metrics.tool_call(&action, outcome, started.elapsed());
    }

    match response {
        // A notification gets no body, per JSON-RPC.
        None => {
            state.metrics.request("/mcp", 202);
            StatusCode::ACCEPTED.into_response()
        }
        Some(r) => {
            state.metrics.request("/mcp", 200);
            Json(r).into_response()
        }
    }
}

async fn mcp_get_not_supported() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({
            "error": "sse_not_supported",
            "message": "this server answers MCP over POST only; there is no server-initiated stream",
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Approvals
// ---------------------------------------------------------------------------

async fn list_approvals(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let caller = match identify(&state, &headers).await {
        Ok(c) => c,
        Err(r) => {
            state.metrics.request("/api/approvals", r.status().as_u16());
            return *r;
        }
    };

    let pending: Vec<Json2> = state
        .engine
        .approvals()
        .pending()
        .into_iter()
        .map(|a| {
            // Whether this viewer could actually decide it, so the console can
            // grey out the buttons instead of failing after the click.
            let decidable = state.engine.may_decide(&a, &caller).is_ok();
            json!({
                "id": a.id,
                "action": a.action,
                "args": a.args,
                "caller": a.caller.id,
                "role": a.caller.role,
                "reason": a.reason,
                "created": a.created,
                "decidable": decidable,
            })
        })
        .collect();

    state.metrics.request("/api/approvals", 200);
    Json(json!({ "pending": pending, "viewer": caller.id })).into_response()
}

async fn approve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let caller = match identify(&state, &headers).await {
        Ok(c) => c,
        Err(r) => {
            state
                .metrics
                .request("/api/approvals/approve", r.status().as_u16());
            return *r;
        }
    };
    match state.engine.approve(&id, &caller).await {
        Ok(result) => {
            state.metrics.request("/api/approvals/approve", 200);
            Json(json!({ "released": id, "result": result.to_json() })).into_response()
        }
        Err(e) => {
            let response = engine_error(&e);
            state
                .metrics
                .request("/api/approvals/approve", response.status().as_u16());
            response
        }
    }
}

async fn deny(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let caller = match identify(&state, &headers).await {
        Ok(c) => c,
        Err(r) => {
            state
                .metrics
                .request("/api/approvals/deny", r.status().as_u16());
            return *r;
        }
    };
    match state.engine.deny(&id, &caller) {
        Ok(a) => {
            state.metrics.request("/api/approvals/deny", 200);
            Json(json!({ "denied": a.id, "action": a.action })).into_response()
        }
        Err(e) => {
            let response = engine_error(&e);
            state
                .metrics
                .request("/api/approvals/deny", response.status().as_u16());
            response
        }
    }
}

fn engine_error(e: &Error) -> Response {
    let status = match e {
        Error::Denied { .. } => StatusCode::FORBIDDEN,
        Error::UnknownAction(_) => StatusCode::NOT_FOUND,
        Error::Approval(m) if m.contains("no approval request") => StatusCode::NOT_FOUND,
        Error::Approval(_) => StatusCode::CONFLICT,
        Error::BadArgument { .. } | Error::Validation { .. } => StatusCode::BAD_REQUEST,
        Error::LimitExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(json!({ "error": e.code(), "message": e.to_string() })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Operational endpoints
// ---------------------------------------------------------------------------

async fn healthz(State(state): State<AppState>) -> Response {
    state.metrics.request("/healthz", 200);
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") })).into_response()
}

async fn readyz(State(state): State<AppState>) -> Response {
    match state.engine.health().await {
        Ok(()) => {
            state.metrics.request("/readyz", 200);
            Json(json!({ "status": "ready", "backend": state.engine.backend_description() }))
                .into_response()
        }
        Err(e) => {
            state.metrics.request("/readyz", 503);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "unavailable", "message": e.to_string() })),
            )
                .into_response()
        }
    }
}

async fn metrics_endpoint(State(state): State<AppState>) -> Response {
    let body = state.metrics.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

async fn console(State(state): State<AppState>) -> Response {
    if !state.console {
        state.metrics.request("/", 404);
        return StatusCode::NOT_FOUND.into_response();
    }
    state.metrics.request("/", 200);
    Html(CONSOLE).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKey, ApiKeyAuthenticator};
    use sluice_core::Value;
    use std::collections::BTreeMap;

    #[test]
    fn engine_errors_map_to_sensible_statuses() {
        assert_eq!(
            engine_error(&Error::Denied {
                role: "r".into(),
                action: "a".into()
            })
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            engine_error(&Error::Approval("no approval request `x`".into())).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            engine_error(&Error::Approval("already executed".into())).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            engine_error(&Error::LimitExceeded("slow down".into())).status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn an_unauthorised_response_advertises_bearer() {
        let r = auth_response(&AuthError::Missing);
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(r.headers().get(header::WWW_AUTHENTICATE).unwrap(), "Bearer");
    }

    #[test]
    fn a_missing_role_is_forbidden_rather_than_unauthorised() {
        let r = auth_response(&AuthError::NoRole("nope".into()));
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(r.headers().get(header::WWW_AUTHENTICATE).is_none());
    }

    #[tokio::test]
    async fn api_key_attributes_reach_the_principal() {
        let auth = ApiKeyAuthenticator::new(vec![ApiKey {
            hash: ApiKeyAuthenticator::hash("k"),
            role: "support_eu".into(),
            caller: "job".into(),
            attributes: BTreeMap::from([("region".to_string(), Value::Text("EU".into()))]),
        }]);
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer k".parse().unwrap());
        let p = auth.authenticate(&headers).await.unwrap();
        assert_eq!(p.attributes.get("region"), Some(&Value::Text("EU".into())));
    }
}
